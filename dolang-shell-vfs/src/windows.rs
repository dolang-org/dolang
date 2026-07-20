use std::{
    cell::Cell,
    ffi::{OsStr, OsString},
    io, mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    time::Duration,
};

use rand::{RngExt, distr::Alphanumeric};
use tokio::{
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    sync::oneshot,
    time,
};
use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize},
        Threading::{INFINITE, TerminateProcess, WaitForSingleObject},
    },
    UI::{
        Shell::{
            SEE_MASK_NO_CONSOLE, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
            ShellExecuteExW,
        },
        WindowsAndMessaging::SW_HIDE,
    },
};

use crate::{Client, Query};

const EXIT_STARTUP_FAILURE: u32 = 1;

/// An elevated Windows VFS session.
pub struct WindowsSession {
    client: Client,
    process: OwnedHandle,
    stopped: Cell<bool>,
}

impl WindowsSession {
    /// Launches an elevated copy of the current executable and connects to it.
    pub async fn launch(cwd: impl Into<PathBuf>) -> io::Result<(Self, Query)> {
        launch_with(cwd.into(), launch_elevated, false).await
    }

    /// Launches an elevated session using opaque-only generic framing.
    #[doc(hidden)]
    pub async fn launch_remote(cwd: impl Into<PathBuf>) -> io::Result<(Self, Query)> {
        launch_with(cwd.into(), launch_elevated, true).await
    }

    /// Launches a non-elevated copy of the current executable for automated tests.
    #[doc(hidden)]
    pub async fn launch_unelevated(cwd: impl Into<PathBuf>) -> io::Result<(Self, Query)> {
        launch_with(cwd.into(), launch_process, false).await
    }

    /// Launches a non-elevated session using opaque-only generic framing.
    #[doc(hidden)]
    pub async fn launch_unelevated_remote(cwd: impl Into<PathBuf>) -> io::Result<(Self, Query)> {
        launch_with(cwd.into(), launch_process, true).await
    }

    /// Returns the VFS RPC client for this session.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Stops the VFS server and waits for the elevated process to exit.
    pub async fn stop(&self) -> io::Result<()> {
        let should_stop = !self.stopped.replace(true);
        let mut guard = should_stop.then(|| ProcessGuard::new(&self.process));
        let stop_result = if should_stop {
            self.client
                .stop()
                .await
                .map_err(crate::Error::into_io_error)
        } else {
            Ok(())
        };
        if stop_result.is_err() {
            drop(guard.take());
        }
        let wait_result = wait_for_exit(&self.process).await;
        if wait_result.is_ok()
            && let Some(guard) = guard.take()
        {
            guard.disarm();
        }
        stop_result.and(wait_result)
    }
}

async fn launch_with(
    cwd: PathBuf,
    launcher: impl FnOnce(&Path, &OsStr, &Path) -> io::Result<OwnedHandle> + Send + 'static,
    remote: bool,
) -> io::Result<(WindowsSession, Query)> {
    let pipe_name = random_pipe_name();
    let pipe = create_pipe(&pipe_name)?;
    let executable = std::env::current_exe()?;
    let process = launch_on_sta_thread(executable, pipe_name, cwd, launcher).await?;
    let guard = ProcessGuard::new(&process);

    connect_or_exit(&pipe, &process).await?;
    let client = if remote {
        Client::new(pipe)
    } else {
        let client_process = process.as_handle().try_clone_to_owned()?;
        unsafe { Client::from_named_pipe_server(pipe, client_process) }
            .map_err(crate::Error::into_io_error)?
    };
    let query = client.query().await.map_err(crate::Error::into_io_error)?;
    guard.disarm();

    Ok((
        WindowsSession {
            client,
            process,
            stopped: Cell::new(false),
        },
        query,
    ))
}

async fn launch_on_sta_thread(
    executable: impl Into<std::path::PathBuf>,
    pipe_name: OsString,
    cwd: PathBuf,
    launcher: impl FnOnce(&Path, &OsStr, &Path) -> io::Result<OwnedHandle> + Send + 'static,
) -> io::Result<OwnedHandle> {
    let executable = executable.into();
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("dolang-vfs-launch".into())
        .spawn(move || {
            let result = with_sta_com(|| launcher(&executable, &pipe_name, &cwd));
            let _ = tx.send(result);
        })?;
    rx.await
        .map_err(|_| io::Error::other("elevation thread exited without returning a result"))?
}

fn with_sta_com<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let result = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::other(format!(
            "failed to initialize COM for elevation: HRESULT 0x{:08x}",
            result as u32
        )));
    }

    struct Uninitialize;
    impl Drop for Uninitialize {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _uninitialize = Uninitialize;
    f()
}

impl Drop for WindowsSession {
    fn drop(&mut self) {
        if !self.stopped.get() && !has_exited(&self.process) {
            terminate(&self.process);
        }
    }
}

fn random_pipe_name() -> OsString {
    let mut rng = rand::rng();
    let suffix: String = (0..32)
        .map(|_| rng.sample(Alphanumeric))
        .map(char::from)
        .collect();
    format!(r"\\.\pipe\dolang-vfs-{}-{suffix}", std::process::id()).into()
}

fn create_pipe(name: &OsStr) -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(name)
}

fn launch_elevated(executable: &Path, pipe_name: &OsStr, cwd: &Path) -> io::Result<OwnedHandle> {
    let verb = wide_null(OsStr::new("runas"));
    let executable = wide_null(executable.as_os_str());
    let parameters = command_line([OsStr::new("--vfs"), OsStr::new("--connect"), pipe_name]);
    let cwd = wide_null(cwd.as_os_str());
    let mut info: SHELLEXECUTEINFOW = unsafe { mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_NO_CONSOLE;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.lpDirectory = cwd.as_ptr();
    info.nShow = SW_HIDE;

    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows elevation was cancelled",
            ));
        }
        return Err(error);
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other(
            "elevated process did not return a process handle",
        ));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess as _) })
}

fn launch_process(executable: &Path, pipe_name: &OsStr, cwd: &Path) -> io::Result<OwnedHandle> {
    let child = std::process::Command::new(executable)
        .arg("--vfs")
        .arg("--connect")
        .arg(pipe_name)
        .current_dir(cwd)
        .spawn()?;
    child.as_handle().try_clone_to_owned()
}

async fn connect_or_exit(pipe: &NamedPipeServer, process: &OwnedHandle) -> io::Result<()> {
    let mut poll = time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            result = pipe.connect() => return result,
            _ = poll.tick() => {
                if has_exited(process) {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "elevated VFS process exited before connecting",
                    ));
                }
            }
        }
    }
}

async fn wait_for_exit(process: &OwnedHandle) -> io::Result<()> {
    let process = process.as_handle().try_clone_to_owned()?;
    tokio::task::spawn_blocking(move || {
        let result = unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, INFINITE) };
        if result == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
    .await
    .map_err(io::Error::other)?
}

fn has_exited(process: &OwnedHandle) -> bool {
    (unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, 0) }) != WAIT_TIMEOUT
}

fn terminate(process: &OwnedHandle) {
    unsafe {
        TerminateProcess(process.as_raw_handle() as HANDLE, EXIT_STARTUP_FAILURE);
    }
}

struct ProcessGuard<'a> {
    process: &'a OwnedHandle,
    armed: bool,
}

impl<'a> ProcessGuard<'a> {
    fn new(process: &'a OwnedHandle) -> Self {
        Self {
            process,
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGuard<'_> {
    fn drop(&mut self) {
        if self.armed && !has_exited(self.process) {
            terminate(self.process);
        }
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn command_line<'a>(args: impl IntoIterator<Item = &'a OsStr>) -> Vec<u16> {
    let mut result = Vec::new();
    for argument in args {
        if !result.is_empty() {
            result.push(b' ' as u16);
        }
        quote_argument(argument, &mut result);
    }
    result.push(0);
    result
}

fn quote_argument(argument: &OsStr, output: &mut Vec<u16>) {
    let argument: Vec<_> = argument.encode_wide().collect();
    let quote = argument.is_empty()
        || argument
            .iter()
            .any(|c| *c == b' ' as u16 || *c == b'\t' as u16 || *c == b'"' as u16);
    if !quote {
        output.extend(argument);
        return;
    }

    output.push(b'"' as u16);
    let mut backslashes = 0;
    for c in argument {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else if c == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            output.push(c);
            backslashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            output.push(c);
            backslashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    output.push(b'"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display_command_line(args: &[&str]) -> String {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        let mut encoded = command_line(args);
        encoded.pop();
        String::from_utf16(&encoded).unwrap()
    }

    #[test]
    fn pipe_names_use_random_suffixes() {
        let first = random_pipe_name();
        let second = random_pipe_name();
        let first = first.to_string_lossy();
        assert!(first.starts_with(r"\\.\pipe\dolang-vfs-"));
        assert_ne!(first, second.to_string_lossy());
        assert!(
            first
                .rsplit('-')
                .next()
                .unwrap()
                .chars()
                .all(char::is_alphanumeric)
        );
        assert_eq!(first.rsplit('-').next().unwrap().len(), 32);
    }

    #[test]
    fn quotes_windows_arguments() {
        assert_eq!(
            display_command_line(&["--vfs", "--connect", r"\\.\pipe\plain"]),
            r"--vfs --connect \\.\pipe\plain"
        );
        assert_eq!(
            display_command_line(&["", "two words"]),
            r#""" "two words""#
        );
        assert_eq!(display_command_line(&[r#"a\"b"#]), r#""a\\\"b""#);
        assert_eq!(
            display_command_line(&[r"C:\path with space\"]),
            "\"C:\\path with space\\\\\""
        );
    }

    #[tokio::test]
    async fn launcher_errors_are_reported_without_uac() {
        let error = match launch_with(
            std::env::current_dir().unwrap(),
            |_, _, _| Err(io::Error::other("launch failed")),
            false,
        )
        .await
        {
            Ok(_) => panic!("launcher unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "launch failed");
    }
}
