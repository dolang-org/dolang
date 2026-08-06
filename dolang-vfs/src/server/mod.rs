use std::{
    result,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[cfg(unix)]
use std::path::{Path, PathBuf};

use dolang_rpc::{CallContext, DefaultHandle, Opaque, OpaqueResource, OsHandle};
use dolang_winterop::SecDesc;
#[cfg(unix)]
use std::os::unix::io::OwnedFd;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream, unix::SocketAddr};
use tokio::sync::{Mutex, watch};
#[cfg(unix)]
use tokio::task::{JoinError, JoinSet};

use crate::{
    AnyFile, AnyVfs, Child as _, Command as _, Direct, Error, ExtContext, FileHandle as _,
    FileLockRequest, FileMarker, OpenOptions as _, PosixAcl, SessionMode, StdioRecv,
    StdioRecvMarker, StdioSend, StdioSendMarker, Utf8TypedPath, Vfs,
    protocol::{
        AccessRequest, AclRequest, CanonicalizeRequest, CopyRequest, CreateDirRequest,
        ExtensionRequest, ExtensionResponse, FsMetadataRequest, GlobRequest, HardLinkRequest,
        MetadataRequest, MoveRequest, OpenHandle, OpenHandlePreference, OpenRequest, OpenVfsHandle,
        PipeResponse, QueryResponse, ReadDirResponse, ReadLinkRequest, RemoveDirRequest,
        RemoveRequest, RenameRequest, Request, RequestKind, ResponseKind, SecDescRequest,
        SetAclRequest, SetMetadataRequest, SetSecDescRequest, SetXattrRequest, SpawnRequest,
        StdioRecvTarget, StdioSendTarget, StreamsRequest, SymlinkKind, SymlinkRequest,
        UnixVfsRequest, VfsProtocol, WellKnownPathRequest, WindowsAdminRequest, WireError,
        WirePath, XattrNamespaceRequest, XattrRequest, XattrsRequest, rpc_builder,
    },
};

fn request_path(path: &WirePath) -> Utf8TypedPath<'_> {
    path.into()
}

#[derive(Clone)]
struct Connection {
    server: Arc<ServerState>,
    mode: SessionMode,
    drain: Arc<Drain>,
}

/// Tracks outstanding stdio endpoints so a stop request can drain them.
///
/// A stop request must not sever the connection while a peer is still relaying
/// through a pipe endpoint it obtained from this session: a stdio relay may
/// outlive the lexical scope of the session it was created in, since pipe
/// negotiation decides which side of a cross-domain pipeline ends up owning it.
/// Instead, a stop marks the session as stopping (so no *new* endpoints can be
/// created) and waits for the endpoints already handed out to be closed.
struct Drain {
    /// Outstanding endpoint count in the upper bits, stopping flag in the LSB.
    state: AtomicUsize,
    done: watch::Sender<bool>,
}

impl Drain {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicUsize::new(0),
            done: watch::channel(false).0,
        })
    }

    /// Reserves `count` endpoints, or fails if the session is stopping.
    ///
    /// The check and the increment are separate non-atomic steps, which is
    /// sound because every handler for a connection runs as a future on the
    /// single serve task: no other handler can observe or modify the state
    /// between them, since there is no await point in between. A stop landing
    /// just after a successful reservation is equivalent to one landing just
    /// before it, and either way the drain waits for those endpoints.
    fn try_acquire(&self, count: usize) -> bool {
        if self.state.load(Ordering::Acquire) & 1 != 0 {
            return false;
        }
        self.state.fetch_add(count << 1, Ordering::Relaxed);
        true
    }

    /// Returns `count` endpoints, completing the drain if it goes idle.
    fn release(&self, count: usize) {
        // The stopping flag plus exactly `count` endpoints means the drain has
        // just gone idle after a stop was requested.
        if self.state.fetch_sub(count << 1, Ordering::AcqRel) == (count << 1) | 1 {
            self.done.send_replace(true);
        }
    }

    /// Marks the session as stopping, completing the drain if it is already idle.
    fn begin_stop(&self) {
        if self.state.fetch_or(1, Ordering::AcqRel) == 0 {
            self.done.send_replace(true);
        }
    }

    /// Waits for a stop to be requested and all outstanding endpoints to close.
    async fn wait(&self) {
        let _ = self.done.subscribe().wait_for(|done| *done).await;
    }
}

struct RetainedVfs {
    vfs: AnyVfs,
    session: Option<crate::VfsSession>,
}

impl RetainedVfs {
    fn plain(vfs: AnyVfs) -> Self {
        Self { vfs, session: None }
    }

    fn session(session: crate::VfsSession) -> Self {
        Self {
            vfs: session.client().clone().into(),
            session: Some(session),
        }
    }
}

impl OpaqueResource for RetainedVfs {
    type Marker = crate::VfsMarker;
}

struct RetainedFile(
    Mutex<AnyFile>,
    std::sync::Mutex<std::collections::HashMap<u64, Arc<tokio::sync::Mutex<crate::FileLock>>>>,
    std::sync::atomic::AtomicU64,
);

impl OpaqueResource for RetainedFile {
    type Marker = FileMarker;
}

struct RetainedStdioSend(Mutex<StdioSend>);

impl OpaqueResource for RetainedStdioSend {
    type Marker = StdioSendMarker;
}

struct RetainedStdioRecv(Mutex<StdioRecv>);

impl OpaqueResource for RetainedStdioRecv {
    type Marker = StdioRecvMarker;
}

struct RetainedChild(Mutex<crate::AnyChild>);

impl OpaqueResource for RetainedChild {
    type Marker = crate::ChildMarker;
}

struct ServerState {
    vfs: AnyVfs,
    #[cfg(unix)]
    shutdown_tx: watch::Sender<()>,
}

/// Agent server that handles VFS RPC requests.
pub struct Server {
    #[cfg(unix)]
    listener: Option<UnixListener>,
    rpc: Option<dolang_rpc::Server<VfsProtocol>>,
    mode: SessionMode,
    shared: Arc<ServerState>,
}

impl Server {
    /// Creates an opaque-only VFS server over a bidirectional byte stream.
    pub async fn new<T>(stream: T) -> crate::Result<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let rpc = rpc_builder()
            .server(stream)
            .await
            .map_err(crate::client::rpc_error)?
            .bind();
        Ok(Self {
            #[cfg(unix)]
            listener: None,
            rpc: Some(rpc),
            mode: SessionMode::Remote,
            shared: Self::state(),
        })
    }

    /// Creates an opaque-only VFS server on separate reader and writer streams.
    pub async fn new_split<R, W>(reader: R, writer: W) -> crate::Result<Self>
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let rpc = rpc_builder()
            .server_split(reader, writer)
            .await
            .map_err(crate::client::rpc_error)?
            .bind();
        Ok(Self {
            #[cfg(unix)]
            listener: None,
            rpc: Some(rpc),
            mode: SessionMode::Remote,
            shared: Self::state(),
        })
    }

    fn state() -> Arc<ServerState> {
        #[cfg(unix)]
        let (shutdown_tx, _) = watch::channel(());
        Arc::new(ServerState {
            vfs: AnyVfs::from(Direct::default()),
            #[cfg(unix)]
            shutdown_tx,
        })
    }

    /// Bind to a socket path and create a server.
    #[cfg(unix)]
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        Ok(Self::from_listener(UnixListener::bind(path)?))
    }

    /// Create a server from an existing `UnixListener`.
    #[cfg(unix)]
    fn from_listener(listener: UnixListener) -> Self {
        Self {
            listener: Some(listener),
            rpc: None,
            mode: SessionMode::Native,
            shared: Self::state(),
        }
    }

    /// Creates a VFS RPC server on the client end of a connected Windows named pipe.
    #[cfg(windows)]
    pub async fn from_named_pipe_client(pipe: NamedPipeClient) -> Result<Self, io::Error> {
        let rpc = rpc_builder()
            .server_named_pipe_client(pipe)
            .await
            .map_err(crate::client::rpc_error)?
            .bind();
        Ok(Self {
            #[cfg(unix)]
            listener: None,
            rpc: Some(rpc),
            mode: SessionMode::Native,
            shared: Self::state(),
        })
    }

    #[cfg(unix)]
    fn handle_accept(
        &self,
        res: io::Result<(UnixStream, SocketAddr)>,
        handlers: &mut JoinSet<Result<(), dolang_rpc::Error>>,
    ) -> Result<(), io::Error> {
        let (stream, _) = res?;
        let stream = stream.into_std()?;
        let connection = Arc::new(Connection {
            server: self.shared.clone(),
            mode: SessionMode::Native,
            drain: Drain::new(),
        });
        handlers.spawn(async move {
            // Negotiation (a real handshake over the wire) happens here,
            // inside the per-connection task, so a slow or misbehaving peer
            // can't stall the accept loop from taking new connections.
            let rpc = rpc_builder().server_unix(stream).await?.bind();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_handler = stop.clone();
            let handler = connection.clone();
            let result = serve_connection(rpc, handler, stop_handler).await;
            if stop.load(Ordering::Acquire) {
                let _ = connection.server.shutdown_tx.send(());
            }
            result
        });
        Ok(())
    }

    /// Accept incoming connections in an infinite loop.
    ///
    /// Each connection spawns a handler task that processes requests.
    #[cfg(unix)]
    pub async fn accept(self) -> Result<(), io::Error> {
        let mut shutdown_rx = self.shared.shutdown_tx.subscribe();
        let mut handlers = JoinSet::new();

        loop {
            tokio::select! {
                res = self.listener.as_ref().unwrap().accept() => {
                    if let Err(error) = self.handle_accept(res, &mut handlers) {
                        eprintln!("VFS server failed to accept a connection: {error}");
                    }
                }
                result = handlers.join_next(), if !handlers.is_empty() => {
                    report_handler_exit(result.unwrap());
                }
                _ = shutdown_rx.changed() => {
                    break;
                }
            }
        }

        while let Some(result) = handlers.join_next().await {
            report_handler_exit(result);
        }
        Ok(())
    }

    /// Serves one connected VFS session.
    pub async fn serve(mut self) -> Result<(), io::Error> {
        let connection = Arc::new(Connection {
            server: self.shared,
            mode: self.mode,
            drain: Drain::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let rpc = self
            .rpc
            .take()
            .expect("server does not own a connected session");
        match serve_connection(rpc, connection, stop).await {
            Ok(()) => Ok(()),
            Err(error) if orderly_disconnect(&error) => Ok(()),
            Err(error) => Err(io::Error::other(error)),
        }
    }
}

#[cfg(unix)]
fn report_handler_exit(result: Result<Result<(), dolang_rpc::Error>, JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) if orderly_disconnect(&error) => {}
        Ok(Err(error)) => eprintln!("VFS connection handler failed: {error}"),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => eprintln!("VFS connection handler task failed: {error}"),
    }
}

fn orderly_disconnect(error: &dolang_rpc::Error) -> bool {
    match error {
        dolang_rpc::Error::ConnectionClosed => true,
        dolang_rpc::Error::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
        ),
        _ => false,
    }
}

async fn serve_connection(
    rpc: dolang_rpc::Server<VfsProtocol>,
    connection: Arc<Connection>,
    stop: Arc<AtomicBool>,
) -> Result<(), dolang_rpc::Error> {
    rpc.serve(async move |mut context, Request { vfs, kind }| {
        let response = if matches!(kind, RequestKind::Stop) {
            connection.handle_stop(&mut context, vfs, &stop).await
        } else if let Err(error) = connection.select(&context, vfs.clone()) {
            ResponseKind::Error(error)
        } else {
            let connection = connection.select(&context, vfs).unwrap();
            match kind {
                RequestKind::Spawn(request) => {
                    connection.handle_spawn_rpc(&mut context, request).await
                }
                RequestKind::ChildWait { child } => {
                    connection.handle_child_wait(&mut context, child).await
                }
                RequestKind::ChildTerminate { child } => {
                    connection.handle_child_terminate(&context, child).await
                }
                RequestKind::ChildClose { child } => connection.handle_child_close(&context, child),
                RequestKind::FileRead { file, len } => {
                    connection.handle_file_read(context, file, len).await;
                    return;
                }
                RequestKind::StdioRecvRead { stdio, len } => {
                    connection.handle_stdio_recv_read(context, stdio, len).await;
                    return;
                }
                RequestKind::Stop => unreachable!(),
                request => connection.handle(&mut context, request).await,
            }
        };
        context.respond(response);
    })
    .await
}

impl Connection {
    fn select(
        &self,
        context: &CallContext<VfsProtocol>,
        vfs: Option<Opaque<crate::VfsMarker>>,
    ) -> Result<Self, WireError> {
        let Some(vfs) = vfs else {
            return Ok(self.clone());
        };
        let selected = context
            .acquire::<RetainedVfs>(vfs.clone())
            .map_err(|_| Self::invalid_opaque("VFS"))?;
        Ok(Self {
            server: Arc::new(ServerState {
                vfs: selected.vfs.clone(),
                #[cfg(unix)]
                shutdown_tx: self.server.shutdown_tx.clone(),
            }),
            mode: self.mode,
            drain: self.drain.clone(),
        })
    }

    async fn handle_stop(
        &self,
        context: &mut CallContext<VfsProtocol>,
        vfs: Option<Opaque<crate::VfsMarker>>,
        stop: &AtomicBool,
    ) -> ResponseKind {
        let Some(vfs) = vfs else {
            stop.store(true, Ordering::Release);
            // Reject new stdio endpoints, then keep serving reads, writes and
            // closes on the ones already handed out until they are all closed.
            // The rpc serve loop polls request handlers on the same task as it
            // reads frames, so awaiting here does not stall the connection.
            self.drain.begin_stop();
            self.drain.wait().await;
            context.shutdown();
            return ResponseKind::Stop;
        };
        let retained = match context.acquire::<RetainedVfs>(vfs.clone()) {
            Ok(retained) => retained,
            Err(_) => return ResponseKind::Error(Self::invalid_opaque("VFS")),
        };
        let client = retained.vfs.as_client().cloned();
        drop(retained);
        let retained = context.unregister::<RetainedVfs>(vfs).ok().flatten();
        if let Some(session) = retained.and_then(|retained| retained.session) {
            return match session.stop().await {
                Ok(()) => ResponseKind::Stop,
                Err(error) => ResponseKind::Error(wire_error(error)),
            };
        }
        let Some(client) = client else {
            return ResponseKind::Error(Self::invalid_opaque("VFS"));
        };
        match client.stop().await {
            Ok(()) => ResponseKind::Stop,
            Err(error) => ResponseKind::Error(wire_error(error)),
        }
    }

    fn unsupported(operation: &str) -> WireError {
        Error::new(
            crate::ErrorKind::Unsupported,
            format!("{operation} is not supported by a remote VFS session"),
        )
        .into()
    }

    fn invalid_opaque(kind: &str) -> WireError {
        Error::new(
            crate::ErrorKind::InvalidInput,
            format!("invalid opaque {kind}"),
        )
        .into()
    }

    fn wire_result<T, E>(result: result::Result<T, E>) -> result::Result<T, WireError>
    where
        E: Into<Error>,
    {
        result.map_err(wire_error)
    }

    async fn handle(
        &self,
        context: &mut CallContext<VfsProtocol>,
        kind: RequestKind,
    ) -> ResponseKind {
        match kind {
            RequestKind::Query => self.handle_query().await,
            RequestKind::UserName { uid } => {
                ResponseKind::UserName(Self::wire_result(self.server.vfs.user_name(uid).await))
            }
            RequestKind::UserId { name } => {
                ResponseKind::UserId(Self::wire_result(self.server.vfs.user_id(&name).await))
            }
            RequestKind::GroupName { gid } => {
                ResponseKind::GroupName(Self::wire_result(self.server.vfs.group_name(gid).await))
            }
            RequestKind::GroupId { name } => {
                ResponseKind::GroupId(Self::wire_result(self.server.vfs.group_id(&name).await))
            }
            RequestKind::SidName { sid } => {
                ResponseKind::SidName(Self::wire_result(self.server.vfs.sid_name(&sid).await))
            }
            RequestKind::AccountName { name } => ResponseKind::AccountName(Self::wire_result(
                self.server.vfs.account_name(&name).await,
            )),
            RequestKind::Which { program, path, cwd } => {
                self.handle_which(program, path, cwd).await
            }
            RequestKind::WellKnownPath(request) => self.handle_well_known_path(request).await,
            RequestKind::Stop
            | RequestKind::Spawn(_)
            | RequestKind::ChildWait { .. }
            | RequestKind::ChildTerminate { .. }
            | RequestKind::ChildClose { .. } => unreachable!(),
            RequestKind::ClearCache => {
                ResponseKind::ClearCache(Self::wire_result(self.server.vfs.clear_cache().await))
            }
            RequestKind::Pipe => self.handle_pipe(context).await,
            RequestKind::Open(request) => self.handle_open(context, request).await,
            RequestKind::FileRead { .. } => unreachable!(),
            RequestKind::FileWrite { file } => self.handle_file_write(context, file).await,
            RequestKind::FileSeek { file, position } => {
                self.handle_file_seek(context, file, position.into()).await
            }
            RequestKind::FileFlush { file } => self.handle_file_flush(context, file).await,
            RequestKind::FileSetSize { file, size } => {
                self.handle_file_set_size(context, file, size).await
            }
            RequestKind::FileLock { file, request } => {
                self.handle_file_lock(context, file, request).await
            }
            RequestKind::FileUnlock { file, lock } => {
                self.handle_file_unlock(context, file, lock).await
            }
            RequestKind::FileToStdioSend { file } => {
                self.handle_file_to_stdio_send(context, file).await
            }
            RequestKind::FileToStdioRecv { file } => {
                self.handle_file_to_stdio_recv(context, file).await
            }
            RequestKind::StdioSendClose { stdio } => {
                ResponseKind::StdioSendClose(self.close_stdio_send(context, stdio))
            }
            RequestKind::StdioSendWrite { stdio } => {
                self.handle_stdio_send_write(context, stdio).await
            }
            RequestKind::StdioSendClone { stdio } => {
                self.handle_stdio_send_clone(context, stdio).await
            }
            RequestKind::StdioRecvClose { stdio } => {
                ResponseKind::StdioRecvClose(self.close_stdio_recv(context, stdio))
            }
            RequestKind::StdioRecvRead { .. } => unreachable!(),
            RequestKind::StdioRecvClone { stdio } => {
                self.handle_stdio_recv_clone(context, stdio).await
            }
            RequestKind::FileMetadata { file } => self.handle_file_metadata(context, file).await,
            RequestKind::FileFsMetadata { file } => {
                self.handle_file_fs_metadata(context, file).await
            }
            RequestKind::FileAcl { file, default } => {
                self.handle_file_acl(context, file, default).await
            }
            RequestKind::FileSetAcl { file, acl, default } => {
                self.handle_file_set_acl(context, file, acl, default).await
            }
            RequestKind::FileSecDesc { file, mask } => {
                self.handle_file_sec_desc(context, file, mask).await
            }
            RequestKind::FileSetSecDesc { file, sec_desc } => {
                self.handle_file_set_sec_desc(context, file, sec_desc).await
            }
            RequestKind::FileXattrs { file, namespace } => {
                self.handle_file_xattrs(context, file, namespace).await
            }
            RequestKind::FileXattr {
                file,
                name,
                namespace,
            } => self.handle_file_xattr(context, file, name, namespace).await,
            RequestKind::FileStreams { file } => self.handle_file_streams(context, file).await,
            RequestKind::FileSetXattr {
                file,
                name,
                namespace,
                value,
            } => {
                self.handle_file_set_xattr(context, file, name, namespace, value)
                    .await
            }
            RequestKind::FileRemoveXattr {
                file,
                name,
                namespace,
            } => {
                self.handle_file_remove_xattr(context, file, name, namespace)
                    .await
            }
            RequestKind::FileClose { file } => self.handle_file_close(context, file).await,
            RequestKind::UnixVfs(request) => self.handle_unix_vfs(context, request).await,
            RequestKind::WindowsAdmin(request) => self.handle_windows_admin(context, request).await,
            RequestKind::ReadDir { path } => self.handle_read_dir(path).await,
            RequestKind::Remove(request) => self.handle_remove(request).await,
            RequestKind::Metadata(request) => self.handle_metadata(request).await,
            RequestKind::FsMetadata(request) => self.handle_fs_metadata(request).await,
            RequestKind::Acl(request) => self.handle_acl(request).await,
            RequestKind::SetAcl(request) => self.handle_set_acl(request).await,
            RequestKind::SecDesc(request) => self.handle_sec_desc(request).await,
            RequestKind::SetSecDesc(request) => self.handle_set_sec_desc(request).await,
            RequestKind::CreateDir(request) => self.handle_create_dir(request).await,
            RequestKind::RemoveDir(request) => self.handle_remove_dir(request).await,
            RequestKind::Copy(request) => self.handle_copy(request).await,
            RequestKind::Rename(request) => self.handle_rename(request).await,
            RequestKind::Move(request) => self.handle_move(request).await,
            RequestKind::Symlink(request) => self.handle_symlink(request).await,
            RequestKind::HardLink(request) => self.handle_hard_link(request).await,
            RequestKind::SymlinkMetadata(request) => self.handle_symlink_metadata(request).await,
            RequestKind::SetMetadata(request) => self.handle_set_metadata(request).await,
            RequestKind::Canonicalize(request) => self.handle_canonicalize(request).await,
            RequestKind::ReadLink(request) => self.handle_read_link(request).await,
            RequestKind::Access(request) => self.handle_access(request).await,
            RequestKind::Glob(request) => self.handle_glob(request).await,
            RequestKind::Xattrs(request) => self.handle_xattrs(request).await,
            RequestKind::Xattr(request) => self.handle_xattr(request).await,
            RequestKind::SetXattr(request) => self.handle_set_xattr(request).await,
            RequestKind::RemoveXattr(request) => self.handle_remove_xattr(request).await,
            RequestKind::Streams(request) => self.handle_streams(request).await,
            RequestKind::Extension(request) => self.handle_extension(context, request).await,
        }
    }

    async fn handle_extension(
        &self,
        context: &mut CallContext<VfsProtocol>,
        request: ExtensionRequest,
    ) -> ResponseKind {
        let ExtensionRequest {
            name,
            version,
            payload,
        } = request;
        let Some(ext) = crate::extension::lookup(&name, version) else {
            return ResponseKind::Extension(Err(Self::unsupported(&format!(
                "VFS extension {name} v{version}"
            ))));
        };
        let mut ctx = ExtContext::remote(context, self.mode == SessionMode::Native);
        let payload = ext.dispatch(&mut ctx, payload).await;
        ResponseKind::Extension(Ok(ExtensionResponse {
            name,
            version,
            payload,
        }))
    }

    async fn handle_which(
        &self,
        program: WirePath,
        path: Option<String>,
        cwd: Option<WirePath>,
    ) -> ResponseKind {
        let resolved = self
            .server
            .vfs
            .which(
                request_path(&program),
                path.as_deref(),
                cwd.as_ref().map(request_path),
            )
            .await;

        ResponseKind::Which(
            resolved
                .map(|path| path.map(Into::into))
                .map_err(wire_error),
        )
    }

    async fn handle_well_known_path(&self, req: WellKnownPathRequest) -> ResponseKind {
        let result = self
            .server
            .vfs
            .well_known_path(req.key, req.app.as_deref(), &req.env)
            .await;
        ResponseKind::WellKnownPath(result.map(Into::into).map_err(wire_error))
    }

    async fn handle_spawn_rpc(
        &self,
        context: &mut CallContext<VfsProtocol>,
        req: SpawnRequest,
    ) -> ResponseKind {
        let mut cmd = self.server.vfs.command(request_path(&req.program));
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
        cmd.process_control(req.process_control);
        cmd.termination_policy(req.termination_policy);

        if let Err(error) = self
            .configure_spawn_stdio(context, &mut cmd, req.stdin, req.stdout, req.stderr)
            .await
        {
            return ResponseKind::Spawn(Err(error));
        }

        let child = match cmd.spawn().await {
            Ok(child) => child,
            Err(e) => {
                return ResponseKind::Spawn(Err(wire_error(e)));
            }
        };
        ResponseKind::Spawn(Ok(context.register(RetainedChild(Mutex::new(child)))))
    }

    fn spawn_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        target: StdioRecvTarget,
    ) -> Result<Option<StdioRecv>, WireError> {
        match target {
            StdioRecvTarget::Null => Ok(None),
            StdioRecvTarget::Native(handle) => {
                if self.mode == SessionMode::Remote {
                    return Err(Self::unsupported("native process stdio"));
                }
                Ok(Some(StdioRecv::from_file(tokio::fs::File::from_std(
                    handle.into_inner().into(),
                ))))
            }
            StdioRecvTarget::Opaque(stdio) => {
                let stdio = context
                    .unregister::<RetainedStdioRecv>(stdio)
                    .map_err(|_| Self::invalid_opaque("stdio receive"))?;
                self.drain.release(1);
                let Some(stdio) = stdio else {
                    return Err(wire_error(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        "opaque stdio receive is in use",
                    )));
                };
                Ok(Some(stdio.0.into_inner()))
            }
        }
    }

    fn spawn_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        target: StdioSendTarget,
    ) -> Result<Option<StdioSend>, WireError> {
        match target {
            StdioSendTarget::Null => Ok(None),
            StdioSendTarget::Native(handle) => {
                if self.mode == SessionMode::Remote {
                    return Err(Self::unsupported("native process stdio"));
                }
                Ok(Some(StdioSend::from_file(tokio::fs::File::from_std(
                    handle.into_inner().into(),
                ))))
            }
            StdioSendTarget::Opaque(stdio) => {
                let stdio = context
                    .unregister::<RetainedStdioSend>(stdio)
                    .map_err(|_| Self::invalid_opaque("stdio send"))?;
                self.drain.release(1);
                let Some(stdio) = stdio else {
                    return Err(wire_error(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        "opaque stdio send is in use",
                    )));
                };
                Ok(Some(stdio.0.into_inner()))
            }
        }
    }

    async fn configure_spawn_stdio(
        &self,
        context: &CallContext<VfsProtocol>,
        command: &mut crate::AnyCommand<'_>,
        stdin: StdioRecvTarget,
        stdout: StdioSendTarget,
        stderr: StdioSendTarget,
    ) -> Result<(), WireError> {
        let stdin = self.spawn_stdio_recv(context, stdin);
        let stdout = self.spawn_stdio_send(context, stdout);
        let stderr = self.spawn_stdio_send(context, stderr);
        let (stdin, stdout, stderr) = (stdin?, stdout?, stderr?);

        if let Some(stdio) = stdin {
            command.stdin(stdio).map_err(wire_error)?;
        } else {
            command.stdin_null();
        }
        if let Some(stdio) = stdout {
            command.stdout(stdio).map_err(wire_error)?;
        } else {
            command.stdout_null();
        }
        if let Some(stdio) = stderr {
            command.stderr(stdio).map_err(wire_error)?;
        } else {
            command.stderr_null();
        }
        Ok(())
    }

    fn take_child(
        &self,
        context: &CallContext<VfsProtocol>,
        child: Opaque<crate::ChildMarker>,
    ) -> Result<RetainedChild, WireError> {
        context
            .unregister::<RetainedChild>(child)
            .map_err(|_| {
                wire_error(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid opaque child",
                ))
            })?
            .ok_or_else(|| {
                wire_error(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "opaque child is in use",
                ))
            })
    }

    async fn handle_child_wait(
        &self,
        context: &mut CallContext<VfsProtocol>,
        child: Opaque<crate::ChildMarker>,
    ) -> ResponseKind {
        let result = match self.take_child(context, child) {
            Ok(child) => {
                let mut child = child.0.into_inner();
                match context.cancel_guard(async |_| child.wait().await).await {
                    Ok(result) => result,
                    Err(_) => child.terminate().await.and_then(|status| {
                        status.ok_or_else(|| {
                            io::Error::other("process was orphaned during cancelled wait").into()
                        })
                    }),
                }
                .map_err(wire_error)
            }
            Err(error) => Err(error),
        };
        ResponseKind::ChildWait(result)
    }

    async fn handle_child_terminate(
        &self,
        context: &CallContext<VfsProtocol>,
        child: Opaque<crate::ChildMarker>,
    ) -> ResponseKind {
        let result = match self.take_child(context, child) {
            Ok(child) => child.0.into_inner().terminate().await.map_err(wire_error),
            Err(error) => Err(error),
        };
        ResponseKind::ChildTerminate(result)
    }

    fn handle_child_close(
        &self,
        context: &CallContext<VfsProtocol>,
        child: Opaque<crate::ChildMarker>,
    ) -> ResponseKind {
        let result = context
            .unregister::<RetainedChild>(child)
            .map(|_| ())
            .map_err(|_| {
                wire_error(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid opaque child",
                ))
            });
        ResponseKind::ChildClose(result)
    }

    async fn handle_query(&self) -> ResponseKind {
        ResponseKind::Query(Self::wire_result(self.server.vfs.query().await.map(
            |query| QueryResponse {
                env: query.env,
                cwd: query.cwd.into(),
                current_exe: query.current_exe.into(),
                target: query.target,
                security: query.security,
            },
        )))
    }

    /// Reserves a drain slot for an endpoint about to be handed to the peer.
    ///
    /// Only endpoint creation is gated this way. `Spawn` and `Open` stay
    /// available while stopping: they create no stdio endpoint of their own,
    /// and refusing a spawn could break the very in-flight pipeline stage the
    /// drain exists to protect.
    fn reserve_stdio(&self, count: usize) -> Result<(), WireError> {
        if self.drain.try_acquire(count) {
            Ok(())
        } else {
            Err(wire_error(io::Error::new(
                io::ErrorKind::NotConnected,
                "VFS session is stopping",
            )))
        }
    }

    async fn handle_pipe(&self, context: &CallContext<VfsProtocol>) -> ResponseKind {
        let result = async {
            let (send, recv) = self.server.vfs.pipe().await.map_err(wire_error)?;
            self.reserve_stdio(2)?;
            Ok(PipeResponse {
                send: context.register(RetainedStdioSend(Mutex::new(send))),
                recv: context.register(RetainedStdioRecv(Mutex::new(recv))),
            })
        }
        .await;
        ResponseKind::Pipe(result)
    }

    fn retained_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Opaque<StdioSendMarker>,
    ) -> Result<dolang_rpc::OpaqueGuard<RetainedStdioSend>, WireError> {
        context
            .acquire::<RetainedStdioSend>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio send"))
    }

    fn retained_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Opaque<StdioRecvMarker>,
    ) -> Result<dolang_rpc::OpaqueGuard<RetainedStdioRecv>, WireError> {
        context
            .acquire::<RetainedStdioRecv>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio receive"))
    }

    fn close_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Opaque<StdioSendMarker>,
    ) -> Result<(), WireError> {
        let retained = self.retained_stdio_send(context, stdio.clone())?;
        drop(retained);
        let retained = context
            .unregister::<RetainedStdioSend>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio send"))?;
        // The endpoint is out of the object table either way, so it no longer
        // holds up a drain. A racing read or write can still be holding the
        // last reference, but a peer doing that while closing has no flush
        // guarantee to lose.
        self.drain.release(1);
        match retained {
            Some(_) => Ok(()),
            None => Err(wire_error(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "opaque stdio send is in use",
            ))),
        }
    }

    fn close_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Opaque<StdioRecvMarker>,
    ) -> Result<(), WireError> {
        let retained = self.retained_stdio_recv(context, stdio.clone())?;
        drop(retained);
        let retained = context
            .unregister::<RetainedStdioRecv>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio receive"))?;
        // The endpoint is out of the object table either way, so it no longer
        // holds up a drain. A racing read or write can still be holding the
        // last reference, but a peer doing that while closing has no flush
        // guarantee to lose.
        self.drain.release(1);
        match retained {
            Some(_) => Ok(()),
            None => Err(wire_error(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "opaque stdio receive is in use",
            ))),
        }
    }

    async fn handle_stdio_send_write(
        &self,
        context: &mut CallContext<VfsProtocol>,
        stdio: Opaque<StdioSendMarker>,
    ) -> ResponseKind {
        let result = async {
            let stdio = self.retained_stdio_send(context, stdio)?;
            let trailer = context.request_trailer().ok_or_else(|| {
                wire_error(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stdio write request is missing its data trailer",
                ))
            })?;
            let len = io::copy(trailer, &mut *stdio.0.lock().await)
                .await
                .map_err(wire_error)?;
            usize::try_from(len).map_err(|_| {
                wire_error(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stdio trailer length does not fit in usize",
                ))
            })
        }
        .await;
        ResponseKind::StdioSendWrite(result)
    }

    async fn handle_stdio_send_clone(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Opaque<StdioSendMarker>,
    ) -> ResponseKind {
        let result = async {
            let stdio = self.retained_stdio_send(context, stdio)?;
            let clone = stdio.0.lock().await.try_clone().await.map_err(wire_error)?;
            self.reserve_stdio(1)?;
            Ok(context.register(RetainedStdioSend(Mutex::new(clone))))
        }
        .await;
        ResponseKind::StdioSendClone(result)
    }

    async fn handle_stdio_recv_read(
        &self,
        context: CallContext<VfsProtocol>,
        stdio: Opaque<StdioRecvMarker>,
        len: usize,
    ) {
        let stdio = match self.retained_stdio_recv(&context, stdio) {
            Ok(stdio) => stdio,
            Err(error) => {
                context.respond(ResponseKind::StdioRecvRead(Err(error)));
                return;
            }
        };
        let mut send = context.respond_with_trailer(ResponseKind::StdioRecvRead(Ok(())));
        let mut stdio = stdio.0.lock().await;
        let mut source = (&mut *stdio).take(len as u64);
        if io::copy(&mut source, &mut send).await.is_ok() {
            send.finish();
        }
    }

    async fn handle_stdio_recv_clone(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Opaque<StdioRecvMarker>,
    ) -> ResponseKind {
        let result = async {
            let stdio = self.retained_stdio_recv(context, stdio)?;
            let clone = stdio.0.lock().await.try_clone().await.map_err(wire_error)?;
            self.reserve_stdio(1)?;
            Ok(context.register(RetainedStdioRecv(Mutex::new(clone))))
        }
        .await;
        ResponseKind::StdioRecvClone(result)
    }

    async fn handle_open(
        &self,
        context: &CallContext<VfsProtocol>,
        req: OpenRequest,
    ) -> ResponseKind {
        let mut opts = self.server.vfs.open_options();
        opts.read(req.read)
            .write(req.write)
            .append(req.append)
            .create(req.create)
            .create_new(req.create_new)
            .truncate(req.truncate)
            .no_follow(req.no_follow);

        match opts.open(request_path(&req.path)).await {
            Ok(file) => {
                if self.mode == SessionMode::Remote
                    || matches!(req.handle_preference, OpenHandlePreference::Opaque)
                {
                    let file = context.register(RetainedFile(
                        Mutex::new(file),
                        std::sync::Mutex::new(std::collections::HashMap::new()),
                        std::sync::atomic::AtomicU64::new(0),
                    ));
                    ResponseKind::Open(Ok(OpenHandle::Opaque(file)))
                } else {
                    let handle: DefaultHandle = file.try_into_std().await.unwrap().into();
                    ResponseKind::Open(Ok(OpenHandle::Native(OsHandle::new(handle))))
                }
            }
            Err(e) => ResponseKind::Open(Err(wire_error(e))),
        }
    }

    fn retained_file(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> Result<dolang_rpc::OpaqueGuard<RetainedFile>, WireError> {
        context.acquire::<RetainedFile>(file).map_err(|_| {
            wire_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid opaque file",
            ))
        })
    }

    async fn handle_file_read(
        &self,
        context: CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        len: usize,
    ) {
        let file = match self.retained_file(&context, file) {
            Ok(file) => file,
            Err(error) => {
                context.respond(ResponseKind::FileRead(Err(error)));
                return;
            }
        };
        let mut send = context.respond_with_trailer(ResponseKind::FileRead(Ok(())));
        let mut file = file.0.lock().await;
        let mut source = (&mut *file).take(len as u64);
        if io::copy(&mut source, &mut send).await.is_ok() {
            send.finish();
        }
    }

    async fn handle_file_write(
        &self,
        context: &mut CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            let trailer = context.request_trailer().ok_or_else(|| {
                wire_error(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file write request is missing its data trailer",
                ))
            })?;
            let len = io::copy(trailer, &mut *file.0.lock().await)
                .await
                .map_err(wire_error)?;
            usize::try_from(len).map_err(|_| {
                wire_error(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file trailer length does not fit in usize",
                ))
            })
        }
        .await;
        ResponseKind::FileWrite(result)
    }

    async fn handle_file_seek(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        position: io::SeekFrom,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.seek(position).await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileSeek(result)
    }

    async fn handle_file_flush(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.flush().await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileFlush(result)
    }

    async fn handle_file_set_size(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        size: u64,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.set_size(size).await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileSetSize(result)
    }

    async fn handle_file_to_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            let stdio = file
                .0
                .lock()
                .await
                .to_stdio_send()
                .await
                .map_err(wire_error)?;
            self.reserve_stdio(1)?;
            Ok(context.register(RetainedStdioSend(Mutex::new(stdio))))
        }
        .await;
        ResponseKind::FileToStdioSend(result)
    }

    async fn handle_file_to_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            let stdio = file
                .0
                .lock()
                .await
                .to_stdio_recv()
                .await
                .map_err(wire_error)?;
            self.reserve_stdio(1)?;
            Ok(context.register(RetainedStdioRecv(Mutex::new(stdio))))
        }
        .await;
        ResponseKind::FileToStdioRecv(result)
    }

    async fn handle_file_metadata(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.metadata().await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileMetadata(result)
    }

    async fn handle_file_fs_metadata(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.fs_metadata().await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileFsMetadata(result)
    }

    async fn handle_file_sec_desc(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        mask: u32,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.sec_desc(mask).await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileSecDesc(result)
    }

    async fn handle_file_acl(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        default: bool,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.acl(default).await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileAcl(result)
    }

    async fn handle_file_set_acl(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        acl: Option<PosixAcl>,
        default: bool,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0
                .lock()
                .await
                .set_acl(acl.as_ref(), default)
                .await
                .map_err(wire_error)
        }
        .await;
        ResponseKind::FileSetAcl(result)
    }

    async fn handle_file_set_sec_desc(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        sec_desc: SecDesc,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0
                .lock()
                .await
                .set_sec_desc(&sec_desc)
                .await
                .map_err(wire_error)
        }
        .await;
        ResponseKind::FileSetSecDesc(result)
    }

    async fn handle_file_xattrs(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        namespace: XattrNamespaceRequest,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0
                .lock()
                .await
                .xattrs(namespace.as_borrowed())
                .await
                .map_err(wire_error)
        }
        .await;
        ResponseKind::FileXattrs(result)
    }

    async fn handle_file_xattr(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        name: String,
        namespace: Option<String>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0
                .lock()
                .await
                .xattr(&name, namespace.as_deref())
                .await
                .map_err(wire_error)
        }
        .await;
        ResponseKind::FileXattr(result)
    }

    async fn handle_file_streams(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0.lock().await.streams().await.map_err(wire_error)
        }
        .await;
        ResponseKind::FileStreams(result)
    }

    async fn handle_file_set_xattr(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        name: String,
        namespace: Option<String>,
        value: Vec<u8>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0
                .lock()
                .await
                .set_xattr(&name, namespace.as_deref(), &value)
                .await
                .map_err(wire_error)
        }
        .await;
        ResponseKind::FileSetXattr(result)
    }

    async fn handle_file_remove_xattr(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        name: String,
        namespace: Option<String>,
    ) -> ResponseKind {
        let result = async {
            let file = self.retained_file(context, file)?;
            file.0
                .lock()
                .await
                .remove_xattr(&name, namespace.as_deref())
                .await
                .map_err(wire_error)
        }
        .await;
        ResponseKind::FileRemoveXattr(result)
    }

    async fn handle_file_lock(
        &self,
        context: &mut CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        request: FileLockRequest,
    ) -> ResponseKind {
        let retained = match self.retained_file(context, file) {
            Ok(retained) => retained,
            Err(error) => return ResponseKind::FileLock(Err(error)),
        };
        let acquired = context
            .cancel_guard(async |_context| retained.0.lock().await.lock(request).await)
            .await;
        let acquired = match acquired {
            Ok(result) => match result {
                Ok(acquired) => acquired,
                Err(error) => return ResponseKind::FileLock(Err(wire_error(error))),
            },
            Err(_) => {
                return ResponseKind::FileLock(Err(wire_error(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "file lock acquisition was cancelled",
                ))));
            }
        };
        let lock = acquired.map(|lock| {
            let id = retained.2.fetch_add(1, Ordering::Relaxed);
            retained
                .1
                .lock()
                .unwrap()
                .insert(id, Arc::new(tokio::sync::Mutex::new(lock)));
            id
        });
        ResponseKind::FileLock(Ok(lock))
    }

    async fn handle_file_unlock(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
        lock: u64,
    ) -> ResponseKind {
        let retained = match self.retained_file(context, file) {
            Ok(retained) => retained,
            Err(_) => return ResponseKind::FileUnlock(Ok(())),
        };
        let retained_lock = retained.1.lock().unwrap().get(&lock).cloned();
        let result = match retained_lock {
            Some(retained_lock) => retained_lock
                .lock()
                .await
                .release()
                .await
                .map_err(wire_error),
            None => Ok(()),
        };
        if result.is_ok() {
            retained.1.lock().unwrap().remove(&lock);
        }
        ResponseKind::FileUnlock(result)
    }

    async fn handle_file_close(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Opaque<FileMarker>,
    ) -> ResponseKind {
        let retained = match self.retained_file(context, file.clone()) {
            Ok(retained) => retained,
            Err(error) => return ResponseKind::FileClose(Err(error)),
        };
        drop(retained);
        let result = match context.unregister::<RetainedFile>(file) {
            Ok(Some(file)) => {
                let locks = file.1.into_inner().unwrap().into_values();
                let mut lock_error = None;
                for lock in locks {
                    if let Err(error) = lock.lock().await.release().await
                        && lock_error.is_none()
                    {
                        lock_error = Some(error);
                    }
                }
                match lock_error {
                    Some(error) => Err(wire_error(error)),
                    None => file.0.into_inner().close().await.map_err(wire_error),
                }
            }
            Ok(None) => Err(wire_error(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "opaque file is in use",
            ))),
            Err(_) => Err(wire_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid opaque file",
            ))),
        };
        ResponseKind::FileClose(result)
    }

    async fn handle_read_dir(&self, path: WirePath) -> ResponseKind {
        let result: crate::Result<ReadDirResponse> = async {
            let mut read_dir = self.server.vfs.read_dir(request_path(&path)).await?;
            let mut entries = Vec::new();
            while let Some(entry) = read_dir.next_entry().await? {
                entries.push(entry);
            }
            Ok(ReadDirResponse { entries })
        }
        .await;
        ResponseKind::ReadDir(Self::wire_result(result))
    }

    async fn handle_unix_vfs(
        &self,
        context: &CallContext<VfsProtocol>,
        req: UnixVfsRequest,
    ) -> ResponseKind {
        #[cfg(unix)]
        if self.mode == SessionMode::Native && matches!(self.server.vfs, AnyVfs::Direct(_)) {
            let result: crate::Result<OwnedFd> = async {
                let path = crate::native_path(request_path(&req.path))?;
                let stream = UnixStream::connect(path).await?;
                Ok(stream.into_std()?.into())
            }
            .await;
            return ResponseKind::UnixVfs(
                result
                    .map(|handle| OpenVfsHandle::Native(OsHandle::new(handle)))
                    .map_err(wire_error),
            );
        }

        ResponseKind::UnixVfs(
            self.server
                .vfs
                .unix_socket(request_path(&req.path))
                .await
                .map(|vfs| OpenVfsHandle::Opaque(context.register(RetainedVfs::plain(vfs))))
                .map_err(wire_error),
        )
    }

    async fn handle_windows_admin(
        &self,
        context: &CallContext<VfsProtocol>,
        req: WindowsAdminRequest,
    ) -> ResponseKind {
        ResponseKind::WindowsAdmin(
            self.server
                .vfs
                .windows_admin(request_path(&req.cwd), req.env, req.elevate)
                .await
                .map(|session| context.register(RetainedVfs::session(session)))
                .map_err(wire_error),
        )
    }

    async fn handle_remove(&self, req: RemoveRequest) -> ResponseKind {
        ResponseKind::Remove(Self::wire_result(
            self.server
                .vfs
                .remove(request_path(&req.path), req.all, req.ignore)
                .await,
        ))
    }

    async fn handle_metadata(&self, req: MetadataRequest) -> ResponseKind {
        ResponseKind::Metadata(Self::wire_result(
            self.server.vfs.metadata(request_path(&req.path)).await,
        ))
    }

    async fn handle_fs_metadata(&self, req: FsMetadataRequest) -> ResponseKind {
        ResponseKind::FsMetadata(Self::wire_result(
            self.server
                .vfs
                .fs_metadata(request_path(&req.path), req.follow)
                .await,
        ))
    }

    async fn handle_sec_desc(&self, req: SecDescRequest) -> ResponseKind {
        ResponseKind::SecDesc(Self::wire_result(
            self.server
                .vfs
                .sec_desc(request_path(&req.path), req.mask, req.follow)
                .await,
        ))
    }

    async fn handle_acl(&self, req: AclRequest) -> ResponseKind {
        ResponseKind::Acl(Self::wire_result(
            self.server
                .vfs
                .acl(request_path(&req.path), req.default, req.follow)
                .await,
        ))
    }

    async fn handle_set_acl(&self, req: SetAclRequest) -> ResponseKind {
        ResponseKind::SetAcl(Self::wire_result(
            self.server
                .vfs
                .set_acl(
                    request_path(&req.path),
                    req.acl.as_ref(),
                    req.default,
                    req.follow,
                )
                .await,
        ))
    }

    async fn handle_set_sec_desc(&self, req: SetSecDescRequest) -> ResponseKind {
        ResponseKind::SetSecDesc(Self::wire_result(
            self.server
                .vfs
                .set_sec_desc(request_path(&req.path), &req.sec_desc, req.follow)
                .await,
        ))
    }

    async fn handle_create_dir(&self, req: CreateDirRequest) -> ResponseKind {
        ResponseKind::CreateDir(Self::wire_result(
            self.server
                .vfs
                .create_dir(request_path(&req.path), req.all)
                .await,
        ))
    }

    async fn handle_remove_dir(&self, req: RemoveDirRequest) -> ResponseKind {
        ResponseKind::RemoveDir(Self::wire_result(
            self.server
                .vfs
                .remove_dir(request_path(&req.path), req.all, req.ignore)
                .await,
        ))
    }

    async fn handle_copy(&self, req: CopyRequest) -> ResponseKind {
        ResponseKind::Copy(Self::wire_result(
            self.server
                .vfs
                .copy(request_path(&req.from), request_path(&req.to), req.all)
                .await,
        ))
    }

    async fn handle_rename(&self, req: RenameRequest) -> ResponseKind {
        ResponseKind::Rename(Self::wire_result(
            self.server
                .vfs
                .rename(request_path(&req.from), request_path(&req.to), req.replace)
                .await,
        ))
    }

    async fn handle_move(&self, req: MoveRequest) -> ResponseKind {
        ResponseKind::Move(Self::wire_result(
            self.server
                .vfs
                .move_(request_path(&req.from), request_path(&req.to), req.all)
                .await,
        ))
    }

    async fn handle_symlink(&self, req: SymlinkRequest) -> ResponseKind {
        let result = match req.kind {
            SymlinkKind::Infer => {
                self.server
                    .vfs
                    .symlink(
                        request_path(&req.cwd),
                        request_path(&req.src),
                        request_path(&req.dst),
                    )
                    .await
            }
            SymlinkKind::Dir => {
                self.server
                    .vfs
                    .symlink_dir(request_path(&req.src), request_path(&req.dst))
                    .await
            }
            SymlinkKind::File => {
                self.server
                    .vfs
                    .symlink_file(request_path(&req.src), request_path(&req.dst))
                    .await
            }
        };
        ResponseKind::Symlink(Self::wire_result(result))
    }

    async fn handle_hard_link(&self, req: HardLinkRequest) -> ResponseKind {
        ResponseKind::HardLink(Self::wire_result(
            self.server
                .vfs
                .hard_link(request_path(&req.src), request_path(&req.dst))
                .await,
        ))
    }

    async fn handle_symlink_metadata(&self, req: MetadataRequest) -> ResponseKind {
        ResponseKind::SymlinkMetadata(Self::wire_result(
            self.server
                .vfs
                .symlink_metadata(request_path(&req.path))
                .await,
        ))
    }

    async fn handle_set_metadata(&self, req: SetMetadataRequest) -> ResponseKind {
        let paths: Vec<_> = req
            .paths
            .iter()
            .map(|path| request_path(path).to_path_buf())
            .collect();
        ResponseKind::SetMetadata(Self::wire_result(
            self.server.vfs.set_metadata(&paths, req.patch).await,
        ))
    }

    async fn handle_canonicalize(&self, req: CanonicalizeRequest) -> ResponseKind {
        let result = self
            .server
            .vfs
            .canonicalize(request_path(&req.path))
            .await
            .map(Into::into);
        ResponseKind::Canonicalize(Self::wire_result(result))
    }

    async fn handle_read_link(&self, req: ReadLinkRequest) -> ResponseKind {
        let result = self
            .server
            .vfs
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
                    return ResponseKind::Access(
                        Err(Error::from_raw_os_error(libc::EINVAL).into()),
                    );
                }
            };
            let flags = AccessFlags::from_bits(mode).unwrap_or(AccessFlags::empty());
            match access(&path, flags) {
                Ok(()) => ResponseKind::Access(Ok(())),
                Err(e) => ResponseKind::Access(Err(Error::from_raw_os_error(e as i32).into())),
            }
        })
        .await
        .unwrap_or_else(|_| ResponseKind::Access(Err(Error::from_raw_os_error(libc::EIO).into())))
    }

    #[cfg(not(unix))]
    async fn handle_access(&self, _req: AccessRequest) -> ResponseKind {
        ResponseKind::Access(Err(Error::new(
            crate::ErrorKind::Unsupported,
            "POSIX access checks are not supported on this platform",
        )
        .into()))
    }

    async fn handle_glob(&self, req: GlobRequest) -> ResponseKind {
        ResponseKind::Glob(Self::wire_result(
            self.server
                .vfs
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

    async fn handle_xattrs(&self, req: XattrsRequest) -> ResponseKind {
        ResponseKind::Xattrs(Self::wire_result(
            self.server
                .vfs
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
                .vfs
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
                .vfs
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
                .vfs
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
                .vfs
                .streams(request_path(&req.path), req.follow)
                .await,
        ))
    }
}

fn wire_error(error: impl Into<Error>) -> WireError {
    error.into().into()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::report_handler_exit;
    use super::{Server, orderly_disconnect};
    use crate::protocol::{
        OpenHandle, OpenHandlePreference, OpenRequest, Request, RequestKind, ResponseKind,
        VfsProtocol,
    };

    fn request(kind: RequestKind) -> Request {
        Request { vfs: None, kind }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[should_panic(expected = "connection handler panic")]
    async fn connection_handler_panics_are_propagated() {
        let mut handlers = tokio::task::JoinSet::new();
        handlers.spawn(async {
            panic!("connection handler panic");
            #[allow(unreachable_code)]
            Ok::<(), dolang_rpc::Error>(())
        });
        report_handler_exit(handlers.join_next().await.unwrap());
    }

    #[test]
    fn orderly_connection_close_is_expected() {
        assert!(orderly_disconnect(&dolang_rpc::Error::ConnectionClosed));
        assert!(orderly_disconnect(&dolang_rpc::Error::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionReset)
        )));
        assert!(!orderly_disconnect(&dolang_rpc::Error::Protocol(
            "bad frame".into()
        )));
    }

    #[tokio::test]
    async fn remote_server_replies_without_serializing_a_handle() {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server =
            tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
        let client = crate::protocol::rpc_builder()
            .client(client_stream)
            .await
            .unwrap()
            .bind::<VfsProtocol>();

        let temp = tempfile::NamedTempFile::new().unwrap();
        let response = client
            .call(request(RequestKind::Open(OpenRequest {
                path: crate::typed_path(temp.path().to_path_buf())
                    .unwrap()
                    .to_path()
                    .into(),
                read: true,
                write: false,
                append: false,
                create: false,
                create_new: false,
                truncate: false,
                no_follow: false,
                handle_preference: OpenHandlePreference::NativePreferred,
            })))
            .await
            .unwrap()
            .into_response();
        let ResponseKind::Open(Ok(OpenHandle::Opaque(file))) = response else {
            panic!("remote open did not return an opaque file");
        };
        let ResponseKind::FileClose(result) = client
            .call(request(RequestKind::FileClose { file: file.clone() }))
            .await
            .unwrap()
            .into_response()
        else {
            panic!("file close returned the wrong response");
        };
        result.unwrap();
        let ResponseKind::FileClose(result) = client
            .call(request(RequestKind::FileClose { file }))
            .await
            .unwrap()
            .into_response()
        else {
            panic!("duplicate file close returned the wrong response");
        };
        assert_eq!(
            crate::Error::from(result.unwrap_err()).kind(),
            std::io::ErrorKind::InvalidInput
        );

        let _ = client.call(request(RequestKind::Stop)).await.unwrap();
        server.await.unwrap().unwrap();
    }
}
