use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    os::unix::io::OwnedFd,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, connect, socket};
use tokio::{
    fs::OpenOptions,
    io,
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::{Mutex, oneshot, watch},
};
use tokio_unix_ipc::{Receiver, channel_from_std, serde::Handle};

use crate::{
    Child as _, Command as _, Direct, LockedSender, Permissions, Vfs,
    protocol::{
        AccessRequest, CanonicalizeRequest, ChownRequest, CopyRequest, CreateDirRequest,
        GlobRequest, MetadataRequest, MoveRequest, OpenRequest, ReadLinkRequest, RemoveDirRequest,
        RemoveRequest, RenameRequest, Request, RequestKind, Response, ResponseKind,
        SetPermissionsRequest, SpawnRequest, SymlinkRequest, UnixStreamSocketRequest, UtimeRequest,
    },
};

struct Connection {
    in_flight: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    sender: LockedSender<Response>,
    server: Arc<ServerState>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    program: PathBuf,
    path: Option<String>,
    cwd: Option<PathBuf>,
}

struct PathCache {
    map: Mutex<HashMap<CacheKey, PathBuf>>,
}

impl PathCache {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    async fn resolve(
        &self,
        program: &Path,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Option<PathBuf> {
        let key = CacheKey {
            program: program.to_path_buf(),
            path: path.map(|p| p.to_string()),
            cwd: cwd.map(|p| p.to_path_buf()),
        };

        let cached = {
            let map = self.map.lock().await;
            map.get(&key).cloned()
        };

        if let Some(cached) = cached {
            return Some(cached);
        }

        let path_env = path
            .map(|p| p.into())
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_else(|| "".into());

        let program = program.to_path_buf();
        let cwd = cwd.map(|p| p.to_path_buf());

        let resolved = tokio::task::spawn_blocking(move || {
            which::which_in(
                &program,
                Some(path_env),
                cwd.as_deref().unwrap_or(Path::new("")),
            )
            .ok()
        })
        .await
        .unwrap_or(None);

        if let Some(ref resolved_path) = resolved {
            let mut map = self.map.lock().await;
            map.insert(key, resolved_path.clone());
        }

        resolved
    }

    async fn clear(&self) {
        self.map.lock().await.clear();
    }
}

struct ServerState {
    shutdown_tx: watch::Sender<()>,
    path_cache: PathCache,
}

/// Agent server that accepts connections and handles spawn requests.
///
/// Created via [`Server::bind`] or [`Server::from_listener`].
pub struct Server {
    listener: UnixListener,
    shared: Arc<ServerState>,
}

impl Server {
    /// Bind to a socket path and create a server.
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        Ok(Self::from_listener(UnixListener::bind(path)?))
    }

    /// Create a server from an existing `UnixListener`.
    fn from_listener(listener: UnixListener) -> Self {
        let (shutdown_tx, _) = watch::channel(());
        Self {
            listener,
            shared: Arc::new(ServerState {
                shutdown_tx,
                path_cache: PathCache::new(),
            }),
        }
    }

    fn handle_accept(&self, res: io::Result<(UnixStream, SocketAddr)>) -> Result<(), io::Error> {
        let (stream, _) = res?;
        let (sender, receiver) = channel_from_std::<Response, Request>(stream.into_std()?)?;

        let connection = Arc::new(Connection {
            server: self.shared.clone(),
            in_flight: Mutex::new(HashMap::new()),
            sender: LockedSender(Mutex::new(sender)),
        });

        tokio::spawn(connection.run(receiver));
        Ok(())
    }

    /// Accept incoming connections in an infinite loop.
    ///
    /// Each connection spawns a handler task that processes requests.
    pub async fn accept(&self) -> Result<(), io::Error> {
        let mut shutdown_rx = self.shared.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                res = self.listener.accept() => {
                    let _ = self.handle_accept(res);
                }
                _ = shutdown_rx.changed() => {
                    return Ok(());
                }
            }
        }
    }
}

impl Connection {
    fn io_result<T>(result: io::Result<T>) -> Result<T, i32> {
        result.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }

    async fn run(self: Arc<Self>, receiver: Receiver<Request>) {
        let receiver = receiver;

        let _ = loop {
            let msg = match receiver.recv().await {
                Ok(msg) => msg,
                Err(err) => break err,
            };
            let this = self.clone();
            tokio::spawn(async move {
                match msg.kind {
                    RequestKind::Spawn(spawn_request) => {
                        Connection::handle_spawn(this, msg.id, spawn_request).await;
                    }
                    RequestKind::Cancel => {
                        this.handle_cancel(msg.id).await;
                    }
                    RequestKind::Query => {
                        this.handle_query(msg.id).await;
                    }
                    RequestKind::Which { program, path, cwd } => {
                        this.handle_which(msg.id, program, path, cwd).await;
                    }
                    RequestKind::Stop => {
                        let _ = this
                            .sender
                            .send(Response {
                                id: msg.id,
                                kind: ResponseKind::Stop,
                            })
                            .await;
                        let _ = this.server.shutdown_tx.send(());
                    }
                    RequestKind::ClearCache => {
                        this.server.path_cache.clear().await;
                        let _ = this
                            .sender
                            .send(Response {
                                id: msg.id,
                                kind: ResponseKind::ClearCache,
                            })
                            .await;
                    }
                    RequestKind::Open(open_request) => {
                        this.handle_open(msg.id, open_request).await;
                    }
                    RequestKind::UnixStreamSocket(socket_request) => {
                        this.handle_unix_stream_socket(msg.id, socket_request).await;
                    }
                    RequestKind::Remove(remove_request) => {
                        this.handle_remove(msg.id, remove_request).await;
                    }
                    RequestKind::Metadata(metadata_request) => {
                        this.handle_metadata(msg.id, metadata_request).await;
                    }
                    RequestKind::CreateDir(create_dir_request) => {
                        this.handle_create_dir(msg.id, create_dir_request).await;
                    }
                    RequestKind::RemoveDir(remove_dir_request) => {
                        this.handle_remove_dir(msg.id, remove_dir_request).await;
                    }
                    RequestKind::Copy(copy_request) => {
                        this.handle_copy(msg.id, copy_request).await;
                    }
                    RequestKind::Rename(rename_request) => {
                        this.handle_rename(msg.id, rename_request).await;
                    }
                    RequestKind::Move(move_request) => {
                        this.handle_move(msg.id, move_request).await;
                    }
                    RequestKind::Symlink(symlink_request) => {
                        this.handle_symlink(msg.id, symlink_request).await;
                    }
                    RequestKind::SymlinkMetadata(metadata_request) => {
                        this.handle_symlink_metadata(msg.id, metadata_request).await;
                    }
                    RequestKind::Canonicalize(canonicalize_request) => {
                        this.handle_canonicalize(msg.id, canonicalize_request).await;
                    }
                    RequestKind::ReadLink(read_link_request) => {
                        this.handle_read_link(msg.id, read_link_request).await;
                    }
                    RequestKind::Access(access_request) => {
                        this.handle_access(msg.id, access_request).await;
                    }
                    RequestKind::Glob(glob_request) => {
                        this.handle_glob(msg.id, glob_request).await;
                    }
                    RequestKind::SetPermissions(perm_request) => {
                        this.handle_set_permissions(msg.id, perm_request).await;
                    }
                    RequestKind::Utime(utime_request) => {
                        this.handle_utime(msg.id, utime_request).await;
                    }
                    RequestKind::Chown(chown_request) => {
                        this.handle_chown(msg.id, chown_request).await;
                    }
                }
            });
        };
    }

    async fn handle_which(
        &self,
        id: u64,
        program: PathBuf,
        path: Option<String>,
        cwd: Option<PathBuf>,
    ) {
        let resolved = self
            .server
            .path_cache
            .resolve(&program, path.as_deref(), cwd.as_deref())
            .await;

        let _ = self
            .sender
            .send(Response {
                id,
                kind: ResponseKind::Which(resolved),
            })
            .await;
    }

    async fn handle_spawn(self: Arc<Self>, id: u64, req: SpawnRequest) {
        let (cancel_tx, cancel_rx) = oneshot::channel();

        let path_override = req
            .env
            .get("PATH")
            .map(|path| path.as_deref().unwrap_or(""));

        let resolved_program = self
            .server
            .path_cache
            .resolve(&req.program, path_override, req.cwd.as_deref())
            .await;

        let resolved_program = match resolved_program {
            Some(p) => p,
            None => {
                let _ = self
                    .sender
                    .send(Response {
                        id,
                        kind: ResponseKind::Spawn(Err(2)),
                    })
                    .await;
                return;
            }
        };

        let mut cmd = Direct.command(&resolved_program);
        for arg in &req.args {
            cmd.arg(arg);
        }

        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }

        for (k, v) in &req.env {
            match v {
                Some(val) => {
                    cmd.env(k, val);
                }
                None => {
                    cmd.env_remove(k);
                }
            };
        }

        if let Some(fd) = req.stdin_fd {
            cmd.stdin_fd(fd.into_inner());
        } else {
            cmd.stdin_null();
        }
        if let Some(fd) = req.stdout_fd {
            cmd.stdout_fd(fd.into_inner());
        } else {
            cmd.stdout_null();
        }
        if let Some(fd) = req.stderr_fd {
            cmd.stderr_fd(fd.into_inner());
        } else {
            cmd.stderr_null();
        }

        let mut child = match cmd.spawn().await {
            Ok(child) => child,
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(libc::EIO);
                let _ = self
                    .sender
                    .send(Response {
                        id,
                        kind: ResponseKind::Spawn(Err(errno)),
                    })
                    .await;
                return;
            }
        };

        self.in_flight.lock().await.insert(id, cancel_tx);

        let exit = tokio::select! {
            _ = cancel_rx => child.terminate().await,
            exit = child.wait() => exit,
        };

        self.in_flight.lock().await.remove(&id);

        let code = match exit {
            Ok(status) => status.into_raw(),
            Err(_) => -1,
        };

        let _ = self
            .sender
            .send(Response {
                id,
                kind: ResponseKind::Spawn(Ok(code)),
            })
            .await;
    }

    async fn handle_cancel(&self, id: u64) {
        let mut in_flight = self.in_flight.lock().await;

        if let Some(spawn) = in_flight.remove(&id) {
            let _ = spawn.send(());
        }

        let _ = self
            .sender
            .send(Response {
                id,
                kind: ResponseKind::Cancel,
            })
            .await;
    }

    async fn handle_query(&self, id: u64) {
        let env: HashMap<_, _> = std::env::vars().collect();
        let cwd = std::env::current_dir().unwrap_or_default();
        let _ = self
            .sender
            .send(Response {
                id,
                kind: ResponseKind::Query { env, cwd },
            })
            .await;
    }

    async fn handle_open(&self, id: u64, req: OpenRequest) {
        let mut opts = OpenOptions::new();
        opts.read(req.read)
            .write(req.write)
            .append(req.append)
            .create(req.create)
            .create_new(req.create_new)
            .truncate(req.truncate);

        let result = match opts.open(&req.path).await {
            Ok(file) => {
                let fd = Handle::new(OwnedFd::from(file.into_std().await));
                ResponseKind::Open(Ok(fd))
            }
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(libc::EIO);
                ResponseKind::Open(Err(errno))
            }
        };

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_unix_stream_socket(&self, id: u64, req: UnixStreamSocketRequest) {
        let result = tokio::task::spawn_blocking(move || {
            let fd = socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )?;

            if let Some(path) = req.bind {
                let addr = UnixAddr::new(&path)?;
                bind(fd.as_raw_fd(), &addr)?;
            }

            if let Some(path) = req.connect {
                let addr = UnixAddr::new(&path)?;
                connect(fd.as_raw_fd(), &addr)?;
            }

            Ok::<OwnedFd, nix::Error>(fd)
        })
        .await;

        let kind = match result {
            Ok(Ok(fd)) => ResponseKind::UnixStreamSocket(Ok(Handle::new(fd))),
            Ok(Err(e)) => ResponseKind::UnixStreamSocket(Err(e as i32)),
            Err(_) => ResponseKind::UnixStreamSocket(Err(libc::EIO)),
        };

        let _ = self.sender.send(Response { id, kind }).await;
    }

    async fn handle_remove(&self, id: u64, req: RemoveRequest) {
        let result = ResponseKind::Remove(Self::io_result(
            Direct.remove(&req.path, req.all, req.ignore).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_metadata(&self, id: u64, req: MetadataRequest) {
        let result = ResponseKind::Metadata(Self::io_result(Direct.metadata(&req.path).await));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_create_dir(&self, id: u64, req: CreateDirRequest) {
        let result =
            ResponseKind::CreateDir(Self::io_result(Direct.create_dir(&req.path, req.all).await));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_remove_dir(&self, id: u64, req: RemoveDirRequest) {
        let result = ResponseKind::RemoveDir(Self::io_result(
            Direct.remove_dir(&req.path, req.all, req.ignore).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_copy(&self, id: u64, req: CopyRequest) {
        let result = ResponseKind::Copy(Self::io_result(
            Direct.copy(&req.from, &req.to, req.all).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_rename(&self, id: u64, req: RenameRequest) {
        let result = ResponseKind::Rename(Self::io_result(Direct.rename(&req.from, &req.to).await));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_move(&self, id: u64, req: MoveRequest) {
        let result = ResponseKind::Move(Self::io_result(
            Direct.move_(&req.from, &req.to, req.all).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_symlink(&self, id: u64, req: SymlinkRequest) {
        let result =
            ResponseKind::Symlink(Self::io_result(Direct.symlink(&req.src, &req.dst).await));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_symlink_metadata(&self, id: u64, req: MetadataRequest) {
        let result = ResponseKind::SymlinkMetadata(Self::io_result(
            Direct.symlink_metadata(&req.path).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_canonicalize(&self, id: u64, req: CanonicalizeRequest) {
        let result =
            ResponseKind::Canonicalize(Self::io_result(Direct.canonicalize(&req.path).await));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_read_link(&self, id: u64, req: ReadLinkRequest) {
        let result = ResponseKind::ReadLink(Self::io_result(Direct.read_link(&req.path).await));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_access(&self, id: u64, req: AccessRequest) {
        use nix::unistd::{AccessFlags, access};

        let path = req.path;
        let mode = req.mode;

        let result = tokio::task::spawn_blocking(move || {
            let flags = AccessFlags::from_bits(mode).unwrap_or(AccessFlags::empty());
            match access(&path, flags) {
                Ok(()) => ResponseKind::Access(Ok(())),
                Err(e) => ResponseKind::Access(Err(e as i32)),
            }
        })
        .await
        .unwrap_or(ResponseKind::Access(Err(libc::EIO)));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_glob(&self, id: u64, req: GlobRequest) {
        let result = ResponseKind::Glob(Self::io_result(
            Direct
                .glob(req.pattern, &req.root, req.follow_symlinks, req.max_depth)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_set_permissions(&self, id: u64, req: SetPermissionsRequest) {
        let result = ResponseKind::SetPermissions(Self::io_result(
            Direct
                .set_permissions(&req.path, Permissions::from_mode(req.mode))
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_utime(&self, id: u64, req: UtimeRequest) {
        let accessed = req
            .accessed
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let modified = req
            .modified
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let result = ResponseKind::Utime(Self::io_result(
            Direct.utime(&req.path, accessed, modified).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_chown(&self, id: u64, req: ChownRequest) {
        let result = ResponseKind::Chown(Self::io_result(
            Direct
                .chown(&req.path, req.user, req.group, req.follow)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }
}
