#![deny(warnings)]

mod diagnostic;
mod env;
mod error;
mod error_code;
mod extension;
mod fs;
mod global;
mod local;
mod pipe_channel;
mod platform;
mod proc;
mod program;
mod security;
mod shell;
mod shlex;
mod syntax;
mod sys;
mod term;
mod time;
mod util;

use std::{
    io::{self, IsTerminal},
    path::{self, PathBuf},
    pin::Pin,
};

#[cfg(unix)]
use std::{io::stderr, os::fd::AsFd};

pub use crate::global::ProgramSource;
use dolang::runtime::{Error, Output, Result, Strand, Value, Vm};
#[cfg(unix)]
use dolang_shell_vfs::Client;
pub use dolang_shell_vfs::FileHandle;
#[cfg(unix)]
use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
pub use shell::Exit;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::global::Global;

pub use diagnostic::{print_compile_diag_stderr, print_error_stderr, render_message_backtrace};
#[doc(hidden)]
pub use syntax::{SemanticToken, highlight_range as highlight_source_range};

/// Instantiate wrapper iterator around stdin
pub fn stdin<'v, 's>(strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    global.types.stdin.create(strand, shell::Stdin::new(), out)
}

/// Instantiate wrapper sink around stdout
pub fn stdout<'v, 's>(strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    global
        .types
        .stdout
        .create(strand, shell::Stdout::new(), out)
}

/// Flush the shell's stdout sink and terminal stderr writer.
///
/// `stdout` must be the value installed as the shell's output sink so the
/// precise Tokio handle used for output is flushed before VM shutdown.
pub async fn flush<'v, 's>(strand: &mut Strand<'v, 's>, stdout: &Value<'v>) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let stdout = global
        .types
        .stdout
        .downcast(stdout)
        .ok_or_else(|| Error::type_error(strand, "stdout sink: expected shell.Stdout"))?;
    stdout
        .borrow_mut(strand)?
        .flush()
        .await
        .map_err(|error| Error::runtime(strand, error))?;
    global
        .terminal
        .writer
        .lock()
        .await
        .flush()
        .await
        .map_err(|error| Error::runtime(strand, error))
}

pub fn as_datetime<'v>(vm: &Vm<'v>, value: &Value<'v>) -> Option<std::time::SystemTime> {
    let global = vm.state::<Global<'v>>();
    let datetime = global.types.date_time.downcast(value)?;
    datetime.annex().to_system_time().ok()
}

pub fn datetime<'v>(
    strand: &mut Strand<'v, '_>,
    time: std::time::SystemTime,
    out: impl Output<'v>,
) -> io::Result<()> {
    let global = strand.state::<Global<'v>>();
    let annex = time::DateTimeAnnex::from_system_time(time)?;
    global
        .types
        .date_time
        .create_with_annex(strand, time::DateTime, annex, out);
    Ok(())
}

/// Get current working directory of strand
pub fn cwd<'v>(strand: &Strand<'v, '_>) -> PathBuf {
    let global = strand.state::<Global<'v>>();
    dolang_shell_vfs::native_path(global.local.get(strand).cwd().to_path())
        .expect("local working directory has the host path style")
}

/// Set arguments for `shell.args` object
pub async fn set_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let mut stored = global.args.borrow_mut();
    stored.clear();
    stored.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    Ok(())
}

/// Set source program for `shell.program`.
pub async fn set_program<'v, 's>(
    strand: &mut Strand<'v, 's>,
    program: Option<impl Into<ProgramSource>>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    *global.program.borrow_mut() = program.map(Into::into);
    Ok(())
}

pub fn as_path<'v, 'a>(vm: &Vm<'v>, value: &'a Value<'v>) -> Option<PathBuf> {
    let global = vm.state::<Global<'v>>();
    if let Some(path) = global.types.unix_path.downcast(value) {
        dolang_shell_vfs::native_path(path.annex().inner.to_path()).ok()
    } else if let Some(path) = global.types.windows_path.downcast(value) {
        dolang_shell_vfs::native_path(path.annex().typed_path_buf().to_path()).ok()
    } else {
        value.as_str(vm).map(|s| PathBuf::from(s.to_string()))
    }
}

/// Downcast a Do value to a Unix path.
pub fn as_unix_path<'v>(
    vm: &Vm<'v>,
    value: &Value<'v>,
) -> Option<dolang_shell_vfs::Utf8UnixPathBuf> {
    let global = vm.state::<Global<'v>>();
    let path = global.types.unix_path.downcast(value)?;
    let annex = path.annex();
    match &annex.inner {
        dolang_shell_vfs::Utf8TypedPathBuf::Unix(path) => Some(path.clone()),
        dolang_shell_vfs::Utf8TypedPathBuf::Windows(_) => None,
    }
}

/// Construct a Do `fs.UnixPath` value.
pub fn unix_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: impl AsRef<str>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    fs::path::create_path(
        strand,
        global,
        dolang_shell_vfs::Utf8TypedPathBuf::from_unix(path.as_ref()),
        out,
    )
}

pub fn path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: PathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let path = dolang_shell_vfs::typed_path(path).map_err(|e| Error::runtime(strand, e))?;
    fs::path::create_path(strand, global, path, out)
}

/// Open file; container-aware
pub async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &path::Path,
    mode: &str,
) -> io::Result<dolang_shell_vfs::AnyFile> {
    match mode {
        "r" | "w" | "a" | "r+" | "w+" | "a+" => {}
        _ => return Err(io::Error::other(format!("invalid mode: {}", mode))),
    }
    let global = strand.state::<Global<'v>>();
    fs::file::open_native(
        strand,
        global,
        dolang_shell_vfs::typed_path(path.to_owned())?.to_path(),
        mode,
    )
    .await
}

#[cfg(unix)]
pub fn vfs<'v, 's, 'a>(strand: &'a Strand<'v, 's>) -> Option<Client> {
    let global = strand.state::<Global<'v>>();
    let local = global.local.get(strand);
    local.vfs().into_client()
}

/// Returns whether stderr is a terminal.
pub fn is_terminal() -> bool {
    std::io::stderr().is_terminal()
}

/// Redirect terminal output (`term.echo`/`term.print` and default child stderr)
/// through the provided writer for the duration of the callback.
///
/// Only one redirect may be active per VM. Returns an error if stderr
/// is not a terminal or if a redirect is already active.
pub async fn with_terminal<'v, 's>(
    strand: &mut Strand<'v, 's>,
    writer: Pin<Box<dyn AsyncWrite>>,
    f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();

    if !is_terminal() {
        return Err(Error::runtime(strand, "stderr is not a terminal"));
    }
    if global.terminal.redirected.get() {
        return Err(Error::runtime(strand, "terminal already redirected"));
    }
    global.terminal.redirected.set(true);

    #[cfg(unix)]
    let echo_guard = match TerminalEchoGuard::disable() {
        Ok(guard) => guard,
        Err(e) => {
            global.terminal.redirected.set(false);
            return Err(Error::runtime(
                strand,
                format!("failed to disable terminal echo: {e}"),
            ));
        }
    };

    // Swap writer
    let original = {
        let mut guard = global.terminal.writer.lock().await;
        std::mem::replace(&mut *guard, writer)
    };

    let result = f(strand).await;

    // Flush the temporary writer before restoring the original terminal
    // destination. This is particularly important for progress writers,
    // which buffer partial lines.
    let flush_result = {
        let global = strand.state::<Global<'v>>();
        global
            .terminal
            .writer
            .lock()
            .await
            .flush()
            .await
            .map_err(|error| Error::runtime(strand, error))
    };

    // Restore
    let global = strand.state::<Global<'v>>();
    {
        let mut guard = global.terminal.writer.lock().await;
        *guard = original;
    }
    global.terminal.redirected.set(false);
    #[cfg(unix)]
    drop(echo_guard);

    result.and(flush_result)
}

#[cfg(unix)]
struct TerminalEchoGuard {
    termios: nix::sys::termios::Termios,
}

#[cfg(unix)]
impl TerminalEchoGuard {
    fn disable() -> io::Result<Self> {
        let stderr = stderr();
        let fd = stderr.as_fd();
        let mut termios = tcgetattr(fd).map_err(io::Error::other)?;
        let original = termios.clone();
        termios.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(fd, SetArg::TCSANOW, &termios).map_err(io::Error::other)?;
        Ok(Self { termios: original })
    }
}

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        let stderr = stderr();
        let _ = tcsetattr(stderr.as_fd(), SetArg::TCSANOW, &self.termios);
    }
}

impl From<PathBuf> for ProgramSource {
    fn from(value: PathBuf) -> Self {
        Self::Path(value)
    }
}

impl From<String> for ProgramSource {
    fn from(value: String) -> Self {
        Self::Module(value)
    }
}
