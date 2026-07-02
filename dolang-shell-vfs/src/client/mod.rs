use std::{
    any::Any,
    collections::HashMap,
    io,
    os::unix::{
        io::{AsFd, OwnedFd},
        net::UnixStream as StdUnixStream,
        process::ExitStatusExt,
    },
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
    Attrs, Child, ChownIdentity, Command, LockedSender, Metadata, Permissions, PipeRecv, PipeSend,
    ReadDir, Vfs, WellKnownPath, XattrEntry,
    direct::Direct,
    protocol::{
        AccessRequest, AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest,
        CreateDirRequest, GlobRequest, HardLinkRequest, MetadataRequest, MoveRequest, OpenRequest,
        ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest, Request, RequestKind,
        Response, ResponseKind, SetAttrsRequest, SetPermissionsRequest, SetTimesRequest,
        SetXattrRequest, SpawnRequest, SymlinkRequest, Timestamp, UnixStreamSocketRequest,
        WellKnownPathRequest, XattrRequest, XattrsRequest,
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

/// Client for connecting to the agent daemon and spawning processes.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
    direct: Direct,
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

        Self {
            inner,
            direct: Direct::default(),
        }
    }

    fn from_std_stream(stream: StdUnixStream) -> Result<Self, io::Error> {
        let (sender, receiver) = tokio_unix_ipc::channel_from_std(stream)?;
        Ok(Self::from_channel(sender, receiver))
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

    pub async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::WellKnownPath(WellKnownPathRequest {
                        key,
                        env: env.clone(),
                    }),
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
}

impl TryFrom<OwnedFd> for Client {
    type Error = io::Error;

    fn try_from(value: OwnedFd) -> Result<Self, Self::Error> {
        let stream = StdUnixStream::from(value);
        stream.set_nonblocking(true)?;
        Self::from_std_stream(stream)
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
///     .spawn()
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

pub struct ClientChild<'a> {
    client: &'a Client,
    request_id: u64,
    inner: Option<oneshot::Receiver<Result<ExitStatus, io::Error>>>,
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
}

impl Child for ClientChild<'_> {
    async fn wait(&mut self) -> Result<ExitStatus, io::Error> {
        let result = match self.inner.as_mut() {
            Some(inner) => inner.await.expect("oneshot sender dropped"),
            None => return Err(io::Error::other("child already waited")),
        };
        self.inner = None;
        result
    }

    async fn terminate(self) -> Result<ExitStatus, io::Error> {
        let inner = self.inner;
        if inner.is_none() {
            return Err(io::Error::other("child already waited"));
        }
        let mut state = self.client.inner.state.lock().await;
        if let ClientState::Alive(alive) = &mut *state {
            let _ = alive
                .sender
                .send(Request {
                    id: self.request_id,
                    kind: RequestKind::Cancel,
                })
                .await;
        }
        drop(state);

        if let Some(inner) = inner {
            return inner.await.expect("oneshot sender dropped");
        }
        Err(io::Error::other("child already waited"))
    }
}

impl<'a> Command for CommandBuilder<'a> {
    type Child = ClientChild<'a>;

    fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(arg.to_owned());
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.env.insert(key.to_owned(), Some(val.to_owned()));
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        self.env.insert(key.to_owned(), None);
        self
    }

    fn current_dir(&mut self, dir: &Path) -> &mut Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        self.stdin_fd = Some(pipe.into_blocking_fd()?);
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stdout_fd = Some(pipe.into_blocking_fd()?);
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdin_fd = Some(std::io::stdin().as_fd().try_clone_to_owned()?);
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdout_fd = Some(std::io::stdout().as_fd().try_clone_to_owned()?);
        Ok(self)
    }

    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self {
        self.stdin_fd = Some(fd);
        self
    }

    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self {
        self.stdout_fd = Some(fd);
        self
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.stdin_fd = None;
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.stdout_fd = None;
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stderr_fd = Some(pipe.into_blocking_fd()?);
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.stderr_fd = Some(std::io::stderr().as_fd().try_clone_to_owned()?);
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.stderr_fd = Some(std::io::stdout().as_fd().try_clone_to_owned()?);
        Ok(self)
    }

    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self {
        self.stderr_fd = Some(fd);
        self
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.stderr_fd = None;
        self
    }

    async fn spawn(self) -> io::Result<Self::Child> {
        let req = SpawnRequest {
            program: self.program,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            stdin_fd: self.stdin_fd.map(Handle::new),
            stdout_fd: self.stdout_fd.map(Handle::new),
            stderr_fd: self.stderr_fd.map(Handle::new),
        };
        let client = self.client;
        let (id, rx) = {
            let mut state = client.inner.state.lock().await;
            match &mut *state {
                ClientState::Alive(alive) => {
                    let (tx, rx) = oneshot::channel();
                    let id = alive.next_id();
                    alive.insert_pending(id, tx);
                    alive
                        .sender
                        .send(Request {
                            id,
                            kind: RequestKind::Spawn(req),
                        })
                        .await?;
                    (id, rx)
                }
                ClientState::Dead(msg) => return Err(io::Error::other(msg.clone())),
            }
        };

        Ok(ClientChild {
            client,
            request_id: id,
            inner: Some(rx),
        })
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
    no_follow: bool,
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
            no_follow: false,
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

    /// Set no-follow mode for the final path component.
    pub fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
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
            no_follow: self.no_follow,
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

impl crate::OpenOptions for OpenOptions<'_> {
    fn read(&mut self, read: bool) -> &mut Self {
        self.read(read)
    }

    fn write(&mut self, write: bool) -> &mut Self {
        self.write(write)
    }

    fn append(&mut self, append: bool) -> &mut Self {
        self.append(append)
    }

    fn create(&mut self, create: bool) -> &mut Self {
        self.create(create)
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new(create_new)
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate(truncate)
    }

    fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow(no_follow)
    }

    async fn open(&self, path: impl AsRef<Path>) -> Result<File, io::Error> {
        self.open(path).await
    }
}

impl Vfs for Client {
    type OpenOptions<'a>
        = OpenOptions<'a>
    where
        Self: 'a;
    type Command<'a>
        = CommandBuilder<'a>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_> {
        OpenOptions::new(self)
    }

    fn command(&self, program: impl AsRef<Path>) -> Self::Command<'_> {
        CommandBuilder::new(self, program)
    }

    async fn read_dir(&self, path: impl AsRef<Path>) -> Result<ReadDir, io::Error> {
        let file = self.open_options().read(true).open(path.as_ref()).await?;
        ReadDir::from_fd(file.into_std().await.into())
    }

    async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<PathBuf>, io::Error> {
        Client::which(self, program, path, cwd).await
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        Client::well_known_path(self, key, env).await
    }

    async fn clear_cache(&self) -> Result<(), io::Error> {
        Client::clear_cache(self).await
    }

    async fn file_xattrs(
        &self,
        file: &File,
        namespace: crate::XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        self.direct.file_xattrs(file, namespace).await
    }

    async fn file_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<u8>, io::Error> {
        self.direct.file_xattr(file, name, namespace).await
    }

    async fn file_set_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<(), io::Error> {
        self.direct
            .file_set_xattr(file, name, namespace, value)
            .await
    }

    async fn file_remove_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<(), io::Error> {
        self.direct.file_remove_xattr(file, name, namespace).await
    }

    async fn xattrs(
        &self,
        path: impl AsRef<Path>,
        namespace: crate::XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Xattrs(XattrsRequest {
                        path: path.as_ref().to_path_buf(),
                        namespace: namespace.into(),
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

    async fn xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Xattr(XattrRequest {
                        path: path.as_ref().to_path_buf(),
                        name: name.to_owned(),
                        namespace: namespace.map(str::to_owned),
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

    async fn set_xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
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
                    kind: RequestKind::SetXattr(SetXattrRequest {
                        path: path.as_ref().to_path_buf(),
                        name: name.to_owned(),
                        namespace: namespace.map(str::to_owned),
                        value: value.to_vec(),
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

    async fn remove_xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
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
                    kind: RequestKind::RemoveXattr(XattrRequest {
                        path: path.as_ref().to_path_buf(),
                        name: name.to_owned(),
                        namespace: namespace.map(str::to_owned),
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

    async fn remove(
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

    async fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
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

    async fn create_dir(&self, path: impl AsRef<Path>, all: bool) -> Result<(), io::Error> {
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

    async fn remove_dir(
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

    async fn copy(
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

    async fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), io::Error> {
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

    async fn move_(
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

    async fn symlink(&self, src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), io::Error> {
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

    async fn hard_link(
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
                    kind: RequestKind::HardLink(HardLinkRequest {
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

    async fn symlink_dir(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        self.symlink(src, dst).await
    }

    async fn symlink_file(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        self.symlink(src, dst).await
    }

    async fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
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

    async fn attrs(&self, path: impl AsRef<Path>, follow: bool) -> Result<Attrs, io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::Attrs(AttrsRequest {
                        path: path.as_ref().to_path_buf(),
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

    async fn set_attrs(&self, path: impl AsRef<Path>, attrs: Attrs) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::SetAttrs(SetAttrsRequest {
                        path: path.as_ref().to_path_buf(),
                        attrs,
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    async fn canonicalize(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
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

    async fn read_link(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
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

    async fn glob(
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

    async fn set_permissions(
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

    async fn set_times(
        &self,
        path: impl AsRef<Path>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        let mut state = self.inner.state.lock().await;
        match &mut *state {
            ClientState::Alive(alive) => {
                let (tx, rx) = oneshot::channel();
                let id = alive.next_id();
                alive.insert_pending(id, tx);
                let request = Request {
                    id,
                    kind: RequestKind::SetTimes(SetTimesRequest {
                        path: path.as_ref().to_path_buf(),
                        accessed: accessed.map(|(secs, nanos)| Timestamp { secs, nanos }),
                        modified: modified.map(|(secs, nanos)| Timestamp { secs, nanos }),
                        created: created.map(|(secs, nanos)| Timestamp { secs, nanos }),
                    }),
                };
                alive.sender.send(request).await?;
                drop(state);
                rx.await.expect("oneshot sender dropped")
            }
            ClientState::Dead(msg) => Err(io::Error::other(msg.clone())),
        }
    }

    async fn chown(
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
                ResponseKind::WellKnownPath(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
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
                ResponseKind::HardLink(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::SymlinkMetadata(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Attrs(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::SetAttrs(result) => {
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
                ResponseKind::SetTimes(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Chown(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Xattrs(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::Xattr(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::SetXattr(result) => {
                    alive.complete(response.id, result.map_err(io::Error::from_raw_os_error));
                }
                ResponseKind::RemoveXattr(result) => {
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
