#![deny(warnings)]

mod env;
mod error;
mod extension;
mod fs;
mod global;
mod local;
mod pipe_channel;
mod proc;
mod program;
mod shell;
mod shlex;
mod sys;
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
#[cfg(unix)]
use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
pub use shell::Exit;
use tokio::io::AsyncWrite;

use crate::global::Global;

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
    global.local.get(strand).cwd().as_ref().into()
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
    if let Some(path) = global.types.path.downcast(value) {
        Some(path.annex().inner.clone())
    } else {
        value.as_str(vm).map(|s| PathBuf::from(s.to_string()))
    }
}

pub fn path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: PathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    fs::path::create_path(strand, global, path, out)
}

/// Open file; container-aware
pub async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &path::Path,
    mode: &str,
) -> io::Result<tokio::fs::File> {
    match mode {
        "r" | "w" | "a" | "r+" | "w+" | "a+" => {}
        _ => return Err(io::Error::other(format!("invalid mode: {}", mode))),
    }
    let global = strand.state::<Global<'v>>();
    fs::file::open(strand, global, path, mode).await
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

/// Redirect terminal output (echo/print and default child stderr)
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

    // Restore
    let global = strand.state::<Global<'v>>();
    {
        let mut guard = global.terminal.writer.lock().await;
        *guard = original;
    }
    global.terminal.redirected.set(false);
    #[cfg(unix)]
    drop(echo_guard);

    result
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
