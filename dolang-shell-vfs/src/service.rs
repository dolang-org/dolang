use std::{ffi::OsStr, io};

#[cfg(unix)]
use std::{os::unix::fs::PermissionsExt, path::Path};
use tokio::runtime::Builder;

use crate::Server;

pub fn main(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> io::Result<()> {
    let mut args = args.into_iter();
    let Some(mode) = args.next() else {
        return Err(io::Error::other(
            "missing --stdio, --listen <path>, or --connect <path>",
        ));
    };
    let mode = mode.as_ref();
    if mode == "--stdio" {
        if args.next().is_some() {
            return Err(io::Error::other("--stdio takes no arguments"));
        }
        serve_stdio()?;
        return Ok(());
    }
    #[cfg(unix)]
    if mode == "--listen" {
        let Some(path) = args.next() else {
            return Err(io::Error::other("--listen requires a socket path"));
        };
        if args.next().is_some() {
            return Err(io::Error::other("--listen takes exactly one argument"));
        }
        return foreground(path.as_ref());
    }
    #[cfg(windows)]
    if mode == "--connect" {
        let Some(path) = args.next() else {
            return Err(io::Error::other("--connect requires a pipe name"));
        };
        if args.next().is_some() {
            return Err(io::Error::other("--connect takes exactly one argument"));
        }
        return serve_named_pipe(path.as_ref());
    }
    #[cfg(not(unix))]
    if mode == "--listen" {
        return Err(io::Error::other("--listen is only supported on Unix"));
    }
    #[cfg(not(windows))]
    if mode == "--connect" {
        return Err(io::Error::other("--connect is only supported on Windows"));
    }
    Err(io::Error::other(format!(
        "unknown option: {}",
        mode.to_string_lossy()
    )))
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
fn foreground(socket_path: &OsStr) -> io::Result<()> {
    let socket_path = Path::new(socket_path);
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
