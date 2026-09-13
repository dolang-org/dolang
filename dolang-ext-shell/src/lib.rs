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
mod util;

use std::{
    io,
    path::{self, PathBuf},
    pin::Pin,
};

#[cfg(unix)]
use std::{io::stderr, os::fd::AsFd};

pub use crate::{
    error::{ErrorExt, ResultExt},
    extension::Shell,
    global::ProgramSource,
    security::AccessMask as WindowsAccessMask,
};
use dolang::runtime::{Error, Output, Result, Slot, Strand, Value};
pub use dolang_ext_term::{
    ansi_enabled, terminal_line_ending, terminal_output, write_terminal_line,
};
pub use dolang_vfs::Vfs;
#[cfg(unix)]
use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
pub use shell::{Exec, Exit};
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::global::Global;

pub use diagnostic::{print_compile_diag_stderr, print_error_stderr, render_message_backtrace};
use dolang_vfs::path as vfs_path;
#[doc(hidden)]
pub use syntax::{
    NodeClass, SemanticToken, classify_node, highlight_range as highlight_source_range,
};

/// Instantiate the `shell.stdin` handle.
///
/// The handle is stateless — the buffered reader itself lives on the VM — so
/// this and `shell.stdin` read the same stream and cannot split its buffer.
pub fn stdin<'v, 's>(strand: &mut Strand<'v, 's>, out: impl Output<'v>) {
    let global = strand.state::<Global<'v>>();
    global
        .types
        .stdin
        .create(strand, shell::Stdin::default(), out)
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
        dolang_ext_term::default_output(strand, out)
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

/// Extracts a `security.windows.Sid` runtime value.
pub fn as_windows_sid<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<dolang_winterop::security::Sid> {
    security::as_windows_sid(strand, value)
}

/// Constructs a `security.windows.Sid` runtime value.
pub fn windows_sid<'v>(
    strand: &mut Strand<'v, '_>,
    sid: dolang_winterop::security::Sid,
    out: &mut dolang::runtime::Slot<'v, '_>,
) {
    security::windows_sid(strand, sid, out);
}

/// Constructs a `security.windows.SidName` runtime value.
pub fn windows_sid_name<'v>(
    strand: &mut Strand<'v, '_>,
    name: dolang_vfs::security::SidName,
    out: &mut dolang::runtime::Slot<'v, '_>,
) {
    let global = strand.state::<Global<'v>>();
    security::create_sid_name(strand, global, name, out);
}

/// Get current working directory of strand
pub fn cwd<'v>(strand: &Strand<'v, '_>) -> PathBuf {
    let global = strand.state::<Global<'v>>();
    global
        .local
        .get(strand)
        .cwd()
        .to_native()
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
            inst.annex().path_buf().to_native().ok()
        })
    } else if let Some(path) = global.types.windows_path.cast(value) {
        path.enter_sync(strand, |_strand, inst| {
            inst.annex().path_buf().to_native().ok()
        })
    } else {
        value.as_str(strand).map(|s| PathBuf::from(s.to_string()))
    }
}

/// Downcast a Do value to a Unix path.
pub fn as_unix_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<vfs_path::PathBuf> {
    let global = strand.state::<Global<'v>>();
    let path = global.types.unix_path.cast(value)?;
    path.enter_sync(strand, |_strand, inst| {
        let path = &inst.annex().path;
        (path.kind() == vfs_path::Kind::Unix).then(|| path.clone())
    })
}

/// Downcast a Do value to a Windows path.
pub fn as_windows_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Option<vfs_path::PathBuf> {
    let global = strand.state::<Global<'v>>();
    let path = global.types.windows_path.cast(value)?;
    path.enter_sync(strand, |_strand, inst| {
        let path = &inst.annex().path;
        (path.kind() == vfs_path::Kind::Windows).then(|| path.clone())
    })
}

/// Construct a Do `fs.unix.Path` value.
pub fn unix_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: impl AsRef<str>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    fs::path::create_path(
        strand,
        global,
        vfs_path::PathBuf::from_unix(path.as_ref()),
        out,
    )
}

/// Construct a Do `fs.windows.Path` value.
pub fn windows_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: impl AsRef<str>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    fs::path::create_path(
        strand,
        global,
        vfs_path::PathBuf::from_windows(path.as_ref()),
        out,
    )
}

pub fn path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: PathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let global = strand.state::<Global<'v>>();
    let path = vfs_path::PathBuf::from_native(path).map_err(|e| Error::runtime(strand, e))?;
    fs::path::create_path(strand, global, path, out)
}

/// Open file; container-aware
pub async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: &path::Path,
    mode: &str,
) -> io::Result<dolang_vfs::file::File> {
    match mode {
        "r" | "w" | "a" | "r+" | "w+" | "a+" => {}
        _ => return Err(io::Error::other(format!("invalid mode: {}", mode))),
    }
    let global = strand.state::<Global<'v>>();
    fs::file::open_native(
        strand,
        global,
        vfs_path::PathBuf::from_native(path.to_owned())?.to_path(),
        mode,
    )
    .await
}

/// Construct a Do `security.windows.SecDesc` value from a raw
/// [`dolang_winterop::security::SecDesc`].
///
/// Exposed so sibling extensions (e.g. `dolang-ext-winreg`) can produce the
/// same `SecDesc` Do type `fs.Path.sec_desc()` does, without needing
/// `security`'s internals to be `pub`.
pub fn create_sec_desc<'v>(
    strand: &mut Strand<'v, '_>,
    sec_desc: dolang_winterop::security::SecDesc,
    out: impl Output<'v>,
) {
    let global = strand.state::<Global<'v>>();
    global
        .types
        .sec_desc
        .create_with_annex(strand, security::SecDesc, sec_desc, out);
}

/// Read a [`dolang_winterop::SecDesc`] from a `update_sec_desc`-style call's
/// own arguments.
///
/// Accepts a positional descriptor — a `security.windows.SecDesc`, a
/// self-relative packet, or a declarative spec — and the descriptor's
/// component options as keyword arguments, which amend a positional
/// descriptor when both are given. `name` roots the paths in error
/// messages, and should be the method's own name.
pub async fn sec_desc_from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: dolang::runtime::Args<'v, '_>,
    name: &str,
) -> Result<'v, 's, dolang_winterop::security::SecDesc> {
    let global = strand.state::<Global<'v>>();
    security::sec_desc_from_args(strand, global, args, &security::SpecPath::root(name)).await
}

/// Coerce a descriptor value accepted by the Windows security APIs.
pub async fn sec_desc_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &dolang::runtime::Value<'v>,
    name: &str,
) -> Result<'v, 's, dolang_winterop::security::SecDesc> {
    let global = strand.state::<Global<'v>>();
    security::sec_desc_from_value(strand, global, value, name).await
}

/// The `security.windows.AccessMask` type object.
///
/// A domain-specific access mask — registry key rights, service rights —
/// registers this as a nominal supertype so it can be used wherever a
/// Windows access mask is expected. A type that does so must expose an `int`
/// field yielding the raw 32-bit rights: that field is how the security APIs
/// read a subtype's bits.
///
/// The extension registering the subtype must name [`Shell`] in its
/// `DEPENDS`, or this type may not exist yet.
pub fn windows_access_mask_type<'v>(
    builder: &dolang::runtime::vm::Builder<'v>,
) -> dolang::runtime::Type<'v, dolang::runtime::object::Flags<WindowsAccessMask>> {
    builder.state::<Global<'v>>().types.access_mask
}

/// Returns the [`AnyVfs`] in scope for the strand (the ambient
/// filesystem/registry/etc. backend — direct or remote — for the current
/// shell/session/container context).
pub fn vfs<'v, 's, 'a>(strand: &'a Strand<'v, 's>) -> Vfs {
    let global = strand.state::<Global<'v>>();
    let local = global.local.get(strand);
    local.vfs()
}

/// Returns whether stderr is a terminal — the same override-aware answer the
/// host console's styling policy falls back on, so `DOLANG_CONSOLE=tty=...`
/// also governs whether an extension can take over the terminal
/// ([`with_terminal`]) or render an interactive display
/// (`dolang-ext-progress`'s indicatif vs. plain choice).
///
/// Unlike `term.console.is_tty`, this always answers about stderr itself: it
/// ignores installed capture consoles, and stays true while an extension has
/// taken the terminal over.
pub fn stderr_is_tty<'v>(strand: &Strand<'v, '_>) -> bool {
    strand.state::<Global<'v>>().terminal.stderr_is_terminal
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

/// Formats an error value and backtrace as a `term.Text`, the way an uncaught
/// error is reported.
///
/// `backtrace` defaults to that of the active handled exception.
pub fn render_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: &Value<'v>,
    backtrace: Option<&Value<'v>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let rendered = diagnostic::render_error_value(strand, error, backtrace)?;
    dolang_ext_term::preformatted_text(strand, &rendered, out)
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
