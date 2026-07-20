use std::{ffi::OsStr, io, path::PathBuf};

#[cfg(unix)]
use std::{os::unix::fs::PermissionsExt, path::Path};
use tokio::runtime::Builder;

use crate::Server;

enum EnvOp {
    Set(String, String),
    Unset(String),
}

pub fn main(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<()> {
    let mut env_ops = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    let mut mode: Option<String> = None;
    let mut mode_args: Vec<String> = Vec::new();

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
        #[cfg(not(unix))]
        if s == "--listen" {
            return Err(io::Error::other("--listen is only supported on Unix"));
        }
        #[cfg(not(windows))]
        if s == "--connect" {
            return Err(io::Error::other("--connect is only supported on Windows"));
        }

        if s == "--set" {
            let Some(next) = args.next() else {
                return Err(io::Error::other("--set requires a name=value argument"));
            };
            let val = next.as_ref().to_string_lossy().into_owned();
            let (name, value) = val
                .split_once('=')
                .ok_or_else(|| io::Error::other("--set argument must have name=value form"))?;
            if name.is_empty() {
                return Err(io::Error::other("--set variable name must not be empty"));
            }
            env_ops.push(EnvOp::Set(name.to_owned(), value.to_owned()));
            continue;
        }
        if s == "--unset" {
            let Some(next) = args.next() else {
                return Err(io::Error::other("--unset requires a variable name"));
            };
            let name = next.as_ref().to_string_lossy().into_owned();
            if name.is_empty() {
                return Err(io::Error::other("--unset variable name must not be empty"));
            }
            env_ops.push(EnvOp::Unset(name));
            continue;
        }
        if s == "--cwd" {
            let Some(next) = args.next() else {
                return Err(io::Error::other("--cwd requires a path"));
            };
            cwd = Some(PathBuf::from(next.as_ref()));
            continue;
        }

        return Err(io::Error::other(format!("unknown option: {}", s)));
    }

    let mode = mode
        .ok_or_else(|| io::Error::other("missing --stdio, --listen <path>, or --connect <path>"))?;

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
    Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Server::new_split(tokio::io::stdin(), tokio::io::stdout())
                .serve()
                .await
        })
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
    let rt = Builder::new_multi_thread().enable_all().build()?;

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
        Server::from_named_pipe_client(pipe)?.serve().await
    })
}
