#![deny(warnings)]

use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::fs;
use wax::{
    Glob,
    walk::{DepthBehavior, DepthMax, Entry, LinkBehavior, WalkBehavior},
};

#[cfg(unix)]
mod client;
#[cfg(unix)]
mod protocol;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod service;

#[cfg(unix)]
mod unix {
    use super::*;

    /// Client for connecting to the agent daemon and spawning processes.
    pub use client::Client;
    /// Builder for constructing spawn requests.
    pub use client::CommandBuilder;
    /// Builder for opening files with configurable options.
    pub use client::OpenOptions;
    /// Representation of file permissions.
    pub use client::Permissions;
    /// Query result containing the daemon's environment and working directory.
    pub use client::Query;
    /// Access permission flags for the `access` method.
    pub use nix::unistd::AccessFlags;
    /// Identity passed to `chown`, either by numeric ID or by account/group name.
    pub use protocol::ChownIdentity;
    /// Type of file entry in metadata.
    pub use protocol::FileType;
    /// File metadata returned by the `metadata` method.
    pub use protocol::Metadata;
    /// Agent server that accepts connections and handles spawn requests.
    pub use server::Server;
    /// Daemonization errors.
    pub use service::ServiceError;
    /// Run the agent server in foreground mode (no daemonization).
    pub use service::foreground;

    /// Thread-safe wrapper for `tokio_unix_ipc::Sender`.
    ///
    /// The underlying sender is `Sync` but will corrupt messages with concurrent sends.
    /// This wrapper serializes access via a mutex to ensure message integrity.
    pub(crate) struct LockedSender<T>(pub(crate) tokio::sync::Mutex<tokio_unix_ipc::Sender<T>>);

    impl<T: serde::Serialize + for<'de> serde::Deserialize<'de>> LockedSender<T> {
        pub(crate) fn new(sender: tokio_unix_ipc::Sender<T>) -> Self {
            Self(tokio::sync::Mutex::new(sender))
        }

        pub(crate) async fn send(&self, message: T) -> std::io::Result<()> {
            self.0.lock().await.send(message).await
        }
    }
}

#[cfg(unix)]
pub use unix::*;

fn directory_requires_all_error() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::EISDIR)
    }
    #[cfg(not(unix))]
    {
        io::Error::new(
            io::ErrorKind::IsADirectory,
            "directory operations require all: true",
        )
    }
}

fn directory_not_empty_error() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::ENOTEMPTY)
    }
    #[cfg(not(unix))]
    {
        io::Error::from(io::ErrorKind::DirectoryNotEmpty)
    }
}

fn not_a_directory_error() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::ENOTDIR)
    }
    #[cfg(not(unix))]
    {
        io::Error::from(io::ErrorKind::NotADirectory)
    }
}

#[cfg(unix)]
async fn create_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    fs::symlink(src, dst).await
}

#[cfg(windows)]
async fn create_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let metadata = fs::metadata(src).await?;
    if metadata.is_dir() {
        fs::symlink_dir(src, dst).await
    } else {
        fs::symlink_file(src, dst).await
    }
}

async fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src).await?;
    create_symlink(&target, dst).await
}

/// Copy a filesystem entry locally.
///
/// Files and symlinks are copied directly. Directories require `all: true`
/// and are copied recursively.
pub async fn copy_local(from: &Path, to: &Path, all: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from).await?;

    if metadata.is_dir() {
        if !all {
            return Err(directory_requires_all_error());
        }

        fs::create_dir(to).await?;
        let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
        while let Some((src_dir, dst_dir)) = stack.pop() {
            let mut entries = fs::read_dir(&src_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let src_path = entry.path();
                let dst_path = dst_dir.join(entry.file_name());
                let metadata = fs::symlink_metadata(&src_path).await?;
                if metadata.is_dir() {
                    fs::create_dir(&dst_path).await?;
                    stack.push((src_path, dst_path));
                } else if metadata.is_file() {
                    fs::copy(&src_path, &dst_path).await?;
                } else if metadata.file_type().is_symlink() {
                    copy_symlink(&src_path, &dst_path).await?;
                } else {
                    return Err(io::Error::other("unsupported file type"));
                }
            }
        }
    } else if metadata.is_file() {
        fs::copy(from, to).await?;
    } else if metadata.file_type().is_symlink() {
        copy_symlink(from, to).await?;
    } else {
        return Err(io::Error::other("unsupported file type"));
    }

    Ok(())
}

/// Move a filesystem entry locally.
///
/// Files and symlinks are moved directly. Directories require `all: true`.
/// Cross-filesystem moves fall back to copy-and-delete.
pub async fn move_local(from: &Path, to: &Path, all: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from).await?;
    let is_dir = metadata.is_dir();

    if is_dir && !all {
        return Err(directory_requires_all_error());
    }

    match fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_local(from, to, all).await?;
            if is_dir {
                fs::remove_dir_all(from).await
            } else {
                fs::remove_file(from).await
            }
        }
        Err(err) => Err(err),
    }
}

async fn read_dir_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut read_dir = fs::read_dir(path).await?;
    let mut paths = Vec::new();
    while let Some(entry) = read_dir.next_entry().await? {
        paths.push(entry.path());
    }
    Ok(paths)
}

/// Remove a directory tree, but only where each removed directory is empty of
/// non-directory entries.
///
/// Returns `true` if the requested root directory itself was removed.
pub async fn remove_dir_empty_tree_local(path: &Path, ignore: bool) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path).await?;
    if !metadata.is_dir() {
        return Err(not_a_directory_error());
    }

    struct Frame {
        path: PathBuf,
        entries: Vec<PathBuf>,
        next: usize,
        removable: bool,
    }

    let mut stack = vec![Frame {
        path: path.to_owned(),
        entries: read_dir_paths(path).await?,
        next: 0,
        removable: true,
    }];
    let mut last_result = None;

    while let Some(frame) = stack.last_mut() {
        if let Some(child_removed) = last_result.take() {
            frame.removable &= child_removed;
        }

        if frame.next == frame.entries.len() {
            let removable = frame.removable;
            let path = frame.path.clone();
            stack.pop();
            if removable {
                fs::remove_dir(&path).await?;
            }
            last_result = Some(removable);
            continue;
        }

        let child_path = frame.entries[frame.next].clone();
        frame.next += 1;
        let metadata = fs::symlink_metadata(&child_path).await?;
        if metadata.is_dir() {
            stack.push(Frame {
                path: child_path.clone(),
                entries: read_dir_paths(&child_path).await?,
                next: 0,
                removable: true,
            });
        } else if ignore {
            frame.removable = false;
        } else {
            return Err(directory_not_empty_error());
        }
    }

    Ok(last_result.unwrap_or(false))
}

/// Execute a glob pattern locally.
///
/// This function executes a glob operation directly using the `wax` crate
/// without requiring an agent server connection.
///
/// # Arguments
///
/// * `pattern` - The glob pattern to match
/// * `cwd` - Optional working directory to start the search from
/// * `follow_symlinks` - Whether to follow symbolic links when traversing directories
/// * `max_depth` - Optional maximum depth to traverse
///
/// # Returns
///
/// A vector of matching paths, or an I/O error.
pub async fn glob_local(
    pattern: impl Into<String>,
    root: &Path,
    follow_symlinks: bool,
    max_depth: Option<usize>,
) -> io::Result<Vec<PathBuf>> {
    let pattern = pattern.into();

    let root = root.to_owned();

    tokio::task::spawn_blocking(move || {
        let (prefix, glob) = Glob::new(&pattern)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid glob pattern"))?
            .partition();
        let walk_root = root.join(&prefix);

        let mut behavior = WalkBehavior::default();
        if follow_symlinks {
            behavior.link = LinkBehavior::ReadTarget;
        }
        if let Some(depth) = max_depth {
            behavior.depth =
                DepthBehavior::Max(DepthMax(depth.saturating_sub(prefix.components().count())));
        }

        let mut paths = Vec::new();
        let walk = match glob {
            Some(g) => g.walk_with_behavior(&walk_root, behavior),
            None => Glob::tree().walk_with_behavior(&walk_root, behavior),
        };

        for entry in walk {
            paths.push(prefix.join(entry?.root_relative_paths().1))
        }

        Ok(paths)
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e)))
}
