use std::{
    any::Any,
    collections::HashMap,
    io,
    os::unix::{io::OwnedFd, net::UnixStream as StdUnixStream, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
};

use tokio::{
    fs::File,
    net::UnixStream,
    sync::{Mutex, oneshot},
};
use tokio_unix_ipc::{Receiver, Sender, serde::Handle};

use crate::{
    LockedSender,
    protocol::{
        AccessRequest, CanonicalizeRequest, ChownIdentity, ChownRequest, CopyRequest,
        CreateDirRequest, GlobRequest, Metadata, MetadataRequest, MoveRequest, OpenRequest,
        ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest, Request, RequestKind,
        Response, ResponseKind, SetPermissionsRequest, SpawnRequest, SymlinkRequest, Timestamp,
        UnixStreamSocketRequest, UtimeRequest,
    },
};

type PendingMap = HashMap<u64, Box<dyn Any + Send>>;

struct AliveState {
    sender: LockedSender<Request>,
    pending: PendingMap,
    next_id: u64,
}

impl AliveState {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn insert_pending<T: Send + 'static>(
        &mut self,
        id: u64,
        sender: oneshot::Sender<Result<T, io::Error>>,
    ) {
        self.pending.insert(id, Box::new(sender));
    }

    fn remove_pending<T: Send + 'static>(
        &mut self,
        id: u64,
    ) -> Option<oneshot::Sender<Result<T, io::Error>>> {
        self.pending
            .remove(&id)?
            .downcast::<oneshot::Sender<Result<T, io::Error>>>()
            .ok()
            .map(|b| *b)
    }

    fn complete<T: Send + 'static>(&mut self, id: u64, result: Result<T, io::Error>) {
        if let Some(tx) = self.remove_pending::<T>(id) {
            let _ = tx.send(result);
        }
    }
}

enum ClientState {
    Alive(AliveState),
    Dead(String),
}

struct ClientInner {
    state: Mutex<ClientState>,
}

/// Query result containing the daemon's environment and working directory.
pub struct Query {
    /// Environment variables from the daemon's process.
    pub env: HashMap<String, String>,
    /// Daemon's current working directory.
    pub cwd: PathBuf,
}

/// Representation of file permissions.
///
/// This struct mimics [`std::fs::Permissions`] and provides access to
/// file permission bits, including Unix-specific mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    mode: u32,
}

impl Permissions {
    /// Creates a new `Permissions` from the given mode bits.
    pub fn from_mode(mode: u32) -> Self {
        Self { mode }
    }

    /// Returns the underlying mode bits.
    ///
    /// On Unix, this returns the full mode including file type and permissions.
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// Sets the mode bits.
    ///
    /// On Unix, this sets the full mode including file type and permissions.
    pub fn set_mode(&mut self, mode: u32) {
        self.mode = mode;
    }

    /// Returns true if these permissions describe a readonly file.
    ///
    /// On Unix, this checks if any write bits are set for owner, group, or others.
    pub fn readonly(&self) -> bool {
        // Check if any write bits are set (owner: 0o200, group: 0o020, others: 0o002)
        self.mode & 0o222 == 0
    }

    /// Sets the readonly flag.
    ///
    /// If `readonly` is true, clears all write permission bits.
    /// If `readonly` is false, sets write permission for the owner.
    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            // Clear all write bits
            self.mode &= !0o222;
        } else {
            // Set owner write bit
            self.mode |= 0o200;
        }
    }
}

impl Metadata {
    /// Returns the file permissions.
    pub fn permissions(&self) -> Permissions {
        Permissions::from_mode(self.mode)
    }
}

/// Client for connecting to the agent daemon and spawning processes.
pub struct Client {
    inner: Arc<ClientInner>,
}

impl Client {
    /// Connect to an agent daemon at the given socket path.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        Self::from_stream(UnixStream::connect(path).await?).await
    }

    /// Connect using an existing `UnixStream`.
    pub async fn from_stream(stream: UnixStream) -> Result<Self, io::Error> {
        Self::from_std_stream(stream.into_std()?)
    }

    pub(crate) fn from_channel(sender: Sender<Request>, receiver: Receiver<Response>) -> Self {
        let alive_state = AliveState {
            sender: LockedSender::new(sender),
            pending: Default::default(),
            next_id: 0,
        };

        let inner = Arc::new(ClientInner {
            state: Mutex::new(ClientState::Alive(alive_state)),
        });

        let inner_clone = inner.clone();

        tokio::spawn(async move {
            let _ = receive_loop(receiver, inner_clone).await;
        });

        Self { inner }
    }

    fn from_std_stream(stream: StdUnixStream) -> Result<Self, io::Error> {
        let (sender, receiver) = tokio_unix_ipc::channel_from_std(stream)?;
        Ok(Self::from_channel(sender, receiver))
    }

    /// Create a spawn request for the given program.
    pub fn command(&self, program: impl AsRef<Path>) -> CommandBuilder<'_> {
        CommandBuilder::new(self, program)
    }

    /// Create an open options builder for opening files.
    pub fn open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }

    /// Create a Unix stream socket in the agent namespace and return its descriptor.
    ///
    /// If `bind` is provided, the socket is bound to that pathname first. If `connect`
    /// is provided, the socket is then connected to that pathname. Either or both may
    /// be omitted.
    pub async fn unix_stream_socket<B, C>(
        &self,
        bind: Option<B>,
        connect: Option<C>,
    ) -> Result<OwnedFd, io::Error>
    where
        B: AsRef<Path>,
        C: AsRef<Path>,
    {
        let req = UnixStreamSocketRequest {
            bind: bind.map(|p| p.as_ref().to_path_buf()),
            connect: connect.map(|p| p.as_ref().to_path_buf()),
        };

        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::UnixStreamSocket(req),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Remove a path.
    pub async fn remove(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Remove(RemoveRequest {
                        path: path.as_ref().to_path_buf(),
                        all,
                        ignore,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Get file metadata at the given path.
    pub async fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Metadata(MetadataRequest {
                        path: path.as_ref().to_path_buf(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Create a directory at the given path.
    pub async fn create_dir(&self, path: impl AsRef<Path>, all: bool) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::CreateDir(CreateDirRequest {
                        path: path.as_ref().to_path_buf(),
                        all,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Remove a directory, optionally pruning empty subdirectories.
    pub async fn remove_dir(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::RemoveDir(RemoveDirRequest {
                        path: path.as_ref().to_path_buf(),
                        ignore,
                        all,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Copy a filesystem entry from one location to another.
    pub async fn copy(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Copy(CopyRequest {
                        from: from.as_ref().to_path_buf(),
                        to: to.as_ref().to_path_buf(),
                        all,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Rename a file or directory.
    pub async fn rename(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Rename(RenameRequest {
                        from: from.as_ref().to_path_buf(),
                        to: to.as_ref().to_path_buf(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Move a filesystem entry from one location to another.
    pub async fn move_(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Move(MoveRequest {
                        from: from.as_ref().to_path_buf(),
                        to: to.as_ref().to_path_buf(),
                        all,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Create a symlink at dst pointing to src.
    pub async fn symlink(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Symlink(SymlinkRequest {
                        src: src.as_ref().to_path_buf(),
                        dst: dst.as_ref().to_path_buf(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Get symlink metadata at the given path (does not follow symlinks).
    pub async fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::SymlinkMetadata(MetadataRequest {
                        path: path.as_ref().to_path_buf(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Get the canonical form of a path.
    pub async fn canonicalize(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Canonicalize(CanonicalizeRequest {
                        path: path.as_ref().to_path_buf(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Read the target of a symlink.
    pub async fn read_link(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::ReadLink(ReadLinkRequest {
                        path: path.as_ref().to_path_buf(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Check file accessibility.
    ///
    /// Mode is a bitmask of accessibility flags from [`AccessFlags`](crate::AccessFlags):
    /// - `AccessFlags::F_OK`: Test for existence
    /// - `AccessFlags::R_OK`: Test for read permission
    /// - `AccessFlags::W_OK`: Test for write permission
    /// - `AccessFlags::X_OK`: Test for execute permission
    pub async fn access(
        &self,
        path: impl AsRef<Path>,
        mode: crate::AccessFlags,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Access(AccessRequest {
                        path: path.as_ref().to_path_buf(),
                        mode: mode.bits(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Execute a glob pattern and return matching paths.
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
    pub async fn glob(
        &self,
        pattern: impl Into<String>,
        root: &Path,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<PathBuf>, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Glob(GlobRequest {
                        pattern: pattern.into(),
                        root: root.to_path_buf(),
                        follow_symlinks,
                        max_depth,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Changes the permissions found on a file or a directory.
    ///
    /// This is an async version of [`std::fs::set_permissions`].
    pub async fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        perm: Permissions,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::SetPermissions(SetPermissionsRequest {
                        path: path.as_ref().to_path_buf(),
                        mode: perm.mode(),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Change file access and modification times.
    pub async fn utime(
        &self,
        path: impl AsRef<Path>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Utime(UtimeRequest {
                        path: path.as_ref().to_path_buf(),
                        accessed: accessed.map(|(secs, nanos)| Timestamp { secs, nanos }),
                        modified: modified.map(|(secs, nanos)| Timestamp { secs, nanos }),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Query the daemon's environment variables and current working directory.
    pub async fn query(&self) -> Result<Query, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Query,
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Resolve a program path using the daemon's PATH resolution.
    pub async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<PathBuf>, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Which {
                        program: program.as_ref().to_path_buf(),
                        path: path.map(|p| p.to_string()),
                        cwd: cwd.map(|p| p.to_path_buf()),
                    },
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Signal the daemon to stop accepting new connections.
    pub async fn stop(&self) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Stop,
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Clear the server's path resolution cache.
    pub async fn clear_cache(&self) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::ClearCache,
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    /// Change file ownership at the given path.
    pub async fn chown(
        &self,
        path: impl AsRef<Path>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Chown(ChownRequest {
                        path: path.as_ref().to_path_buf(),
                        user,
                        group,
                        follow,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }
}

impl TryFrom<OwnedFd> for Client {
    type Error = io::Error;

    fn try_from(value: OwnedFd) -> Result<Self, Self::Error> {
        let stream = StdUnixStream::from(value);
        stream.set_nonblocking(true)?;
        Self::from_std_stream(stream)
    }
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Builder for constructing spawn requests.
///
/// # Example
///
/// ```ignore
/// let status = client
///     .command("ls")
///     .arg("-l")
///     .arg("/tmp")
///     .env("RUST_LOG", "info")
///     .env_remove("DEBUG")
///     .current_dir("/home")
///     .stdin(fd)
///     .status()
///     .await?;
/// ```
pub struct CommandBuilder<'a> {
    client: &'a Client,
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<PathBuf>,
    stdin_fd: Option<OwnedFd>,
    stdout_fd: Option<OwnedFd>,
    stderr_fd: Option<OwnedFd>,
}

impl<'a> CommandBuilder<'a> {
    fn new(client: &'a Client, program: impl AsRef<Path>) -> Self {
        Self {
            client,
            program: program.as_ref().to_path_buf(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin_fd: None,
            stdout_fd: None,
            stderr_fd: None,
        }
    }

    /// Add a command-line argument.
    pub fn arg(&mut self, arg: impl Into<String>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Add multiple command-line arguments.
    pub fn args(&mut self, args: impl IntoIterator<Item = String>) -> &mut Self {
        self.args.extend(args);
        self
    }

    /// Set an environment variable. Use `env_remove` to unset.
    pub fn env(&mut self, key: impl Into<String>, val: impl Into<String>) -> &mut Self {
        self.env.insert(key.into(), Some(val.into()));
        self
    }

    /// Remove an environment variable.
    pub fn env_remove(&mut self, key: impl Into<String>) -> &mut Self {
        self.env.insert(key.into(), None);
        self
    }

    /// Set the working directory for the spawned process.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Redirect stdin from the given file descriptor.
    pub fn stdin(&mut self, fd: impl Into<OwnedFd>) -> &mut Self {
        self.stdin_fd = Some(fd.into());
        self
    }

    /// Redirect stdin from `/dev/null`.
    pub fn stdin_null(&mut self) -> &mut Self {
        self.stdin_fd = None;
        self
    }

    /// Redirect stdout to the given file descriptor.
    pub fn stdout(&mut self, fd: impl Into<OwnedFd>) -> &mut Self {
        self.stdout_fd = Some(fd.into());
        self
    }

    /// Redirect stdout to `/dev/null`.
    pub fn stdout_null(&mut self) -> &mut Self {
        self.stdout_fd = None;
        self
    }

    /// Redirect stderr to the given file descriptor.
    pub fn stderr(&mut self, fd: impl Into<OwnedFd>) -> &mut Self {
        self.stderr_fd = Some(fd.into());
        self
    }

    /// Redirect stderr to `/dev/null`.
    pub fn stderr_null(&mut self) -> &mut Self {
        self.stderr_fd = None;
        self
    }

    /// Spawn the process and wait for it to exit.
    pub async fn status(self) -> Result<ExitStatus, io::Error> {
        let req = SpawnRequest {
            program: self.program,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            stdin_fd: self.stdin_fd.map(Handle::new),
            stdout_fd: self.stdout_fd.map(Handle::new),
            stderr_fd: self.stderr_fd.map(Handle::new),
        };

        let mut state = self.client.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Spawn(req),
                };
                let _ = alive.sender.send(request).await;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }
}

/// Builder for opening files with configurable options.
///
/// # Example
///
/// ```ignore
/// let file = client
///     .open_options()
///     .read(true)
///     .write(true)
///     .create(true)
///     .open("/tmp/myfile.txt")
///     .await?;
/// ```
pub struct OpenOptions<'a> {
    client: &'a Client,
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
}

impl<'a> OpenOptions<'a> {
    fn new(client: &'a Client) -> Self {
        Self {
            client,
            read: false,
            write: false,
            append: false,
            create: false,
            create_new: false,
            truncate: false,
        }
    }

    /// Set read access mode.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Set write access mode.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Set append mode.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    /// Set create mode (creates file if it doesn't exist).
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Set create_new mode (fails if file already exists).
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Set truncate mode (truncates file on open).
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Open the file at the given path.
    pub async fn open(&self, path: impl AsRef<Path>) -> Result<File, io::Error> {
        let req = OpenRequest {
            path: path.as_ref().to_path_buf(),
            read: self.read,
            write: self.write,
            append: self.append,
            create: self.create,
            create_new: self.create_new,
            truncate: self.truncate,
        };

        let mut state = self.client.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Open(req),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }
}

async fn receive_loop(receiver: Receiver<Response>, inner: Arc<ClientInner>) {
    let err_msg = loop {
        let response = match receiver.recv().await {
            Ok(res) => res,
            Err(err) => break err.to_string(),
        };

        let mut state = inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => match response.kind {
                ResponseKind::Spawn(result) => {
                    alive.complete(
                        response.id,
                        result
                            .map(ExitStatus::from_raw)
                            .map_err(io::Error::from_raw_os_error),
                    );
                }
                ResponseKind::Cancel => {}
                ResponseKind::Query { env, cwd } => {
                    alive.complete(response.id, Ok(Query { env, cwd }))
                }
                ResponseKind::Which(result) => alive.complete(response.id, Ok(result)),
                ResponseKind::Stop => alive.complete(response.id, Ok(())),
                ResponseKind::ClearCache => alive.complete(response.id, Ok(())),
                ResponseKind::Open(result) => {
                    alive.complete(
                        response.id,
                        result
                            .map(|fd| File::from_std(fd.into_inner().into()))
                            .map_err(io::Error::from_raw_os_error),
                    );
                }
                ResponseKind::UnixStreamSocket(result) => {
                    alive.complete(
                        response.id,
                        result
                            .map(|fd| fd.into_inner())
                            .map_err(io::Error::from_raw_os_error),
                    );
                }
                ResponseKind::Remove(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Metadata(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::CreateDir(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::RemoveDir(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Copy(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Rename(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Move(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Symlink(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::SymlinkMetadata(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Canonicalize(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::ReadLink(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Access(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Glob(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::SetPermissions(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Utime(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Chown(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
            },
            ClientState::Dead(_) => {}
        }
    };

    let mut state = inner.state.lock().await;
    let pending = std::mem::replace(&mut *state, ClientState::Dead(err_msg.clone()));

    if let ClientState::Alive(alive) = pending {
        // Dropping pending entries causes waiters to receive errors
        drop(alive);
    }
}
