use std::{
    error,
    fmt::{self, Display, Formatter},
    io::{self},
    os::unix::fs::PermissionsExt,
    path::Path,
};
use tokio::{
    runtime::Builder,
    signal::unix::{SignalKind, signal},
};

use crate::Server;

/// Daemonization errors.
#[derive(Debug)]
pub enum ServiceError {
    /// I/O error (pipe operations, file operations).
    Io(io::Error),
}

impl Display for ServiceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl error::Error for ServiceError {}

impl From<io::Error> for ServiceError {
    fn from(e: io::Error) -> Self {
        ServiceError::Io(e)
    }
}

async fn create_server(socket_path: &Path) -> Result<Server, ServiceError> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?;
    let mode = tokio::fs::metadata(&parent).await?.permissions().mode() & 0o777;

    if mode != 0o700 {
        return Err(io::Error::other(format!(
            "refusing to bind socket in directory not restricted to owner: {mode:o}"
        ))
        .into());
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

async fn accept_loop(server: Server, print_ready: bool) -> Result<(), io::Error> {
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
pub fn foreground(socket_path: impl AsRef<Path>) -> Result<(), ServiceError> {
    let rt = Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ServiceError::Io(io::Error::other(e)))?;

    rt.block_on(async move {
        let server = create_server(socket_path.as_ref()).await?;
        let res = accept_loop(server, true).await;
        let socket_path = socket_path.as_ref();
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }
        res.map_err(ServiceError::Io)
    })
}
