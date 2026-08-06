use std::{
    ffi::{OsStr, OsString},
    io,
    path::PathBuf,
};

#[cfg(unix)]
use std::{os::unix::fs::PermissionsExt, path::Path};
use tokio::runtime::Builder;

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::Server;

enum EnvOp {
    Set(OsString, OsString),
    Unset(OsString),
}

/// Resolve an option's value, decoding it if the `-base64` form was used.
///
/// Options that carry a caller-supplied value all have a `-base64` variant
/// because the helper is typically launched over SSH, which joins its command
/// with spaces and lets the remote account's shell split it again. That shell
/// is a POSIX shell on a Unix host and `cmd.exe` on a Windows one, and which
/// answers is not knowable when the command is built, so no quoting is
/// portable. The base64 alphabet needs none: it survives both untouched, and
/// carries bytes that are not valid UTF-8 into the bargain.
fn option_value(option: &str, arg: &OsStr) -> io::Result<OsString> {
    if !option.ends_with("-base64") {
        return Ok(arg.to_owned());
    }

    let bytes = STANDARD.decode(arg.as_encoded_bytes()).map_err(|error| {
        io::Error::other(format!("{option} value is not valid base64: {error}"))
    })?;

    decoded_string(bytes)
        .ok_or_else(|| io::Error::other(format!("{option} value is not a valid string")))
}

#[cfg(unix)]
fn decoded_string(bytes: Vec<u8>) -> Option<OsString> {
    Some(std::os::unix::ffi::OsStringExt::from_vec(bytes))
}

/// Elsewhere there is no way to spell an arbitrary byte string, so the decoded
/// value has to be valid UTF-8.
#[cfg(not(unix))]
fn decoded_string(bytes: Vec<u8>) -> Option<OsString> {
    String::from_utf8(bytes).ok().map(OsString::from)
}

/// Parse the shell out of `--login-env=<shell>` or its `-base64` form.
///
/// Returns `None` for any other argument.
fn login_env_shell(arg: &OsStr) -> Option<io::Result<OsString>> {
    let bytes = arg.as_encoded_bytes();
    let (prefix, rest) = ["--login-env=", "--login-env-base64="]
        .into_iter()
        .find_map(|prefix| Some((prefix, bytes.strip_prefix(prefix.as_bytes())?)))?;

    // Recover the raw argument bytes so a shell path that is not valid UTF-8
    // survives. SAFETY: the prefix is ASCII, so the split lands on a character
    // boundary of the platform encoding.
    let rest = unsafe { OsStr::from_encoded_bytes_unchecked(rest) };

    Some(match option_value(prefix.trim_end_matches('='), rest) {
        Ok(shell) if shell.is_empty() => {
            Err(io::Error::other(format!("{prefix} requires a shell path")))
        }
        result => result,
    })
}

/// Split a `NAME=VALUE` argument on its first `=`.
fn split_assignment(arg: &OsStr) -> Option<(OsString, OsString)> {
    let bytes = arg.as_encoded_bytes();
    let at = bytes.iter().position(|byte| *byte == b'=')?;

    // SAFETY: `=` is ASCII, so it cannot fall inside a multi-byte sequence and
    // both halves remain well-formed in the platform encoding.
    unsafe {
        Some((
            OsStr::from_encoded_bytes_unchecked(&bytes[..at]).to_owned(),
            OsStr::from_encoded_bytes_unchecked(&bytes[at + 1..]).to_owned(),
        ))
    }
}

pub fn main(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<()> {
    let mut env_ops = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    let mut mode: Option<String> = None;
    let mut mode_args: Vec<String> = Vec::new();
    // Outer option: whether --login-env was given at all. Inner: the shell it
    // explicitly named, if any.
    let mut login_env: Option<Option<OsString>> = None;

    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg.as_ref();
        let s = arg.to_string_lossy().into_owned();

        if mode.is_some() {
            mode_args.push(s);
            continue;
        }

        if s == "--stdio" {
            mode = Some(s);
            continue;
        }
        #[cfg(unix)]
        if s == "--listen" {
            mode = Some(s);
            continue;
        }
        #[cfg(windows)]
        if s == "--connect" {
            mode = Some(s);
            continue;
        }
        #[cfg(unix)]
        if s == "--login-env-probe" {
            mode = Some(s);
            continue;
        }
        #[cfg(not(unix))]
        if s == "--listen" {
            return Err(io::Error::other("--listen is only supported on Unix"));
        }
        #[cfg(not(windows))]
        if s == "--connect" {
            return Err(io::Error::other("--connect is only supported on Windows"));
        }

        // Accepted on every platform: a client launching the helper over SSH
        // does not necessarily know which operating system will answer. The
        // shell form is only meaningful on Unix.
        if s == "--login-env" {
            login_env = Some(None);
            continue;
        }
        if let Some(shell) = login_env_shell(arg) {
            login_env = Some(Some(shell?));
            continue;
        }

        if s == "--set" || s == "--set-base64" {
            let Some(next) = args.next() else {
                return Err(io::Error::other(format!(
                    "{s} requires a name=value argument"
                )));
            };
            let val = option_value(&s, next.as_ref())?;
            let (name, value) = split_assignment(&val).ok_or_else(|| {
                io::Error::other(format!("{s} argument must have name=value form"))
            })?;
            if name.is_empty() {
                return Err(io::Error::other(format!(
                    "{s} variable name must not be empty"
                )));
            }
            env_ops.push(EnvOp::Set(name, value));
            continue;
        }
        if s == "--unset" || s == "--unset-base64" {
            let Some(next) = args.next() else {
                return Err(io::Error::other(format!("{s} requires a variable name")));
            };
            let name = option_value(&s, next.as_ref())?;
            if name.is_empty() {
                return Err(io::Error::other(format!(
                    "{s} variable name must not be empty"
                )));
            }
            env_ops.push(EnvOp::Unset(name));
            continue;
        }
        if s == "--cd" || s == "--cd-base64" {
            let Some(next) = args.next() else {
                return Err(io::Error::other(format!("{s} requires a path")));
            };
            cwd = Some(PathBuf::from(option_value(&s, next.as_ref())?));
            continue;
        }

        return Err(io::Error::other(format!("unknown option: {}", s)));
    }

    let mode = mode
        .ok_or_else(|| io::Error::other("missing --stdio, --listen <path>, or --connect <path>"))?;

    // The probe emits a snapshot of the environment we were started with, so
    // it must run before anything modifies it.
    #[cfg(unix)]
    if mode == "--login-env-probe" {
        if !mode_args.is_empty() {
            return Err(io::Error::other("--login-env-probe takes no arguments"));
        }
        return crate::probe::emit();
    }

    // Import the login environment first so that explicit --set and --unset
    // operations take precedence over it.
    #[cfg(any(unix, windows))]
    if let Some(shell) = &login_env {
        // SAFETY: single-threaded, before tokio
        unsafe { crate::probe::import(shell.as_deref())? };
    }
    #[cfg(not(any(unix, windows)))]
    let _ = &login_env;

    // Apply environment and cwd operations before starting tokio.
    // SAFETY: we are single-threaded here and have not yet spawned threads.
    for op in &env_ops {
        match op {
            EnvOp::Set(name, value) => {
                // SAFETY: single-threaded, before tokio
                unsafe { std::env::set_var(name, value) };
            }
            EnvOp::Unset(name) => {
                // SAFETY: single-threaded, before tokio
                unsafe { std::env::remove_var(name) };
            }
        }
    }
    if let Some(path) = &cwd {
        std::env::set_current_dir(path)?;
    }

    let mode = mode.as_str();
    match mode {
        "--stdio" => {
            if !mode_args.is_empty() {
                return Err(io::Error::other("--stdio takes no arguments"));
            }
            serve_stdio()?;
        }
        #[cfg(unix)]
        "--listen" => {
            if mode_args.len() != 1 {
                return Err(io::Error::other("--listen requires exactly one argument"));
            }
            foreground(Path::new(&mode_args[0]))?;
        }
        #[cfg(windows)]
        "--connect" => {
            if mode_args.len() != 1 {
                return Err(io::Error::other("--connect requires exactly one argument"));
            }
            serve_named_pipe(OsStr::new(&mode_args[0]))?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn serve_stdio() -> io::Result<()> {
    // Single-threaded: every used configuration serves a single client, and
    // a multi-thread runtime measurably hurts single-stream pipe throughput
    // (see dolang-rpc's benches/pipe_trailer.rs and benches/pipe_raw.rs).
    Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            #[cfg(unix)]
            let (stdin, stdout) = unix_stdio(io::stdin(), io::stdout())?;
            #[cfg(not(unix))]
            let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());

            let server = Server::new_split(stdin, stdout)
                .await
                .map_err(crate::Error::into_io_error)?;
            #[cfg(windows)]
            {
                tokio::select! {
                    result = server.serve() => result,
                    result = windows_interrupt_signal() => result,
                }
            }
            #[cfg(not(windows))]
            server.serve().await
        })
}

/// Raised kernel pipe buffer size requested for the `--stdio` fast path.
#[cfg(unix)]
const STDIO_PIPE_BUFFER_SIZE: usize = 1024 * 1024;

/// Wraps process stdin/stdout as non-blocking pipes, bypassing tokio's
/// `tokio::io::stdin`/`stdout`, which shuttle every read and write through a
/// dedicated blocking-pool thread since the standard library gives no other
/// way to drive console/tty stdio asynchronously. `--stdio` mode always
/// connects to a parent process speaking the VFS RPC protocol over a pipe —
/// never a console, and never anything a human could usefully type into or
/// capture from a file — so this requires both ends to actually be pipes
/// and drives them directly through `tokio::net::unix::pipe`, which polls
/// the raw fd itself with no intermediate thread. This also lets the
/// outbound side request a larger kernel pipe buffer, which the
/// blocking-pool path has no access to.
#[cfg(unix)]
fn unix_stdio(
    stdin: io::Stdin,
    stdout: io::Stdout,
) -> io::Result<(
    tokio::net::unix::pipe::Receiver,
    tokio::net::unix::pipe::Sender,
)> {
    use std::os::fd::{AsFd, AsRawFd};

    let stdin = tokio::net::unix::pipe::Receiver::from_owned_fd_unchecked(dup_nonblocking_pipe(
        stdin.as_fd(),
    )?)?;
    let stdout = tokio::net::unix::pipe::Sender::from_owned_fd_unchecked(dup_nonblocking_pipe(
        stdout.as_fd(),
    )?)?;
    // The buffer is a property of the pipe itself, shared by both ends, so
    // either side can raise it.
    crate::pipe::set_pipe_buffer_size(stdin.as_raw_fd(), STDIO_PIPE_BUFFER_SIZE);
    crate::pipe::set_pipe_buffer_size(stdout.as_raw_fd(), STDIO_PIPE_BUFFER_SIZE);
    Ok((stdin, stdout))
}

/// Duplicates `fd` and sets the dup non-blocking, failing if `fd` isn't
/// actually a pipe (FIFO). `fcntl(F_SETFL)` applies to the open file
/// description, so this only ever touches the dup — the original fd is left
/// alone, and only the dup is handed to the caller for exclusive async use.
#[cfg(unix)]
fn dup_nonblocking_pipe(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<std::os::fd::OwnedFd> {
    use nix::{
        fcntl::{FcntlArg, OFlag, fcntl},
        sys::stat::{SFlag, fstat, mode_t},
    };

    let stat = fstat(fd)?;
    if SFlag::from_bits_truncate(stat.st_mode as mode_t) & SFlag::S_IFMT != SFlag::S_IFIFO {
        return Err(io::Error::other(
            "--stdio requires stdin/stdout to be pipes",
        ));
    }

    let dup = fd.try_clone_to_owned()?;
    let flags = OFlag::from_bits_truncate(fcntl(&dup, FcntlArg::F_GETFL)?);
    fcntl(&dup, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(dup)
}

#[cfg(windows)]
async fn windows_interrupt_signal() -> io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    let mut ctrl_c = ctrl_c()?;
    let mut ctrl_break = ctrl_break()?;
    tokio::select! {
        _ = ctrl_c.recv() => Ok(()),
        _ = ctrl_break.recv() => Ok(()),
    }
}

#[cfg(unix)]
async fn create_server(socket_path: &Path) -> Result<Server, io::Error> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?;
    let mode = tokio::fs::metadata(&parent).await?.permissions().mode() & 0o777;

    if mode != 0o700 {
        return Err(io::Error::other(format!(
            "refusing to bind socket in directory not restricted to owner: {mode:o}"
        )));
    }

    let tmp_path = socket_path.with_added_extension("incomplete");

    if tmp_path.exists() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    let server = Server::bind(&tmp_path).await?;

    let mut permissions = tokio::fs::metadata(&tmp_path).await?.permissions();
    permissions.set_mode(0o666);
    tokio::fs::set_permissions(&tmp_path, permissions).await?;
    tokio::fs::rename(&tmp_path, socket_path).await?;

    Ok(server)
}

#[cfg(unix)]
async fn accept_loop(server: Server, print_ready: bool) -> Result<(), io::Error> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    if print_ready {
        println!("READY");
    }

    tokio::select! {
        res = server.accept() => res,
        _ = sigint.recv() => Ok(()),
        _ = sigterm.recv() => Ok(()),
    }
}

/// Run the agent server in foreground mode (no daemonization).
///
/// Unlike [`daemonize()`], this function does not fork and keeps stdout/stderr
/// attached to the terminal. It blocks indefinitely accepting connections,
/// handling SIGINT and SIGTERM for graceful shutdown.
///
/// # Arguments
///
/// * `socket_path` - Path to the Unix socket to bind
///
/// # Returns
///
/// Returns `Ok(())` on successful socket bind and server start.
/// Returns an error if the socket cannot be bound.
#[cfg(unix)]
fn foreground(socket_path: &Path) -> io::Result<()> {
    // Single-threaded: every used configuration serves a single client, and
    // a multi-thread runtime measurably hurts single-stream pipe throughput
    // (see dolang-rpc's benches/pipe_trailer.rs and benches/pipe_raw.rs).
    let rt = Builder::new_current_thread().enable_all().build()?;

    rt.block_on(async move {
        let server = create_server(socket_path).await?;
        let res = accept_loop(server, true).await;
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }
        res
    })
}

#[cfg(windows)]
fn serve_named_pipe(pipe_name: &OsStr) -> io::Result<()> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let pipe = ClientOptions::new().open(pipe_name)?;
        let server = Server::from_named_pipe_client(pipe).await?;
        tokio::select! {
            result = server.serve() => result,
            result = windows_interrupt_signal() => result,
        }
    })
}
