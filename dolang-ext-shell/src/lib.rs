#![deny(warnings)]

mod console;
mod diagnostic;
mod env;
mod error;
mod error_code;
mod extension;
mod fs;
mod geometry;
mod global;
mod io_mode;
mod local;
mod pipe_channel;
mod platform;
mod proc;
mod program;
mod security;
mod shell;
mod shell_args;
mod shlex;
mod syntax;
mod sys;
mod term;
mod time;
mod util;

use std::{
    io,
    path::{self, PathBuf},
    pin::Pin,
};

#[cfg(unix)]
use std::{io::stderr, os::fd::AsFd};

pub use crate::error::{ErrorExt, ResultExt};
pub use crate::global::ProgramSource;
use dolang::runtime::{Error, Output, Result, Strand, Value};
pub use dolang_vfs::{AnyVfs, FileHandle};
#[cfg(unix)]
use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
pub use shell::Exit;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::global::Global;

pub use diagnostic::{print_compile_diag_stderr, print_error_stderr, render_message_backtrace};
#[doc(hidden)]
pub use syntax::{SemanticToken, highlight_range as highlight_source_range};

/// Instantiate the `shell.stdin` handle.
///
/// The handle is stateless — the buffered reader itself lives on the VM — so
/// this and `shell.stdin` read the same stream and cannot split its buffer.
pub fn stdin<'v, 's>(strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    global.types.stdin.create(strand, shell::Stdin, out)
}

/// Instantiate the `shell.stdout` handle.
///
/// Stateless, as with [`stdin`].
pub fn stdout<'v, 's>(strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    global.types.stdout.create(strand, shell::Stdout, out)
}

/// Instantiate the strand's default output handle.
///
/// `term.default` when stdout is a terminal, so unnamed program output keeps
/// following capture and progress takeover for the life of the process; the
/// literal `shell.stdout` otherwise, since there is nothing to follow and raw
/// fd inheritance is the cheaper, simpler path.
pub fn default_output<'v, 's>(strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    if global.terminal.stdout_is_terminal {
        global
            .types
            .default
            .create(strand, console::DefaultOutput, out)
    } else {
        global.types.stdout.create(strand, shell::Stdout, out)
    }
}

/// Flush the process's standard streams and the console writer.
///
/// Tokio stdio handles can retain buffered output when the runtime shuts down,
/// so this must run while the runtime is still alive.
pub async fn flush<'v, 's>(strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    global
        .stdio
        .stdout
        .lock()
        .await
        .flush()
        .await
        .map_err(|error| Error::runtime(strand, error))?;
    global
        .stdio
        .stderr
        .lock()
        .await
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

pub fn as_datetime<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<std::time::SystemTime> {
    let global = strand.state::<Global<'v>>();
    let datetime = global.types.date_time.cast(value)?;
    datetime.enter_sync(strand, |_strand, inst| inst.annex().to_system_time().ok())
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
    dolang_vfs::native_path(global.local.get(strand).cwd().to_path())
        .expect("local working directory has the host path style")
}

/// Set arguments for `shell.args` object
pub async fn set_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    *global.args.borrow_mut() = args
        .into_iter()
        .map(|arg| Box::<str>::from(arg.as_ref()))
        .collect::<Vec<_>>()
        .into();
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

pub fn as_path<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Option<PathBuf> {
    let global = strand.state::<Global<'v>>();
    if let Some(path) = global.types.unix_path.cast(value) {
        path.enter_sync(strand, |_strand, inst| {
            dolang_vfs::native_path(inst.annex().inner.to_path()).ok()
        })
    } else if let Some(path) = global.types.windows_path.cast(value) {
        path.enter_sync(strand, |_strand, inst| {
            dolang_vfs::native_path(inst.annex().typed_path_buf().to_path()).ok()
        })
    } else {
        value.as_str(strand).map(|s| PathBuf::from(s.to_string()))
    }
}

/// Downcast a Do value to a Unix path.
pub fn as_unix_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<dolang_vfs::Utf8UnixPathBuf> {
    let global = strand.state::<Global<'v>>();
    let path = global.types.unix_path.cast(value)?;
    path.enter_sync(strand, |_strand, inst| {
        let annex = inst.annex();
        match &annex.inner {
            dolang_vfs::Utf8TypedPathBuf::Unix(path) => Some(path.clone()),
            dolang_vfs::Utf8TypedPathBuf::Windows(_) => None,
        }
    })
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
        dolang_vfs::Utf8TypedPathBuf::from_unix(path.as_ref()),
        out,
    )
}

pub fn path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: PathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let path = dolang_vfs::typed_path(path).map_err(|e| Error::runtime(strand, e))?;
    fs::path::create_path(strand, global, path, out)
}

/// Open file; container-aware
pub async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &path::Path,
    mode: &str,
) -> io::Result<dolang_vfs::AnyFile> {
    match mode {
        "r" | "w" | "a" | "r+" | "w+" | "a+" => {}
        _ => return Err(io::Error::other(format!("invalid mode: {}", mode))),
    }
    let global = strand.state::<Global<'v>>();
    fs::file::open_native(
        strand,
        global,
        dolang_vfs::typed_path(path.to_owned())?.to_path(),
        mode,
    )
    .await
}

/// Construct a Do `security.windows.SecDesc` value from a raw
/// [`dolang_winterop::SecDesc`].
///
/// Exposed so sibling extensions (e.g. `dolang-ext-winreg`) can produce the
/// same `SecDesc` Do type `fs.Path.sec_desc()` does, without needing
/// `security`'s internals to be `pub`.
pub fn create_sec_desc<'v>(
    strand: &mut Strand<'v, '_>,
    sec_desc: dolang_winterop::SecDesc,
    out: impl Output<'v>,
) {
    let global = strand.state::<Global<'v>>();
    global
        .types
        .sec_desc
        .create_with_annex(strand, security::SecDesc, sec_desc, out);
}

/// Extract the raw [`dolang_winterop::SecDesc`] from a Do
/// `security.windows.SecDesc` value.
pub fn sec_desc_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, dolang_winterop::SecDesc> {
    let global = strand.state::<Global<'v>>();
    security::sec_desc_from_value(strand, global, value)
}

/// Returns the [`AnyVfs`] in scope for the strand (the ambient
/// filesystem/registry/etc. backend — direct or remote — for the current
/// shell/session/container context).
pub fn vfs<'v, 's, 'a>(strand: &'a Strand<'v, 's>) -> AnyVfs {
    let global = strand.state::<Global<'v>>();
    let local = global.local.get(strand);
    local.vfs()
}

/// Returns whether stderr is a terminal — the same override-aware answer
/// `term.console.is_tty` and `console::ansi`'s tty-detection fallback use, so
/// `DOLANG_CONSOLE=tty=...` also governs whether an extension can take over
/// the terminal ([`with_terminal`]) or render an interactive display
/// (`dolang-ext-progress`'s indicatif vs. plain choice).
///
/// Capture-blind: unlike [`crate::console::is_tty`], this always answers
/// about stderr itself, never an installed capture console.
pub fn stderr_is_tty<'v>(strand: &Strand<'v, '_>) -> bool {
    strand.state::<Global<'v>>().terminal.stderr_is_terminal
}

/// Whether ANSI styling should be emitted to stderr, per the same
/// NO_COLOR/FORCE_COLOR/tty policy `term.echo`/`term.print` use.
pub fn ansi_enabled<'v>(strand: &Strand<'v, '_>) -> bool {
    crate::console::ansi(strand)
}

/// Stderr's terminal width in columns, the same override-aware answer
/// `term.console.geometry().cols` gives: `DOLANG_CONSOLE=cols=...` wins if
/// set, otherwise the real terminal is queried. `None` if stderr isn't a
/// terminal (or the terminal declines to report its size) and no override
/// pins the column count down.
pub fn stderr_cols<'v>(strand: &Strand<'v, '_>) -> Option<u16> {
    let global = strand.state::<Global<'v>>();
    let ov = &global.terminal.console_override;
    ov.cols
        .or_else(|| ::console::Term::stderr().size_checked().map(|(_, c)| c))
}

/// Write a line (newline appended) through the shared terminal writer,
/// serialized with `term.echo`/`term.print`/diagnostic output.
///
/// Unlike [`with_terminal`], this does not require stderr to be a terminal
/// or take exclusive ownership of the writer — it just locks the same mutex
/// used by every other terminal writer, so callers may use it freely
/// alongside concurrent `echo`/`print` calls from other strands.
pub async fn write_terminal_line<'v, 's>(
    strand: &mut Strand<'v, 's>,
    line: &str,
) -> Result<'v, 's, ()> {
    crate::console::writeln(strand, line.as_bytes()).await
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

    if !stderr_is_tty(strand) {
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
