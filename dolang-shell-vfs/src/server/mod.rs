use std::{
    collections::HashMap, os::fd::AsRawFd, os::unix::io::OwnedFd, os::unix::process::ExitStatusExt,
    path::Path, sync::Arc,
};

use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, connect, socket};
use tokio::{
    io,
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::{Mutex, oneshot, watch},
};
use tokio_unix_ipc::{Receiver, channel_from_std, serde::Handle};

use crate::{
    Child as _, Command as _, Direct, LockedSender, OpenOptions as _, Permissions, Vfs,
    protocol::{
        AccessRequest, AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest,
        CreateDirRequest, FsMetadataRequest, GlobRequest, HardLinkRequest, MetadataRequest,
        MoveRequest, OpenRequest, ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest,
        Request, RequestKind, Response, ResponseKind, SetAttrsRequest, SetPermissionsRequest,
        SetTimesRequest, SetXattrRequest, SpawnRequest, SymlinkRequest, UnixStreamSocketRequest,
        WellKnownPathRequest, XattrRequest, XattrsRequest,
    },
};

struct Connection {
    in_flight: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    sender: LockedSender<Response>,
    server: Arc<ServerState>,
}

struct ServerState {
    direct: Direct,
    shutdown_tx: watch::Sender<()>,
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
                direct: Direct::default(),
                shutdown_tx,
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
                    RequestKind::WellKnownPath(path_request) => {
                        this.handle_well_known_path(msg.id, path_request).await;
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
                        let _ = this.server.direct.clear_cache().await;
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
                    RequestKind::FsMetadata(fs_metadata_request) => {
                        this.handle_fs_metadata(msg.id, fs_metadata_request).await;
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
                    RequestKind::HardLink(hard_link_request) => {
                        this.handle_hard_link(msg.id, hard_link_request).await;
                    }
                    RequestKind::SymlinkMetadata(metadata_request) => {
                        this.handle_symlink_metadata(msg.id, metadata_request).await;
                    }
                    RequestKind::Attrs(attrs_request) => {
                        this.handle_attrs(msg.id, attrs_request).await;
                    }
                    RequestKind::SetAttrs(attrs_request) => {
                        this.handle_set_attrs(msg.id, attrs_request).await;
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
                    RequestKind::SetTimes(set_times_request) => {
                        this.handle_set_times(msg.id, set_times_request).await;
                    }
                    RequestKind::Chown(chown_request) => {
                        this.handle_chown(msg.id, chown_request).await;
                    }
                    RequestKind::Xattrs(xattrs_request) => {
                        this.handle_xattrs(msg.id, xattrs_request).await;
                    }
                    RequestKind::Xattr(xattr_request) => {
                        this.handle_xattr(msg.id, xattr_request).await;
                    }
                    RequestKind::SetXattr(set_xattr_request) => {
                        this.handle_set_xattr(msg.id, set_xattr_request).await;
                    }
                    RequestKind::RemoveXattr(xattr_request) => {
                        this.handle_remove_xattr(msg.id, xattr_request).await;
                    }
                }
            });
        };
    }

    async fn handle_which(
        &self,
        id: u64,
        program: std::path::PathBuf,
        path: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) {
        let resolved = self
            .server
            .direct
            .which(&program, path.as_deref(), cwd.as_deref())
            .await;

        let _ = self
            .sender
            .send(Response {
                id,
                kind: ResponseKind::Which(resolved.unwrap_or(None)),
            })
            .await;
    }

    async fn handle_well_known_path(&self, id: u64, req: WellKnownPathRequest) {
        let result = self.server.direct.well_known_path(req.key, &req.env).await;

        let _ = self
            .sender
            .send(Response {
                id,
                kind: ResponseKind::WellKnownPath(
                    result.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO)),
                ),
            })
            .await;
    }

    async fn handle_spawn(self: Arc<Self>, id: u64, req: SpawnRequest) {
        let (cancel_tx, cancel_rx) = oneshot::channel();

        let mut cmd = self.server.direct.command(&req.program);
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
        let mut opts = self.server.direct.open_options();
        opts.read(req.read)
            .write(req.write)
            .append(req.append)
            .create(req.create)
            .create_new(req.create_new)
            .truncate(req.truncate)
            .no_follow(req.no_follow);

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
            self.server
                .direct
                .remove(&req.path, req.all, req.ignore)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_metadata(&self, id: u64, req: MetadataRequest) {
        let result = ResponseKind::Metadata(Self::io_result(
            self.server.direct.metadata(&req.path).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_fs_metadata(&self, id: u64, req: FsMetadataRequest) {
        let result = ResponseKind::FsMetadata(Self::io_result(
            self.server.direct.fs_metadata(&req.path, req.follow).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_create_dir(&self, id: u64, req: CreateDirRequest) {
        let result = ResponseKind::CreateDir(Self::io_result(
            self.server.direct.create_dir(&req.path, req.all).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_remove_dir(&self, id: u64, req: RemoveDirRequest) {
        let result = ResponseKind::RemoveDir(Self::io_result(
            self.server
                .direct
                .remove_dir(&req.path, req.all, req.ignore)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_copy(&self, id: u64, req: CopyRequest) {
        let result = ResponseKind::Copy(Self::io_result(
            self.server.direct.copy(&req.from, &req.to, req.all).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_rename(&self, id: u64, req: RenameRequest) {
        let result = ResponseKind::Rename(Self::io_result(
            self.server.direct.rename(&req.from, &req.to).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_move(&self, id: u64, req: MoveRequest) {
        let result = ResponseKind::Move(Self::io_result(
            self.server.direct.move_(&req.from, &req.to, req.all).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_symlink(&self, id: u64, req: SymlinkRequest) {
        let result = ResponseKind::Symlink(Self::io_result(
            self.server.direct.symlink(&req.src, &req.dst).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_hard_link(&self, id: u64, req: HardLinkRequest) {
        let result = ResponseKind::HardLink(Self::io_result(
            self.server.direct.hard_link(&req.src, &req.dst).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_symlink_metadata(&self, id: u64, req: MetadataRequest) {
        let result = ResponseKind::SymlinkMetadata(Self::io_result(
            self.server.direct.symlink_metadata(&req.path).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_attrs(&self, id: u64, req: AttrsRequest) {
        let result = ResponseKind::Attrs(Self::io_result(
            self.server.direct.attrs(&req.path, req.follow).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_set_attrs(&self, id: u64, req: SetAttrsRequest) {
        let result = ResponseKind::SetAttrs(Self::io_result(
            self.server.direct.set_attrs(&req.path, req.attrs).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_canonicalize(&self, id: u64, req: CanonicalizeRequest) {
        let result = ResponseKind::Canonicalize(Self::io_result(
            self.server.direct.canonicalize(&req.path).await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_read_link(&self, id: u64, req: ReadLinkRequest) {
        let result = ResponseKind::ReadLink(Self::io_result(
            self.server.direct.read_link(&req.path).await,
        ));

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
            self.server
                .direct
                .glob(req.pattern, &req.root, req.follow_symlinks, req.max_depth)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_set_permissions(&self, id: u64, req: SetPermissionsRequest) {
        let result = ResponseKind::SetPermissions(Self::io_result(
            self.server
                .direct
                .set_permissions(&req.path, Permissions::from_mode(req.mode))
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_set_times(&self, id: u64, req: SetTimesRequest) {
        let accessed = req
            .accessed
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let modified = req
            .modified
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let created = req
            .created
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let result = ResponseKind::SetTimes(Self::io_result(
            self.server
                .direct
                .set_times(&req.path, accessed, modified, created)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_chown(&self, id: u64, req: ChownRequest) {
        let result = ResponseKind::Chown(Self::io_result(
            self.server
                .direct
                .chown(&req.path, req.user, req.group, req.follow)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_xattrs(&self, id: u64, req: XattrsRequest) {
        let result = ResponseKind::Xattrs(Self::io_result(
            self.server
                .direct
                .xattrs(&req.path, req.namespace.as_borrowed(), req.follow)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_xattr(&self, id: u64, req: XattrRequest) {
        let result = ResponseKind::Xattr(Self::io_result(
            self.server
                .direct
                .xattr(&req.path, &req.name, req.namespace.as_deref(), req.follow)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_set_xattr(&self, id: u64, req: SetXattrRequest) {
        let result = ResponseKind::SetXattr(Self::io_result(
            self.server
                .direct
                .set_xattr(
                    &req.path,
                    &req.name,
                    req.namespace.as_deref(),
                    &req.value,
                    req.follow,
                )
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }

    async fn handle_remove_xattr(&self, id: u64, req: XattrRequest) {
        let result = ResponseKind::RemoveXattr(Self::io_result(
            self.server
                .direct
                .remove_xattr(&req.path, &req.name, req.namespace.as_deref(), req.follow)
                .await,
        ));

        let _ = self.sender.send(Response { id, kind: result }).await;
    }
}
