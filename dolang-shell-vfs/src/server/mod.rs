use std::{
    collections::HashMap,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(unix)]
use std::path::{Path, PathBuf};

use dolang_rpc::{CallContext, DefaultHandle, OsHandle};
#[cfg(unix)]
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, connect, socket};
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::io::OwnedFd, unix::process::ExitStatusExt};
use tokio::io;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
use tokio::{
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::watch,
};

use crate::{
    Child as _, Command as _, Direct, FileHandle as _, OpenOptions as _, Permissions,
    Utf8TypedPath, Vfs,
    protocol::{
        AccessRequest, AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest,
        CreateDirRequest, FsMetadataRequest, GlobRequest, HardLinkRequest, MetadataRequest,
        MoveRequest, OpenRequest, ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest,
        RequestKind, ResponseKind, SetAttrsRequest, SetPermissionsRequest, SetTimesRequest,
        SetXattrRequest, SpawnRequest, StreamsRequest, SymlinkKind, SymlinkRequest,
        UnixStreamSocketRequest, VfsProtocol, WellKnownPathRequest, WirePath, XattrRequest,
        XattrsRequest,
    },
};

fn request_path(path: &WirePath) -> Utf8TypedPath<'_> {
    path.into()
}

struct Connection {
    server: Arc<ServerState>,
}

struct ServerState {
    direct: Direct,
    #[cfg(unix)]
    shutdown_tx: watch::Sender<()>,
}

/// Agent server that handles VFS RPC requests.
pub struct Server {
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(windows)]
    rpc: dolang_rpc::Server<VfsProtocol>,
    shared: Arc<ServerState>,
}

impl Server {
    /// Bind to a socket path and create a server.
    #[cfg(unix)]
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        Ok(Self::from_listener(UnixListener::bind(path)?))
    }

    /// Create a server from an existing `UnixListener`.
    #[cfg(unix)]
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

    /// Creates a VFS RPC server on the client end of a connected Windows named pipe.
    #[cfg(windows)]
    pub fn from_named_pipe_client(pipe: NamedPipeClient) -> Result<Self, io::Error> {
        Ok(Self {
            rpc: dolang_rpc::Server::from_named_pipe_client(pipe)?,
            shared: Arc::new(ServerState {
                direct: Direct::default(),
            }),
        })
    }

    #[cfg(unix)]
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
            let _ = serve_connection(rpc, handler, stop_handler).await;
            if stop.load(Ordering::Acquire) {
                let _ = connection.server.shutdown_tx.send(());
            }
        });
        Ok(())
    }

    /// Accept incoming connections in an infinite loop.
    ///
    /// Each connection spawns a handler task that processes requests.
    #[cfg(unix)]
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

    /// Serves one connected Windows named-pipe session.
    #[cfg(windows)]
    pub async fn serve(self) -> Result<(), io::Error> {
        let connection = Arc::new(Connection {
            server: self.shared,
        });
        let stop = Arc::new(AtomicBool::new(false));
        match serve_connection(self.rpc, connection, stop).await {
            Ok(()) => Ok(()),
            Err(dolang_rpc::Error::ConnectionClosed) => Ok(()),
            Err(dolang_rpc::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }
}

async fn serve_connection(
    rpc: dolang_rpc::Server<VfsProtocol>,
    connection: Arc<Connection>,
    stop: Arc<AtomicBool>,
) -> Result<(), dolang_rpc::Error> {
    rpc.serve(async move |context, request| match request {
        RequestKind::Spawn(request) => connection.handle_spawn_rpc(context, request).await,
        RequestKind::Stop => {
            stop.store(true, Ordering::Release);
            context.shutdown();
            ResponseKind::Stop
        }
        request => connection.handle(request).await,
    })
    .await
}

impl Connection {
    fn wire_result<T, E>(
        result: std::result::Result<T, E>,
    ) -> std::result::Result<T, crate::protocol::WireError>
    where
        E: Into<crate::Error>,
    {
        result.map_err(wire_error)
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
            RequestKind::ReadDir { path } => self.handle_read_dir(path).await,
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
            RequestKind::Streams(request) => self.handle_streams(request).await,
        }
    }

    async fn handle_which(
        &self,
        program: WirePath,
        path: Option<String>,
        cwd: Option<WirePath>,
    ) -> ResponseKind {
        let resolved = self
            .server
            .direct
            .which(
                request_path(&program),
                path.as_deref(),
                cwd.as_ref().map(request_path),
            )
            .await;

        ResponseKind::Which(resolved.unwrap_or(None).map(Into::into))
    }

    async fn handle_well_known_path(&self, req: WellKnownPathRequest) -> ResponseKind {
        let result = self.server.direct.well_known_path(req.key, &req.env).await;
        ResponseKind::WellKnownPath(result.map(Into::into).map_err(wire_error))
    }

    async fn handle_spawn_rpc(
        &self,
        context: &mut CallContext<VfsProtocol>,
        req: SpawnRequest,
    ) -> ResponseKind {
        let mut cmd = self.server.direct.command(request_path(&req.program));
        for arg in &req.args {
            cmd.arg(arg);
        }

        if let Some(cwd) = &req.cwd {
            cmd.current_dir(request_path(cwd));
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

        if let Some(handle) = req.stdin_fd {
            cmd.stdin_handle(crate::DirectFile::from_std(handle.into_inner().into()))
                .unwrap();
        } else {
            cmd.stdin_null();
        }
        if let Some(handle) = req.stdout_fd {
            cmd.stdout_handle(crate::DirectFile::from_std(handle.into_inner().into()))
                .unwrap();
        } else {
            cmd.stdout_null();
        }
        if let Some(handle) = req.stderr_fd {
            cmd.stderr_handle(crate::DirectFile::from_std(handle.into_inner().into()))
                .unwrap();
        } else {
            cmd.stderr_null();
        }

        let mut child = match cmd.spawn().await {
            Ok(child) => child,
            Err(e) => {
                return ResponseKind::Spawn(Err(wire_error(e)));
            }
        };

        let exit = match context.cancel_guard(async |_| child.wait().await).await {
            Ok(exit) => exit,
            Err(_) => child.terminate().await,
        };
        ResponseKind::Spawn(exit.map(exit_status_to_raw).map_err(wire_error))
    }

    async fn handle_query(&self) -> ResponseKind {
        let env: HashMap<_, _> = std::env::vars().collect();
        let cwd = WirePath::try_from(std::env::current_dir().unwrap_or_default())
            .expect("current directory must be UTF-8");
        ResponseKind::Query {
            env,
            cwd,
            target: crate::TargetInfo::current(),
        }
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

        match opts.open(request_path(&req.path)).await {
            Ok(file) => {
                let handle: DefaultHandle = file.try_into_std().await.unwrap().into();
                ResponseKind::Open(Ok(OsHandle::new(handle)))
            }
            Err(e) => ResponseKind::Open(Err(wire_error(e))),
        }
    }

    async fn handle_read_dir(&self, path: WirePath) -> ResponseKind {
        let result: crate::Result<Vec<crate::DirEntry>> = async {
            let mut read_dir = self.server.direct.read_dir(request_path(&path)).await?;
            let mut entries = Vec::new();
            while let Some(entry) = read_dir.next_entry().await? {
                entries.push(entry);
            }
            Ok(entries)
        }
        .await;
        ResponseKind::ReadDir(Self::wire_result(result))
    }

    #[cfg(unix)]
    async fn handle_unix_stream_socket(&self, req: UnixStreamSocketRequest) -> ResponseKind {
        let result = tokio::task::spawn_blocking(move || {
            let fd = socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::empty(),
                None,
            )?;

            if let Some(path) = req.bind {
                let path = PathBuf::try_from(path).map_err(|_| nix::errno::Errno::EINVAL)?;
                let addr = UnixAddr::new(&path)?;
                bind(fd.as_raw_fd(), &addr)?;
            }

            if let Some(path) = req.connect {
                let path = PathBuf::try_from(path).map_err(|_| nix::errno::Errno::EINVAL)?;
                let addr = UnixAddr::new(&path)?;
                connect(fd.as_raw_fd(), &addr)?;
            }

            Ok::<OwnedFd, nix::Error>(fd)
        })
        .await;

        match result {
            Ok(Ok(fd)) => ResponseKind::UnixStreamSocket(Ok(OsHandle::new(fd))),
            Ok(Err(e)) => ResponseKind::UnixStreamSocket(Err(wire_error(
                io::Error::from_raw_os_error(e as i32),
            ))),
            Err(_) => ResponseKind::UnixStreamSocket(Err(wire_error(
                io::Error::from_raw_os_error(libc::EIO),
            ))),
        }
    }

    #[cfg(not(unix))]
    async fn handle_unix_stream_socket(&self, _req: UnixStreamSocketRequest) -> ResponseKind {
        ResponseKind::UnixStreamSocket(Err(wire_error(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix stream sockets are not supported on this platform",
        ))))
    }

    async fn handle_remove(&self, req: RemoveRequest) -> ResponseKind {
        ResponseKind::Remove(Self::wire_result(
            self.server
                .direct
                .remove(request_path(&req.path), req.all, req.ignore)
                .await,
        ))
    }

    async fn handle_metadata(&self, req: MetadataRequest) -> ResponseKind {
        ResponseKind::Metadata(Self::wire_result(
            self.server.direct.metadata(request_path(&req.path)).await,
        ))
    }

    async fn handle_fs_metadata(&self, req: FsMetadataRequest) -> ResponseKind {
        ResponseKind::FsMetadata(Self::wire_result(
            self.server
                .direct
                .fs_metadata(request_path(&req.path), req.follow)
                .await,
        ))
    }

    async fn handle_create_dir(&self, req: CreateDirRequest) -> ResponseKind {
        ResponseKind::CreateDir(Self::wire_result(
            self.server
                .direct
                .create_dir(request_path(&req.path), req.all)
                .await,
        ))
    }

    async fn handle_remove_dir(&self, req: RemoveDirRequest) -> ResponseKind {
        ResponseKind::RemoveDir(Self::wire_result(
            self.server
                .direct
                .remove_dir(request_path(&req.path), req.all, req.ignore)
                .await,
        ))
    }

    async fn handle_copy(&self, req: CopyRequest) -> ResponseKind {
        ResponseKind::Copy(Self::wire_result(
            self.server
                .direct
                .copy(request_path(&req.from), request_path(&req.to), req.all)
                .await,
        ))
    }

    async fn handle_rename(&self, req: RenameRequest) -> ResponseKind {
        ResponseKind::Rename(Self::wire_result(
            self.server
                .direct
                .rename(request_path(&req.from), request_path(&req.to))
                .await,
        ))
    }

    async fn handle_move(&self, req: MoveRequest) -> ResponseKind {
        ResponseKind::Move(Self::wire_result(
            self.server
                .direct
                .move_(request_path(&req.from), request_path(&req.to), req.all)
                .await,
        ))
    }

    async fn handle_symlink(&self, req: SymlinkRequest) -> ResponseKind {
        let result = match req.kind {
            SymlinkKind::Infer => {
                self.server
                    .direct
                    .symlink(
                        request_path(&req.cwd),
                        request_path(&req.src),
                        request_path(&req.dst),
                    )
                    .await
            }
            SymlinkKind::Dir => {
                self.server
                    .direct
                    .symlink_dir(request_path(&req.src), request_path(&req.dst))
                    .await
            }
            SymlinkKind::File => {
                self.server
                    .direct
                    .symlink_file(request_path(&req.src), request_path(&req.dst))
                    .await
            }
        };
        ResponseKind::Symlink(Self::wire_result(result))
    }

    async fn handle_hard_link(&self, req: HardLinkRequest) -> ResponseKind {
        ResponseKind::HardLink(Self::wire_result(
            self.server
                .direct
                .hard_link(request_path(&req.src), request_path(&req.dst))
                .await,
        ))
    }

    async fn handle_symlink_metadata(&self, req: MetadataRequest) -> ResponseKind {
        ResponseKind::SymlinkMetadata(Self::wire_result(
            self.server
                .direct
                .symlink_metadata(request_path(&req.path))
                .await,
        ))
    }

    async fn handle_attrs(&self, req: AttrsRequest) -> ResponseKind {
        ResponseKind::Attrs(Self::wire_result(
            self.server
                .direct
                .attrs(request_path(&req.path), req.follow)
                .await,
        ))
    }

    async fn handle_set_attrs(&self, req: SetAttrsRequest) -> ResponseKind {
        ResponseKind::SetAttrs(Self::wire_result(
            self.server
                .direct
                .set_attrs(request_path(&req.path), req.attrs)
                .await,
        ))
    }

    async fn handle_canonicalize(&self, req: CanonicalizeRequest) -> ResponseKind {
        let result = self
            .server
            .direct
            .canonicalize(request_path(&req.path))
            .await
            .map(Into::into);
        ResponseKind::Canonicalize(Self::wire_result(result))
    }

    async fn handle_read_link(&self, req: ReadLinkRequest) -> ResponseKind {
        let result = self
            .server
            .direct
            .read_link(request_path(&req.path))
            .await
            .map(Into::into);
        ResponseKind::ReadLink(Self::wire_result(result))
    }

    #[cfg(unix)]
    async fn handle_access(&self, req: AccessRequest) -> ResponseKind {
        use nix::unistd::{AccessFlags, access};

        let path = req.path;
        let mode = req.mode;

        tokio::task::spawn_blocking(move || {
            let path = match PathBuf::try_from(path) {
                Ok(path) => path,
                Err(_) => {
                    return ResponseKind::Access(Err(wire_error(io::Error::from_raw_os_error(
                        libc::EINVAL,
                    ))));
                }
            };
            let flags = AccessFlags::from_bits(mode).unwrap_or(AccessFlags::empty());
            match access(&path, flags) {
                Ok(()) => ResponseKind::Access(Ok(())),
                Err(e) => {
                    ResponseKind::Access(Err(wire_error(io::Error::from_raw_os_error(e as i32))))
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            ResponseKind::Access(Err(wire_error(io::Error::from_raw_os_error(libc::EIO))))
        })
    }

    #[cfg(not(unix))]
    async fn handle_access(&self, _req: AccessRequest) -> ResponseKind {
        ResponseKind::Access(Err(wire_error(io::Error::new(
            io::ErrorKind::Unsupported,
            "POSIX access checks are not supported on this platform",
        ))))
    }

    async fn handle_glob(&self, req: GlobRequest) -> ResponseKind {
        ResponseKind::Glob(Self::wire_result(
            self.server
                .direct
                .glob(
                    req.pattern,
                    request_path(&req.root),
                    req.follow_symlinks,
                    req.max_depth,
                )
                .await
                .map(|paths| paths.into_iter().map(Into::into).collect()),
        ))
    }

    async fn handle_set_permissions(&self, req: SetPermissionsRequest) -> ResponseKind {
        ResponseKind::SetPermissions(Self::wire_result(
            self.server
                .direct
                .set_permissions(request_path(&req.path), Permissions::from_mode(req.mode))
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
        ResponseKind::SetTimes(Self::wire_result(
            self.server
                .direct
                .set_times(request_path(&req.path), accessed, modified, created)
                .await,
        ))
    }

    async fn handle_chown(&self, req: ChownRequest) -> ResponseKind {
        ResponseKind::Chown(Self::wire_result(
            self.server
                .direct
                .chown(request_path(&req.path), req.user, req.group, req.follow)
                .await,
        ))
    }

    async fn handle_xattrs(&self, req: XattrsRequest) -> ResponseKind {
        ResponseKind::Xattrs(Self::wire_result(
            self.server
                .direct
                .xattrs(
                    request_path(&req.path),
                    req.namespace.as_borrowed(),
                    req.follow,
                )
                .await,
        ))
    }

    async fn handle_xattr(&self, req: XattrRequest) -> ResponseKind {
        ResponseKind::Xattr(Self::wire_result(
            self.server
                .direct
                .xattr(
                    request_path(&req.path),
                    &req.name,
                    req.namespace.as_deref(),
                    req.follow,
                )
                .await,
        ))
    }

    async fn handle_set_xattr(&self, req: SetXattrRequest) -> ResponseKind {
        ResponseKind::SetXattr(Self::wire_result(
            self.server
                .direct
                .set_xattr(
                    request_path(&req.path),
                    &req.name,
                    req.namespace.as_deref(),
                    &req.value,
                    req.follow,
                )
                .await,
        ))
    }

    async fn handle_remove_xattr(&self, req: XattrRequest) -> ResponseKind {
        ResponseKind::RemoveXattr(Self::wire_result(
            self.server
                .direct
                .remove_xattr(
                    request_path(&req.path),
                    &req.name,
                    req.namespace.as_deref(),
                    req.follow,
                )
                .await,
        ))
    }

    async fn handle_streams(&self, req: StreamsRequest) -> ResponseKind {
        ResponseKind::Streams(Self::wire_result(
            self.server
                .direct
                .streams(request_path(&req.path), req.follow)
                .await,
        ))
    }
}

fn wire_error(error: impl Into<crate::Error>) -> crate::protocol::WireError {
    error.into().into()
}

#[cfg(unix)]
fn exit_status_to_raw(status: ExitStatus) -> i32 {
    status.into_raw()
}

#[cfg(windows)]
fn exit_status_to_raw(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}
