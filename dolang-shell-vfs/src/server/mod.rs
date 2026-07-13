use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    os::unix::io::OwnedFd,
    os::unix::process::ExitStatusExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use dolang_rpc::{CallContext, OsHandle};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, connect, socket};
use tokio::{
    io,
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::watch,
};

use crate::{
    Child as _, Command as _, Direct, OpenOptions as _, Permissions, Vfs,
    protocol::{
        AccessRequest, AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest,
        CreateDirRequest, FsMetadataRequest, GlobRequest, HardLinkRequest, MetadataRequest,
        MoveRequest, OpenRequest, ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest,
        RequestKind, ResponseKind, SetAttrsRequest, SetPermissionsRequest, SetTimesRequest,
        SetXattrRequest, SpawnRequest, SymlinkRequest, UnixStreamSocketRequest, VfsProtocol,
        WellKnownPathRequest, XattrRequest, XattrsRequest,
    },
};

struct Connection {
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
        let rpc = dolang_rpc::Server::<VfsProtocol>::from_unix_stream(stream.into_std()?)?;
        let connection = Arc::new(Connection {
            server: self.shared.clone(),
        });
        tokio::spawn(async move {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_handler = stop.clone();
            let handler = connection.clone();
            let _ = rpc
                .serve(async move |context, request| match request {
                    RequestKind::Spawn(request) => handler.handle_spawn_rpc(context, request).await,
                    RequestKind::Stop => {
                        stop_handler.store(true, Ordering::Release);
                        context.shutdown();
                        ResponseKind::Stop
                    }
                    request => handler.handle(request).await,
                })
                .await;
            if stop.load(Ordering::Acquire) {
                let _ = connection.server.shutdown_tx.send(());
            }
        });
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

    async fn handle(&self, kind: RequestKind) -> ResponseKind {
        match kind {
            RequestKind::Query => self.handle_query().await,
            RequestKind::Which { program, path, cwd } => {
                self.handle_which(program, path, cwd).await
            }
            RequestKind::WellKnownPath(request) => self.handle_well_known_path(request).await,
            RequestKind::Stop | RequestKind::Spawn(_) => unreachable!(),
            RequestKind::ClearCache => {
                let _ = self.server.direct.clear_cache().await;
                ResponseKind::ClearCache
            }
            RequestKind::Open(request) => self.handle_open(request).await,
            RequestKind::UnixStreamSocket(request) => self.handle_unix_stream_socket(request).await,
            RequestKind::Remove(request) => self.handle_remove(request).await,
            RequestKind::Metadata(request) => self.handle_metadata(request).await,
            RequestKind::FsMetadata(request) => self.handle_fs_metadata(request).await,
            RequestKind::CreateDir(request) => self.handle_create_dir(request).await,
            RequestKind::RemoveDir(request) => self.handle_remove_dir(request).await,
            RequestKind::Copy(request) => self.handle_copy(request).await,
            RequestKind::Rename(request) => self.handle_rename(request).await,
            RequestKind::Move(request) => self.handle_move(request).await,
            RequestKind::Symlink(request) => self.handle_symlink(request).await,
            RequestKind::HardLink(request) => self.handle_hard_link(request).await,
            RequestKind::SymlinkMetadata(request) => self.handle_symlink_metadata(request).await,
            RequestKind::Attrs(request) => self.handle_attrs(request).await,
            RequestKind::SetAttrs(request) => self.handle_set_attrs(request).await,
            RequestKind::Canonicalize(request) => self.handle_canonicalize(request).await,
            RequestKind::ReadLink(request) => self.handle_read_link(request).await,
            RequestKind::Access(request) => self.handle_access(request).await,
            RequestKind::Glob(request) => self.handle_glob(request).await,
            RequestKind::SetPermissions(request) => self.handle_set_permissions(request).await,
            RequestKind::SetTimes(request) => self.handle_set_times(request).await,
            RequestKind::Chown(request) => self.handle_chown(request).await,
            RequestKind::Xattrs(request) => self.handle_xattrs(request).await,
            RequestKind::Xattr(request) => self.handle_xattr(request).await,
            RequestKind::SetXattr(request) => self.handle_set_xattr(request).await,
            RequestKind::RemoveXattr(request) => self.handle_remove_xattr(request).await,
        }
    }

    async fn handle_which(
        &self,
        program: std::path::PathBuf,
        path: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> ResponseKind {
        let resolved = self
            .server
            .direct
            .which(&program, path.as_deref(), cwd.as_deref())
            .await;

        ResponseKind::Which(resolved.unwrap_or(None))
    }

    async fn handle_well_known_path(&self, req: WellKnownPathRequest) -> ResponseKind {
        let result = self.server.direct.well_known_path(req.key, &req.env).await;
        ResponseKind::WellKnownPath(result.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO)))
    }

    async fn handle_spawn_rpc(
        &self,
        context: &mut CallContext<VfsProtocol>,
        req: SpawnRequest,
    ) -> ResponseKind {
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
                return ResponseKind::Spawn(Err(errno));
            }
        };

        let exit = match context.cancel_guard(async |_| child.wait().await).await {
            Ok(exit) => exit,
            Err(_) => child.terminate().await,
        };
        let code = match exit {
            Ok(status) => status.into_raw(),
            Err(_) => -1,
        };
        ResponseKind::Spawn(Ok(code))
    }

    async fn handle_query(&self) -> ResponseKind {
        let env: HashMap<_, _> = std::env::vars().collect();
        let cwd = std::env::current_dir().unwrap_or_default();
        ResponseKind::Query { env, cwd }
    }

    async fn handle_open(&self, req: OpenRequest) -> ResponseKind {
        let mut opts = self.server.direct.open_options();
        opts.read(req.read)
            .write(req.write)
            .append(req.append)
            .create(req.create)
            .create_new(req.create_new)
            .truncate(req.truncate)
            .no_follow(req.no_follow);

        match opts.open(&req.path).await {
            Ok(file) => {
                let fd = OsHandle::new(OwnedFd::from(file.into_std().await));
                ResponseKind::Open(Ok(fd))
            }
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(libc::EIO);
                ResponseKind::Open(Err(errno))
            }
        }
    }

    async fn handle_unix_stream_socket(&self, req: UnixStreamSocketRequest) -> ResponseKind {
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

        match result {
            Ok(Ok(fd)) => ResponseKind::UnixStreamSocket(Ok(OsHandle::new(fd))),
            Ok(Err(e)) => ResponseKind::UnixStreamSocket(Err(e as i32)),
            Err(_) => ResponseKind::UnixStreamSocket(Err(libc::EIO)),
        }
    }

    async fn handle_remove(&self, req: RemoveRequest) -> ResponseKind {
        ResponseKind::Remove(Self::io_result(
            self.server
                .direct
                .remove(&req.path, req.all, req.ignore)
                .await,
        ))
    }

    async fn handle_metadata(&self, req: MetadataRequest) -> ResponseKind {
        ResponseKind::Metadata(Self::io_result(
            self.server.direct.metadata(&req.path).await,
        ))
    }

    async fn handle_fs_metadata(&self, req: FsMetadataRequest) -> ResponseKind {
        ResponseKind::FsMetadata(Self::io_result(
            self.server.direct.fs_metadata(&req.path, req.follow).await,
        ))
    }

    async fn handle_create_dir(&self, req: CreateDirRequest) -> ResponseKind {
        ResponseKind::CreateDir(Self::io_result(
            self.server.direct.create_dir(&req.path, req.all).await,
        ))
    }

    async fn handle_remove_dir(&self, req: RemoveDirRequest) -> ResponseKind {
        ResponseKind::RemoveDir(Self::io_result(
            self.server
                .direct
                .remove_dir(&req.path, req.all, req.ignore)
                .await,
        ))
    }

    async fn handle_copy(&self, req: CopyRequest) -> ResponseKind {
        ResponseKind::Copy(Self::io_result(
            self.server.direct.copy(&req.from, &req.to, req.all).await,
        ))
    }

    async fn handle_rename(&self, req: RenameRequest) -> ResponseKind {
        ResponseKind::Rename(Self::io_result(
            self.server.direct.rename(&req.from, &req.to).await,
        ))
    }

    async fn handle_move(&self, req: MoveRequest) -> ResponseKind {
        ResponseKind::Move(Self::io_result(
            self.server.direct.move_(&req.from, &req.to, req.all).await,
        ))
    }

    async fn handle_symlink(&self, req: SymlinkRequest) -> ResponseKind {
        ResponseKind::Symlink(Self::io_result(
            self.server
                .direct
                .symlink(Path::new(""), &req.src, &req.dst)
                .await,
        ))
    }

    async fn handle_hard_link(&self, req: HardLinkRequest) -> ResponseKind {
        ResponseKind::HardLink(Self::io_result(
            self.server.direct.hard_link(&req.src, &req.dst).await,
        ))
    }

    async fn handle_symlink_metadata(&self, req: MetadataRequest) -> ResponseKind {
        ResponseKind::SymlinkMetadata(Self::io_result(
            self.server.direct.symlink_metadata(&req.path).await,
        ))
    }

    async fn handle_attrs(&self, req: AttrsRequest) -> ResponseKind {
        ResponseKind::Attrs(Self::io_result(
            self.server.direct.attrs(&req.path, req.follow).await,
        ))
    }

    async fn handle_set_attrs(&self, req: SetAttrsRequest) -> ResponseKind {
        ResponseKind::SetAttrs(Self::io_result(
            self.server.direct.set_attrs(&req.path, req.attrs).await,
        ))
    }

    async fn handle_canonicalize(&self, req: CanonicalizeRequest) -> ResponseKind {
        ResponseKind::Canonicalize(Self::io_result(
            self.server.direct.canonicalize(&req.path).await,
        ))
    }

    async fn handle_read_link(&self, req: ReadLinkRequest) -> ResponseKind {
        ResponseKind::ReadLink(Self::io_result(
            self.server.direct.read_link(&req.path).await,
        ))
    }

    async fn handle_access(&self, req: AccessRequest) -> ResponseKind {
        use nix::unistd::{AccessFlags, access};

        let path = req.path;
        let mode = req.mode;

        tokio::task::spawn_blocking(move || {
            let flags = AccessFlags::from_bits(mode).unwrap_or(AccessFlags::empty());
            match access(&path, flags) {
                Ok(()) => ResponseKind::Access(Ok(())),
                Err(e) => ResponseKind::Access(Err(e as i32)),
            }
        })
        .await
        .unwrap_or(ResponseKind::Access(Err(libc::EIO)))
    }

    async fn handle_glob(&self, req: GlobRequest) -> ResponseKind {
        ResponseKind::Glob(Self::io_result(
            self.server
                .direct
                .glob(req.pattern, &req.root, req.follow_symlinks, req.max_depth)
                .await,
        ))
    }

    async fn handle_set_permissions(&self, req: SetPermissionsRequest) -> ResponseKind {
        ResponseKind::SetPermissions(Self::io_result(
            self.server
                .direct
                .set_permissions(&req.path, Permissions::from_mode(req.mode))
                .await,
        ))
    }

    async fn handle_set_times(&self, req: SetTimesRequest) -> ResponseKind {
        let accessed = req
            .accessed
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let modified = req
            .modified
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        let created = req
            .created
            .map(|timestamp| (timestamp.secs, timestamp.nanos));
        ResponseKind::SetTimes(Self::io_result(
            self.server
                .direct
                .set_times(&req.path, accessed, modified, created)
                .await,
        ))
    }

    async fn handle_chown(&self, req: ChownRequest) -> ResponseKind {
        ResponseKind::Chown(Self::io_result(
            self.server
                .direct
                .chown(&req.path, req.user, req.group, req.follow)
                .await,
        ))
    }

    async fn handle_xattrs(&self, req: XattrsRequest) -> ResponseKind {
        ResponseKind::Xattrs(Self::io_result(
            self.server
                .direct
                .xattrs(&req.path, req.namespace.as_borrowed(), req.follow)
                .await,
        ))
    }

    async fn handle_xattr(&self, req: XattrRequest) -> ResponseKind {
        ResponseKind::Xattr(Self::io_result(
            self.server
                .direct
                .xattr(&req.path, &req.name, req.namespace.as_deref(), req.follow)
                .await,
        ))
    }

    async fn handle_set_xattr(&self, req: SetXattrRequest) -> ResponseKind {
        ResponseKind::SetXattr(Self::io_result(
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
        ))
    }

    async fn handle_remove_xattr(&self, req: XattrRequest) -> ResponseKind {
        ResponseKind::RemoveXattr(Self::io_result(
            self.server
                .direct
                .remove_xattr(&req.path, &req.name, req.namespace.as_deref(), req.follow)
                .await,
        ))
    }
}
