use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[cfg(unix)]
use std::{path::Path, time::Duration};

use bytes::{Buf, BytesMut};
#[cfg(unix)]
use dolang_rpc::auth::AuthKey;
use dolang_rpc::{
    handle::{DefaultHandle, OsHandle},
    server::CallContext,
    session::{Cite, Gift, OpaqueGuard, OpaqueResource},
};
use dolang_winterop::security::SecDesc;
#[cfg(unix)]
use std::os::unix::io::OwnedFd;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient;
use typed_path::Utf8TypedPath;
#[cfg(all(docsrs, not(windows)))]
struct NamedPipeClient;
use tokio::sync::{Mutex, watch};
#[cfg(unix)]
use tokio::{
    net::{UnixListener, UnixStream, unix::SocketAddr},
    sync::mpsc,
    task::{JoinError, JoinSet},
    time::timeout,
};

use crate::{
    MAX_FILE_READ, STREAM_CHUNK_SIZE, SessionMode, Vfs,
    directory::ReadDir,
    error::{Error, ErrorKind, HandoffError, Result},
    extension::{self, ExtContext},
    file::XattrEntry,
    file::{AccessFlags, File, FileLock, FileLockRequest, StreamEntry},
    metadata::{FsMetadata, Metadata},
    process::{Child, Command, StdioRecv, StdioSend},
    protocol::{
        AccessRequest, AclRequest, CanonicalizeRequest, CopyRequest, CreateDirRequest,
        ExtensionRequest, ExtensionResponse, FsMetadataRequest, GlobRequest, HardLinkRequest,
        MetadataRequest, MoveRequest, OpenFlags, OpenHandle, OpenRequest, OpenVfsHandle,
        PipeResponse, QueryResponse, ReadDirPage, ReadLinkRequest, RemoveDirRequest, RemoveRequest,
        RenameRequest, Request, RequestKind, ResponseKind, SecDescRequest, SetAclRequest,
        SetMetadataRequest, SetSecDescRequest, SetXattrRequest, SpawnRequest, StdioRecvTarget,
        StdioSendTarget, StreamsRequest, SymlinkKind, SymlinkRequest, UnixVfsRequest, VfsProtocol,
        WellKnownPathRequest, WindowsAdminRequest, WirePath, XattrNamespaceRequest, XattrRequest,
        XattrsRequest, rpc_builder,
    },
    security::{Acl, AclKind},
    session::{
        ChildMarker, FileLockMarker, FileMarker, ReadDirMarker, StdioRecvMarker, StdioSendMarker,
        VfsMarker,
    },
};

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

/// A reserved drain slot, returned when the endpoint holding it dies.
///
/// Accounting rides the endpoint's own lifetime rather than any particular
/// message, so an endpoint reaches the drain exactly once however it ends:
/// explicitly closed, consumed by a spawn, or released because the peer
/// dropped the last opaque naming it.
struct DrainSlot(Arc<Drain>);

impl Drop for DrainSlot {
    fn drop(&mut self) {
        self.0.release(1);
    }
}

struct RetainedVfs {
    vfs: Vfs,
}

impl RetainedVfs {
    fn plain(vfs: Vfs) -> Self {
        Self { vfs }
    }
}

impl OpaqueResource for RetainedVfs {
    type Marker = VfsMarker;
}

/// Reads the whole of `len` bytes at `offset`, or as much as exists.
///
/// Deliberately buffers the entire answer instead of streaming it: the response
/// header goes out first and is the only place a structured error can be
/// reported, so the read has to have already succeeded by the time it is sent.
/// `len` is bounded by [`MAX_FILE_READ`] at the call site, which is what makes
/// buffering it whole affordable.
///
/// One [`File::read_at`] may come up short for reasons other than the end
/// of the file — a nested remote file clamps at one chunk, and a positional read
/// is permitted to be short in general — so this loops rather than treating the
/// first short read as the end.
async fn read_file_range(file: &File, offset: u64, len: usize) -> Result<BytesMut> {
    let mut buf = BytesMut::with_capacity(len);
    while buf.len() < len {
        let at = offset + buf.len() as u64;
        if file.read_at(&mut buf, at).await? == 0 {
            break;
        }
    }
    // `read_at` fills the spare capacity, which the allocator may have rounded
    // up past `len`. Delivering those extra bytes would be a protocol violation
    // on the peer's side, so trim back to what was asked for.
    buf.truncate(len);
    Ok(buf)
}

/// Accumulates up to one chunk from `trailer`, or `None` at its end.
///
/// Bounds how much of a write is held in memory at once: the peer may submit a
/// trailer far larger than a chunk, and the positional write it feeds wants an
/// owned buffer.
async fn next_trailer_chunk(
    trailer: &mut dolang_rpc::trailer::TrailerRecv,
) -> Result<Option<BytesMut>> {
    let mut buf = BytesMut::with_capacity(STREAM_CHUNK_SIZE);
    // Spare capacity is never zero while this loop runs, so `read_buf` fills
    // the buffer rather than growing it.
    while buf.len() < STREAM_CHUNK_SIZE && trailer.read_buf(&mut buf).await? != 0 {}
    Ok((!buf.is_empty()).then_some(buf))
}

struct RetainedFile(File);

/// A lock the peer still holds, addressed by its own opaque handle.
///
/// Not wrapped in a mutex: releasing is the only thing ever done with one, and
/// that unregisters the lock first, so the releasing task has it by value and
/// no other task can still reach it.
struct RetainedFileLock(FileLock);

impl OpaqueResource for RetainedFileLock {
    type Marker = FileLockMarker;
}

struct RetainedReadDir(Mutex<ReadDir>);

impl OpaqueResource for RetainedReadDir {
    type Marker = ReadDirMarker;
}

impl OpaqueResource for RetainedFile {
    type Marker = FileMarker;
}

struct RetainedStdioSend {
    stdio: Mutex<StdioSend>,
    /// Returned to the drain when the endpoint dies, however it ends.
    _slot: DrainSlot,
}

impl OpaqueResource for RetainedStdioSend {
    type Marker = StdioSendMarker;
}

struct RetainedStdioRecv {
    stdio: Mutex<StdioRecv>,
    /// Returned to the drain when the endpoint dies, however it ends.
    _slot: DrainSlot,
}

impl OpaqueResource for RetainedStdioRecv {
    type Marker = StdioRecvMarker;
}

struct RetainedChild(Mutex<Child>);

impl OpaqueResource for RetainedChild {
    type Marker = ChildMarker;
}

struct ServerState {
    vfs: Vfs,
    #[cfg(unix)]
    shutdown_tx: watch::Sender<()>,
}

/// How long a single connection may take to complete negotiation in
/// single-session mode before it is dropped. Generous by handshake standards:
/// the point is only to keep a peer that never speaks from occupying a slot
/// indefinitely, not to police slow networks.
#[cfg(unix)]
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many connections may be negotiating at once in single-session mode.
/// Bounds what a peer that opens connections without finishing them can tie
/// up; the listener resumes accepting as attempts drain.
#[cfg(unix)]
const MAX_PENDING_CONNECTIONS: usize = 8;

/// A connection that has completed negotiation, handed from a handler task
/// back to [`Server::accept_one`].
#[cfg(unix)]
struct Negotiated {
    rpc: dolang_rpc::server::Server<VfsProtocol>,
    connection: Arc<Connection>,
}

/// VFS agent server.
///
/// Construct a connected server with [`new`](Self::new) or
/// [`new_split`](Self::new_split) and call [`serve`](Self::serve). On Unix,
/// `bind` constructs a listener that accepts sessions until a client requests
/// shutdown.
pub struct Server {
    #[cfg(unix)]
    listener: Option<UnixListener>,
    rpc: Option<dolang_rpc::server::Server<VfsProtocol>>,
    mode: SessionMode,
    shared: Arc<ServerState>,
    /// Key each accepted connection must prove knowledge of. Held here rather
    /// than passed to [`bind`](Self::bind) because negotiation happens per
    /// connection, long after the listener exists.
    #[cfg(unix)]
    key: Option<AuthKey>,
}

impl Server {
    /// Creates an opaque-only VFS server over a bidirectional byte stream.
    pub async fn new<T>(stream: T) -> Result<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let rpc = rpc_builder(None).server(stream).await?.bind();
        Ok(Self {
            #[cfg(unix)]
            listener: None,
            rpc: Some(rpc),
            mode: SessionMode::Remote,
            shared: Self::state()?,
            #[cfg(unix)]
            key: None,
        })
    }

    /// Creates an opaque-only VFS server on separate reader and writer streams.
    pub async fn new_split<R, W>(reader: R, writer: W) -> Result<Self>
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let rpc = rpc_builder(None).server_split(reader, writer).await?.bind();
        Ok(Self {
            #[cfg(unix)]
            listener: None,
            rpc: Some(rpc),
            mode: SessionMode::Remote,
            shared: Self::state()?,
            #[cfg(unix)]
            key: None,
        })
    }

    fn state() -> Result<Arc<ServerState>> {
        #[cfg(unix)]
        let (shutdown_tx, _) = watch::channel(());
        Ok(Arc::new(ServerState {
            vfs: Vfs::direct()?,
            #[cfg(unix)]
            shutdown_tx,
        }))
    }

    /// Binds a Unix-domain listener for VFS agent connections.
    #[cfg(unix)]
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_listener(UnixListener::bind(path)?, None)
    }

    /// Binds a Unix-domain listener that requires mutual proof of a pre-shared
    /// key from every connection.
    ///
    /// The socket's permissions cannot distinguish the intended client when
    /// the peer's uid is not knowable in advance; `key` is what does. A
    /// connection that fails the check is dropped during negotiation, before
    /// it can issue any request.
    #[cfg(unix)]
    pub async fn bind_with_key(path: impl AsRef<Path>, key: Option<AuthKey>) -> Result<Self> {
        Self::from_listener(UnixListener::bind(path)?, key)
    }

    /// Create a server from an existing `UnixListener`.
    #[cfg(unix)]
    fn from_listener(listener: UnixListener, key: Option<AuthKey>) -> Result<Self> {
        Ok(Self {
            listener: Some(listener),
            rpc: None,
            mode: SessionMode::Native,
            shared: Self::state()?,
            key,
        })
    }

    /// Creates a VFS RPC server on the client end of a connected Windows named pipe.
    #[cfg(any(windows, docsrs))]
    #[cfg_attr(docsrs, doc(cfg(windows)))]
    #[cfg_attr(all(docsrs, not(windows)), allow(private_interfaces))]
    pub async fn from_named_pipe_client(pipe: NamedPipeClient) -> Result<Self> {
        #[cfg(windows)]
        {
            let rpc = rpc_builder(None)
                .server_named_pipe_client(pipe)
                .await?
                .bind();
            Ok(Self {
                #[cfg(unix)]
                listener: None,
                rpc: Some(rpc),
                mode: SessionMode::Native,
                shared: Self::state()?,
            })
        }
        #[cfg(all(docsrs, not(windows)))]
        {
            let _ = pipe;
            unreachable!()
        }
    }

    #[cfg(unix)]
    fn handle_accept(
        &self,
        res: io::Result<(UnixStream, SocketAddr)>,
        handlers: &mut JoinSet<Result<()>>,
    ) -> Result<()> {
        let (stream, _) = res?;
        let stream = stream.into_std()?;
        let connection = Arc::new(Connection {
            server: self.shared.clone(),
            mode: SessionMode::Native,
            drain: Drain::new(),
        });
        let key = self.key;
        handlers.spawn(async move {
            // Negotiation (a real handshake over the wire) happens here,
            // inside the per-connection task, so a slow or misbehaving peer
            // can't stall the accept loop from taking new connections. That
            // includes authentication: an unauthenticated peer fails here and
            // never reaches `serve_connection`.
            let rpc = rpc_builder(key).server_unix(stream).await?.bind();
            let handler = connection.clone();
            serve_connection(rpc, handler).await
        });
        Ok(())
    }

    /// Accepts connections until a client requests server shutdown.
    ///
    /// Each connection runs in an independent handler task. Routine client
    /// disconnects are ignored; unexpected handler failures are reported to
    /// standard error.
    #[cfg(unix)]
    pub async fn accept(mut self) -> Result<()> {
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
                    self.listener.take();
                    break;
                }
            }
        }

        while let Some(result) = handlers.join_next().await {
            report_handler_exit(result);
        }
        Ok(())
    }

    /// Accepts connections until one completes negotiation, then serves that
    /// session alone.
    ///
    /// `established` runs once, as soon as some connection has negotiated
    /// successfully — the point at which the listening socket has done its job
    /// and the caller can unlink it. Nothing is accepted afterwards.
    ///
    /// Connections that fail to negotiate (including failing authentication)
    /// are dropped and do *not* consume the single slot, so an impostor that
    /// reaches the socket first cannot deny the intended client its session;
    /// it can only waste an attempt. Negotiation has a timeout and a fixed
    /// limit on in-flight attempts, so a peer that connects and then says
    /// nothing cannot stall or crowd out the real one either.
    #[cfg(unix)]
    pub async fn accept_one<F>(mut self, established: F) -> Result<()>
    where
        F: FnOnce(),
    {
        let mut handlers = JoinSet::new();
        // Capacity one, and only ever received from once: whichever connection
        // negotiates first hands its session over and wins. A second one that
        // finishes in the same instant finds the channel full and is dropped.
        let (session_tx, mut session_rx) = mpsc::channel::<Negotiated>(1);

        let session = loop {
            tokio::select! {
                res = self.listener.as_ref().unwrap().accept(),
                    if handlers.len() < MAX_PENDING_CONNECTIONS =>
                {
                    if let Err(error) = self.handle_accept_one(res, &mut handlers, &session_tx) {
                        eprintln!("VFS server failed to accept a connection: {error}");
                    }
                }
                result = handlers.join_next(), if !handlers.is_empty() => {
                    report_handler_exit(result.unwrap());
                }
                Some(session) = session_rx.recv() => break session,
            }
        };

        // Stop listening before the session runs: the socket has done its job,
        // and any connection still mid-handshake has already lost the race, so
        // waiting for it would only delay the session (and, at shutdown, hold
        // the process open for the length of a negotiation timeout).
        self.listener.take();
        handlers.abort_all();
        while let Some(result) = handlers.join_next().await {
            if matches!(&result, Err(error) if error.is_cancelled()) {
                continue;
            }
            report_handler_exit(result);
        }
        established();

        let Negotiated { rpc, connection } = session;
        match serve_connection(rpc, connection).await {
            Ok(()) => Ok(()),
            Err(error) if orderly_disconnect(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Spawns a handler that negotiates and offers the resulting session to
    /// [`accept_one`](Self::accept_one), which serves it.
    ///
    /// The session is handed back rather than served in place so that the
    /// accept loop can abandon every other in-flight attempt the moment one
    /// succeeds.
    #[cfg(unix)]
    fn handle_accept_one(
        &self,
        res: io::Result<(UnixStream, SocketAddr)>,
        handlers: &mut JoinSet<Result<()>>,
        session_tx: &mpsc::Sender<Negotiated>,
    ) -> Result<()> {
        let (stream, _) = res?;
        let stream = stream.into_std()?;
        let connection = Arc::new(Connection {
            server: self.shared.clone(),
            mode: SessionMode::Native,
            drain: Drain::new(),
        });
        let key = self.key;
        let session_tx = session_tx.clone();
        handlers.spawn(async move {
            let negotiated = timeout(NEGOTIATE_TIMEOUT, rpc_builder(key).server_unix(stream)).await;
            let rpc = match negotiated {
                Ok(result) => result?.bind(),
                Err(_elapsed) => {
                    // Not worth reporting: a peer that connects and then says
                    // nothing is exactly what the timeout is for.
                    return Ok(());
                }
            };
            // A full channel or a closed receiver both mean another connection
            // got there first; drop this one.
            let _ = session_tx.try_send(Negotiated { rpc, connection });
            Ok(())
        });
        Ok(())
    }

    /// Serves one connected VFS session until it closes or fails.
    pub async fn serve(mut self) -> Result<()> {
        let connection = Arc::new(Connection {
            server: self.shared,
            mode: self.mode,
            drain: Drain::new(),
        });
        let rpc = self
            .rpc
            .take()
            .expect("server does not own a connected session");
        match serve_connection(rpc, connection).await {
            Ok(()) => Ok(()),
            Err(error) if orderly_disconnect(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
fn report_handler_exit(result: std::result::Result<Result<()>, JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) if orderly_disconnect(&error) => {}
        Ok(Err(error)) => eprintln!("VFS connection handler failed: {error}"),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => eprintln!("VFS connection handler task failed: {error}"),
    }
}

fn orderly_disconnect(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::NotConnected
    )
}

async fn serve_connection(
    rpc: dolang_rpc::server::Server<VfsProtocol>,
    connection: Arc<Connection>,
) -> Result<()> {
    rpc.serve(async move |mut context, Request { vfs, kind }| {
        let response = if matches!(kind, RequestKind::Stop) {
            connection.handle_stop(&mut context, vfs).await
        } else if let Err(error) = connection.select(&context, vfs.clone()) {
            Err(error)
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
                RequestKind::FileRead { file, offset, len } => {
                    connection
                        .handle_file_read(context, file, offset, len)
                        .await;
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
    .await?;
    Ok(())
}

impl Connection {
    fn select(
        &self,
        context: &CallContext<VfsProtocol>,
        vfs: Option<Cite<VfsMarker>>,
    ) -> Result<Self> {
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
        vfs: Option<Cite<VfsMarker>>,
    ) -> Result<ResponseKind> {
        let Some(vfs) = vfs else {
            // Stop accepting immediately. Existing sessions have their own
            // connection tasks and continue draining independently.
            #[cfg(unix)]
            let _ = self.server.shutdown_tx.send(());
            // Reject new stdio endpoints, then keep serving reads, writes and
            // closes on the ones already handed out until they are all closed.
            // The rpc serve loop polls request handlers on the same task as it
            // reads frames, so awaiting here does not stall the connection.
            self.drain.begin_stop();
            self.drain.wait().await;
            context.shutdown();
            return Ok(ResponseKind::Stop);
        };
        let retained = match context.unregister::<RetainedVfs>(vfs) {
            Ok(Some(retained)) => retained,
            Ok(None) => {
                return Err(Error::new(ErrorKind::ResourceBusy, "opaque VFS is in use"));
            }
            Err(_) => return Err(Self::invalid_opaque("VFS")),
        };
        match retained.vfs.stop().await {
            Ok(()) => Ok(ResponseKind::Stop),
            Err(error) => Err(error),
        }
    }

    fn unsupported(operation: &str) -> Error {
        Error::new(
            ErrorKind::Unsupported,
            format!("{operation} is not supported by a remote VFS session"),
        )
    }

    fn invalid_opaque(kind: &str) -> Error {
        Error::new(ErrorKind::InvalidInput, format!("invalid opaque {kind}"))
    }

    async fn handle(
        &self,
        context: &mut CallContext<VfsProtocol>,
        kind: RequestKind,
    ) -> Result<ResponseKind> {
        match kind {
            RequestKind::Query => self.handle_query().await,
            RequestKind::UserName { uid } => Ok(ResponseKind::UserName(
                self.server.vfs.user_name(uid).await?,
            )),
            RequestKind::UserId { name } => {
                Ok(ResponseKind::UserId(self.server.vfs.user_id(&name).await?))
            }
            RequestKind::GroupName { gid } => Ok(ResponseKind::GroupName(
                self.server.vfs.group_name(gid).await?,
            )),
            RequestKind::GroupId { name } => Ok(ResponseKind::GroupId(
                self.server.vfs.group_id(&name).await?,
            )),
            RequestKind::SidName { sid } => {
                Ok(ResponseKind::SidName(self.server.vfs.sid_name(&sid).await?))
            }
            RequestKind::AccountName { name } => Ok(ResponseKind::AccountName(
                self.server.vfs.account_name(&name).await?,
            )),
            RequestKind::ResolvePrincipalId { input, want } => {
                Ok(ResponseKind::ResolvePrincipalId(
                    self.server.vfs.resolve_principal_id(input, want).await?,
                ))
            }
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
                self.server.vfs.clear_cache().await?;
                Ok(ResponseKind::ClearCache)
            }
            RequestKind::Pipe { buf_size } => Ok(ResponseKind::Pipe(
                self.handle_pipe(context, buf_size).await?,
            )),
            RequestKind::Open(request) => self.handle_open(context, request).await,
            RequestKind::FileRead { .. } => unreachable!(),
            RequestKind::FileWrite { file, offset } => Ok(ResponseKind::FileWrite(
                self.handle_file_write(context, file, offset).await?,
            )),
            RequestKind::FileAppend { file } => Ok(ResponseKind::FileAppend(
                self.handle_file_append(context, file).await?,
            )),
            RequestKind::FileSize { file } => Ok(ResponseKind::FileSize(
                self.handle_file_size(context, file).await?,
            )),
            RequestKind::FileSetSize { file, size } => {
                self.handle_file_set_size(context, file, size).await?;
                Ok(ResponseKind::FileSetSize)
            }
            RequestKind::FileLock { file, request } => {
                self.handle_file_lock(context, file, request).await
            }
            RequestKind::FileUnlock { lock } => self.handle_file_unlock(context, lock).await,
            RequestKind::FileToStdioSend { file, offset } => Ok(ResponseKind::FileToStdioSend(
                self.handle_file_to_stdio_send(context, file, offset)
                    .await?,
            )),
            RequestKind::FileToStdioRecv { file, offset } => Ok(ResponseKind::FileToStdioRecv(
                self.handle_file_to_stdio_recv(context, file, offset)
                    .await?,
            )),
            RequestKind::StdioSendClose { stdio } => {
                self.close_stdio_send(context, stdio)?;
                Ok(ResponseKind::StdioSendClose)
            }
            RequestKind::StdioSendWrite { stdio } => Ok(ResponseKind::StdioSendWrite(
                self.handle_stdio_send_write(context, stdio).await?,
            )),
            RequestKind::StdioSendClone { stdio } => Ok(ResponseKind::StdioSendClone(
                self.handle_stdio_send_clone(context, stdio).await?,
            )),
            RequestKind::StdioRecvClose { stdio } => {
                self.close_stdio_recv(context, stdio)?;
                Ok(ResponseKind::StdioRecvClose)
            }
            RequestKind::StdioRecvRead { .. } => unreachable!(),
            RequestKind::StdioRecvClone { stdio } => Ok(ResponseKind::StdioRecvClone(
                self.handle_stdio_recv_clone(context, stdio).await?,
            )),
            RequestKind::FileMetadata { file } => Ok(ResponseKind::FileMetadata(
                self.handle_file_metadata(context, file).await?,
            )),
            RequestKind::FileFsMetadata { file } => Ok(ResponseKind::FileFsMetadata(
                self.handle_file_fs_metadata(context, file).await?,
            )),
            RequestKind::FileAcl {
                file,
                kind,
                default,
            } => Ok(ResponseKind::FileAcl(
                self.handle_file_acl(context, file, kind, default).await?,
            )),
            RequestKind::FileSetAcl {
                file,
                kind,
                acl,
                default,
            } => {
                self.handle_file_set_acl(context, file, kind, acl, default)
                    .await?;
                Ok(ResponseKind::FileSetAcl)
            }
            RequestKind::FileSecDesc { file, mask } => Ok(ResponseKind::FileSecDesc(
                self.handle_file_sec_desc(context, file, mask).await?,
            )),
            RequestKind::FileSetSecDesc { file, sec_desc } => {
                self.handle_file_set_sec_desc(context, file, sec_desc)
                    .await?;
                Ok(ResponseKind::FileSetSecDesc)
            }
            RequestKind::FileXattrs { file, namespace } => Ok(ResponseKind::FileXattrs(
                self.handle_file_xattrs(context, file, namespace).await?,
            )),
            RequestKind::FileXattr {
                file,
                name,
                namespace,
            } => Ok(ResponseKind::FileXattr(
                self.handle_file_xattr(context, file, name, namespace)
                    .await?,
            )),
            RequestKind::FileStreams { file } => Ok(ResponseKind::FileStreams(
                self.handle_file_streams(context, file).await?,
            )),
            RequestKind::FileSetXattr {
                file,
                name,
                namespace,
                value,
            } => {
                self.handle_file_set_xattr(context, file, name, namespace, value)
                    .await?;
                Ok(ResponseKind::FileSetXattr)
            }
            RequestKind::FileRemoveXattr {
                file,
                name,
                namespace,
            } => {
                self.handle_file_remove_xattr(context, file, name, namespace)
                    .await?;
                Ok(ResponseKind::FileRemoveXattr)
            }
            RequestKind::FileClose { file } => self.handle_file_close(context, file).await,
            RequestKind::UnixVfs(request) => self.handle_unix_vfs(context, request).await,
            RequestKind::WindowsAdmin(request) => self.handle_windows_admin(context, request).await,
            RequestKind::ReadDir { path } => self.handle_read_dir(context, path).await,
            RequestKind::ReadDirNext { read_dir } => {
                self.handle_read_dir_next(context, read_dir).await
            }
            RequestKind::ReadDirClose { read_dir } => self.handle_read_dir_close(context, read_dir),
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
    ) -> Result<ResponseKind> {
        let ExtensionRequest {
            name,
            version,
            payload,
        } = request;
        let Some(ext) = extension::lookup(&name, version).filter(|extension| extension.available())
        else {
            return Err(Self::unsupported(&format!(
                "VFS extension {name} v{version}"
            )));
        };
        let mut ctx = ExtContext::remote(context, self.mode == SessionMode::Native);
        let payload = ext.dispatch(&mut ctx, payload).await;
        Ok(ResponseKind::Extension(ExtensionResponse {
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
    ) -> Result<ResponseKind> {
        let resolved = self
            .server
            .vfs
            .which(
                Into::into(&program),
                path.as_deref(),
                cwd.as_ref().map(Into::into),
            )
            .await?
            .map(Into::into);
        Ok(ResponseKind::Which(resolved))
    }

    async fn handle_well_known_path(&self, req: WellKnownPathRequest) -> Result<ResponseKind> {
        let path = self
            .server
            .vfs
            .well_known_path(req.key, req.app.as_deref(), &req.env)
            .await?
            .into();
        Ok(ResponseKind::WellKnownPath(path))
    }

    async fn handle_spawn_rpc(
        &self,
        context: &mut CallContext<VfsProtocol>,
        req: SpawnRequest,
    ) -> Result<ResponseKind> {
        let mut cmd = self.server.vfs.command(Into::into(&req.program));
        for arg in &req.args {
            cmd.arg(arg);
        }

        if let Some(cwd) = &req.cwd {
            cmd.current_dir(Into::into(cwd));
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

        self.configure_spawn_stdio(context, &mut cmd, req.stdin, req.stdout, req.stderr)
            .await?;

        let child = cmd.spawn().await?;
        Ok(ResponseKind::Spawn(
            context.register(RetainedChild(Mutex::new(child))),
        ))
    }

    fn spawn_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        target: StdioRecvTarget,
    ) -> Result<Option<StdioRecv>> {
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
                // Consuming the endpoint hands it to the child, which takes
                // its drain slot along with it: once a child owns an endpoint
                // the peer is no longer relaying through it, which is the only
                // thing the drain protects.
                let stdio = context
                    .unregister::<RetainedStdioRecv>(stdio)
                    .map_err(|_| Self::invalid_opaque("stdio receive"))?;
                let Some(stdio) = stdio else {
                    return Err(Error::new(
                        ErrorKind::ResourceBusy,
                        "opaque stdio receive is in use",
                    ));
                };
                Ok(Some(stdio.stdio.into_inner()))
            }
        }
    }

    fn spawn_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        target: StdioSendTarget,
    ) -> Result<Option<StdioSend>> {
        match target {
            StdioSendTarget::Null | StdioSendTarget::Stdout => Ok(None),
            StdioSendTarget::Native(handle) => {
                if self.mode == SessionMode::Remote {
                    return Err(Self::unsupported("native process stdio"));
                }
                Ok(Some(StdioSend::from_file(tokio::fs::File::from_std(
                    handle.into_inner().into(),
                ))))
            }
            StdioSendTarget::Opaque(stdio) => {
                // Consuming the endpoint hands it to the child, which takes
                // its drain slot along with it: once a child owns an endpoint
                // the peer is no longer relaying through it, which is the only
                // thing the drain protects.
                let stdio = context
                    .unregister::<RetainedStdioSend>(stdio)
                    .map_err(|_| Self::invalid_opaque("stdio send"))?;
                let Some(stdio) = stdio else {
                    return Err(Error::new(
                        ErrorKind::ResourceBusy,
                        "opaque stdio send is in use",
                    ));
                };
                Ok(Some(stdio.stdio.into_inner()))
            }
        }
    }

    async fn configure_spawn_stdio(
        &self,
        context: &CallContext<VfsProtocol>,
        command: &mut Command<'_>,
        stdin: StdioRecvTarget,
        stdout: StdioSendTarget,
        stderr: StdioSendTarget,
    ) -> Result<()> {
        let stdin = self.spawn_stdio_recv(context, stdin);
        let stdout = self.spawn_stdio_send(context, stdout);
        let stderr_to_stdout = matches!(stderr, StdioSendTarget::Stdout);
        let stderr = self.spawn_stdio_send(context, stderr);
        let (stdin, stdout, stderr) = (stdin?, stdout?, stderr?);

        if let Some(stdio) = stdin {
            command.stdin(stdio)?;
        } else {
            command.stdin_null();
        }
        if let Some(stdio) = stdout {
            command.stdout(stdio)?;
        } else {
            command.stdout_null();
        }
        if stderr_to_stdout {
            command.stderr_to_stdout()?;
        } else if let Some(stdio) = stderr {
            command.stderr(stdio)?;
        } else {
            command.stderr_null();
        }
        Ok(())
    }

    fn take_child(
        &self,
        context: &CallContext<VfsProtocol>,
        child: Cite<ChildMarker>,
    ) -> Result<RetainedChild> {
        context
            .unregister::<RetainedChild>(child)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid opaque child"))?
            .ok_or_else(|| Error::new(ErrorKind::ResourceBusy, "opaque child is in use"))
    }

    async fn handle_child_wait(
        &self,
        context: &mut CallContext<VfsProtocol>,
        child: Cite<ChildMarker>,
    ) -> Result<ResponseKind> {
        let child = self.take_child(context, child)?;
        let mut child = child.0.into_inner();
        let status = match context.cancel_guard(async |_| child.wait().await).await {
            Ok(result) => result?,
            Err(_) => child
                .terminate()
                .await?
                .ok_or_else(|| Error::other("process was orphaned during cancelled wait"))?,
        };
        Ok(ResponseKind::ChildWait(status))
    }

    async fn handle_child_terminate(
        &self,
        context: &CallContext<VfsProtocol>,
        child: Cite<ChildMarker>,
    ) -> Result<ResponseKind> {
        let child = self.take_child(context, child)?;
        let status = child.0.into_inner().terminate().await?;
        Ok(ResponseKind::ChildTerminate(status))
    }

    fn handle_child_close(
        &self,
        context: &CallContext<VfsProtocol>,
        child: Cite<ChildMarker>,
    ) -> Result<ResponseKind> {
        context
            .unregister::<RetainedChild>(child)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid opaque child"))?;
        Ok(ResponseKind::ChildClose)
    }

    async fn handle_query(&self) -> Result<ResponseKind> {
        let vfs = &self.server.vfs;
        Ok(ResponseKind::Query(QueryResponse {
            env: vfs.env().collect(),
            cwd: vfs.cwd().into(),
            current_exe: vfs.current_exe().into(),
            target: vfs.target().clone(),
            security: vfs.security().clone(),
            extensions: vfs.extensions().clone(),
        }))
    }

    /// Reserves a drain slot for an endpoint about to be handed to the peer.
    ///
    /// Only endpoint creation is gated this way. `Spawn` and `Open` stay
    /// available while stopping: they create no stdio endpoint of their own,
    /// and refusing a spawn could break the very in-flight pipeline stage the
    /// drain exists to protect.
    fn reserve_stdio(&self) -> Result<DrainSlot> {
        if self.drain.try_acquire(1) {
            Ok(DrainSlot(self.drain.clone()))
        } else {
            Err(Error::new(
                ErrorKind::NotConnected,
                "VFS session is stopping",
            ))
        }
    }

    async fn handle_pipe(
        &self,
        context: &CallContext<VfsProtocol>,
        buf_size: Option<usize>,
    ) -> Result<PipeResponse> {
        let (send, recv) = self.server.vfs.pipe(buf_size).await?;
        let send_slot = self.reserve_stdio()?;
        let recv_slot = self.reserve_stdio()?;
        Ok(PipeResponse {
            send: context.register(RetainedStdioSend {
                stdio: Mutex::new(send),
                _slot: send_slot,
            }),
            recv: context.register(RetainedStdioRecv {
                stdio: Mutex::new(recv),
                _slot: recv_slot,
            }),
        })
    }

    fn retained_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Cite<StdioSendMarker>,
    ) -> Result<OpaqueGuard<RetainedStdioSend>> {
        context
            .acquire::<RetainedStdioSend>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio send"))
    }

    fn retained_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Cite<StdioRecvMarker>,
    ) -> Result<OpaqueGuard<RetainedStdioRecv>> {
        context
            .acquire::<RetainedStdioRecv>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio receive"))
    }

    /// Empties an endpoint's contents, without waiting for the peer to stop
    /// naming it.
    ///
    /// A close that races the endpoint's own read or write leaves the contents
    /// alive until that operation finishes, and the registration alive until
    /// the peer drops its last reference. Neither is worth reporting: a peer
    /// closing while its own I/O is in flight has no flush guarantee to lose,
    /// and the drain slot is returned when the endpoint actually dies rather
    /// than when this request happens to be served.
    fn close_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Cite<StdioSendMarker>,
    ) -> Result<()> {
        context
            .unregister::<RetainedStdioSend>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio send"))?;
        Ok(())
    }

    fn close_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Cite<StdioRecvMarker>,
    ) -> Result<()> {
        context
            .unregister::<RetainedStdioRecv>(stdio)
            .map_err(|_| Self::invalid_opaque("stdio receive"))?;
        Ok(())
    }

    async fn handle_stdio_send_write(
        &self,
        context: &mut CallContext<VfsProtocol>,
        stdio: Cite<StdioSendMarker>,
    ) -> Result<usize> {
        let stdio = self.retained_stdio_send(context, stdio)?;
        let trailer = context.trailer().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "stdio write request is missing its data trailer",
            )
        })?;
        let mut trailer = io::BufReader::with_capacity(STREAM_CHUNK_SIZE, trailer);
        let len = io::copy_buf(&mut trailer, &mut *stdio.stdio.lock().await).await?;
        usize::try_from(len).map_err(|_| {
            Error::new(
                ErrorKind::InvalidData,
                "stdio trailer length does not fit in usize",
            )
        })
    }

    async fn handle_stdio_send_clone(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Cite<StdioSendMarker>,
    ) -> Result<Gift<StdioSendMarker>> {
        let stdio = self.retained_stdio_send(context, stdio)?;
        let clone = stdio.stdio.lock().await.try_clone().await?;
        let slot = self.reserve_stdio()?;
        Ok(context.register(RetainedStdioSend {
            stdio: Mutex::new(clone),
            _slot: slot,
        }))
    }

    async fn handle_stdio_recv_read(
        &self,
        context: CallContext<VfsProtocol>,
        stdio: Cite<StdioRecvMarker>,
        len: usize,
    ) {
        let stdio = match self.retained_stdio_recv(&context, stdio) {
            Ok(stdio) => stdio,
            Err(error) => {
                context.respond(Err(error));
                return;
            }
        };
        // Unlike `handle_file_read`, this streams: the source is a pipe of
        // unknown length, so there is nothing to hold and the answer cannot be
        // known before responding. A failure part way through therefore has to
        // report itself by abandoning the trailer, which for a byte stream is
        // the right shape anyway — it is a teardown, not an operation that
        // failed with a particular errno the peer could act on.
        let mut send = context.respond_with_trailer(Ok(ResponseKind::StdioRecvRead));
        let copied = {
            let mut guard = stdio.stdio.lock().await;
            let mut source = io::BufReader::with_capacity(
                len.clamp(1, STREAM_CHUNK_SIZE),
                (&mut *guard).take(len as u64),
            );
            io::copy_buf(&mut source, &mut send).await.is_ok()
        };
        // Released before the terminal fragment: the peer may hand the endpoint
        // to a child or close it as soon as the trailer ends, and both consume
        // the endpoint.
        drop(stdio);
        if copied {
            send.finish();
        }
    }

    async fn handle_stdio_recv_clone(
        &self,
        context: &CallContext<VfsProtocol>,
        stdio: Cite<StdioRecvMarker>,
    ) -> Result<Gift<StdioRecvMarker>> {
        let stdio = self.retained_stdio_recv(context, stdio)?;
        let clone = stdio.stdio.lock().await.try_clone().await?;
        let slot = self.reserve_stdio()?;
        Ok(context.register(RetainedStdioRecv {
            stdio: Mutex::new(clone),
            _slot: slot,
        }))
    }

    async fn handle_open(
        &self,
        context: &CallContext<VfsProtocol>,
        req: OpenRequest,
    ) -> Result<ResponseKind> {
        let mut opts = self.server.vfs.open_options();
        opts.read(req.flags.contains(OpenFlags::READ))
            .write(req.flags.contains(OpenFlags::WRITE))
            .append(req.flags.contains(OpenFlags::APPEND))
            .create(req.flags.contains(OpenFlags::CREATE))
            .create_new(req.flags.contains(OpenFlags::CREATE_NEW))
            .truncate(req.flags.contains(OpenFlags::TRUNCATE))
            .no_follow(req.flags.contains(OpenFlags::NO_FOLLOW));

        let file = opts.open(Into::into(&req.path)).await?;
        let handle = if self.mode == SessionMode::Remote {
            OpenHandle::Opaque(context.register(RetainedFile(file)))
        } else {
            let handle: DefaultHandle = file.try_into_std().await.unwrap().into();
            OpenHandle::Native(OsHandle::new(handle))
        };
        Ok(ResponseKind::Open(handle))
    }

    fn retained_file(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<OpaqueGuard<RetainedFile>> {
        context
            .acquire::<RetainedFile>(file)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid opaque file"))
    }

    /// Takes a file out of the session's registry, for an operation that
    /// consumes it.
    ///
    /// Uses the recovering [`try_unregister`], so a file with operations still
    /// in flight stays registered and the peer can retry once they finish;
    /// [`handle_file_close`](Self::handle_file_close) deliberately does not,
    /// because a close must still take effect after the racing operation ends.
    ///
    /// [`try_unregister`]: dolang_rpc::server::CallContext::try_unregister
    fn take_file(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<RetainedFile> {
        match context.try_unregister::<RetainedFile>(file) {
            Ok(Some(file)) => Ok(file),
            Ok(None) => Err(Error::new(ErrorKind::ResourceBusy, "opaque file is in use")),
            Err(_) => Err(Error::new(ErrorKind::InvalidInput, "invalid opaque file")),
        }
    }

    async fn handle_file_read(
        &self,
        context: CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        offset: u64,
        len: usize,
    ) {
        let file = match self.retained_file(&context, file) {
            Ok(file) => file,
            Err(error) => {
                context.respond(Err(error));
                return;
            }
        };
        // Read before responding. Once the response header is out the only way
        // left to report a failure is to abandon the trailer, which reaches the
        // peer as a bare `BrokenPipe` abort with the real error discarded — so
        // the read has to have already succeeded or failed by then. Clamping is
        // what makes holding the whole answer affordable, and it also stops the
        // peer from choosing the size of this allocation.
        let data = read_file_range(&file.0, offset, len.min(MAX_FILE_READ)).await;
        // Release the file before anything at all goes back. The peer is
        // entitled to close the file the moment it sees this read conclude, and
        // a reference still held here would make its `FileClose` fail as in
        // use. Buffering the answer first is what lets the guard be dropped
        // this early — earlier than when the read was streamed, where it had to
        // live until the last fragment.
        drop(file);
        let data = match data {
            Ok(data) => data,
            Err(error) => {
                context.respond(Err(error));
                return;
            }
        };
        let mut send = context.respond_with_trailer(Ok(ResponseKind::FileRead));
        if send.write_all(&data).await.is_ok() {
            send.finish();
        }
    }

    async fn handle_file_write(
        &self,
        context: &mut CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        offset: u64,
    ) -> Result<usize> {
        let file = self.retained_file(context, file)?;
        let mut trailer = Self::write_trailer(context)?;
        let mut written = 0usize;
        while let Some(chunk) = next_trailer_chunk(&mut trailer).await? {
            let mut chunk = chunk.freeze();
            while !chunk.is_empty() {
                let n = file
                    .0
                    .write_at(chunk.clone(), offset + written as u64)
                    .await?;
                if n == 0 {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "file write made no progress",
                    ));
                }
                chunk.advance(n);
                written += n;
            }
        }
        Ok(written)
    }

    async fn handle_file_append(
        &self,
        context: &mut CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<(usize, u64)> {
        let file = self.retained_file(context, file)?;
        let mut trailer = Self::write_trailer(context)?;
        // The offset of an append is the description's business, not ours. The
        // peer cannot know where the data landed either, so report the
        // resulting position along with the count.
        let mut written = 0usize;
        let mut end = file.0.metadata().await?.len;
        while let Some(chunk) = next_trailer_chunk(&mut trailer).await? {
            let mut chunk = chunk.freeze();
            while !chunk.is_empty() {
                let (n, position) = file.0.append(chunk.clone()).await?;
                if n == 0 {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "file append made no progress",
                    ));
                }
                chunk.advance(n);
                written += n;
                end = position;
            }
        }
        Ok((written, end))
    }

    fn write_trailer(
        context: &mut CallContext<VfsProtocol>,
    ) -> Result<dolang_rpc::trailer::TrailerRecv> {
        context.trailer().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "file write request is missing its data trailer",
            )
        })
    }

    async fn handle_file_size(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<u64> {
        let file = self.retained_file(context, file)?;
        Ok(file.0.metadata().await?.len)
    }

    async fn handle_file_set_size(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        size: u64,
    ) -> Result<()> {
        let file = self.retained_file(context, file)?;
        file.0.set_size(size).await
    }

    async fn handle_file_to_stdio_send(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        offset: u64,
    ) -> Result<Gift<StdioSendMarker>> {
        // Before the file is taken, so that running out of endpoint slots
        // leaves the peer's handle alone.
        let slot = self.reserve_stdio()?;
        let file = self.take_file(context, file)?;
        // The peer's cursor is the one that matters, and the descriptor the
        // child inherits carries a position of its own, so plant it explicitly
        // rather than letting this side's idea of the position decide.
        let stdio = file
            .0
            .into_stdio_send(offset)
            .await
            .map_err(handoff_error)?;
        Ok(context.register(RetainedStdioSend {
            stdio: Mutex::new(stdio),
            _slot: slot,
        }))
    }

    async fn handle_file_to_stdio_recv(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        offset: u64,
    ) -> Result<Gift<StdioRecvMarker>> {
        let slot = self.reserve_stdio()?;
        let file = self.take_file(context, file)?;
        // The peer's cursor is the one that matters, and the descriptor the
        // child inherits carries a position of its own, so plant it explicitly
        // rather than letting this side's idea of the position decide.
        let stdio = file
            .0
            .into_stdio_recv(offset)
            .await
            .map_err(handoff_error)?;
        Ok(context.register(RetainedStdioRecv {
            stdio: Mutex::new(stdio),
            _slot: slot,
        }))
    }

    async fn handle_file_metadata(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<Metadata> {
        let file = self.retained_file(context, file)?;
        file.0.metadata().await
    }

    async fn handle_file_fs_metadata(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<FsMetadata> {
        let file = self.retained_file(context, file)?;
        file.0.fs_metadata().await
    }

    async fn handle_file_sec_desc(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        mask: dolang_winterop::security::SecInfo,
    ) -> Result<SecDesc> {
        let file = self.retained_file(context, file)?;
        file.0.sec_desc(mask).await
    }

    async fn handle_file_acl(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        kind: AclKind,
        default: bool,
    ) -> Result<Option<Acl>> {
        let file = self.retained_file(context, file)?;
        file.0.acl(kind, default).await
    }

    async fn handle_file_set_acl(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        kind: AclKind,
        acl: Option<Acl>,
        default: bool,
    ) -> Result<()> {
        let file = self.retained_file(context, file)?;
        file.0.set_acl(kind, acl.as_ref(), default).await
    }

    async fn handle_file_set_sec_desc(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        sec_desc: SecDesc,
    ) -> Result<()> {
        let file = self.retained_file(context, file)?;
        file.0.set_sec_desc(&sec_desc).await
    }

    async fn handle_file_xattrs(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        namespace: XattrNamespaceRequest,
    ) -> Result<Vec<XattrEntry>> {
        let file = self.retained_file(context, file)?;
        file.0.xattrs(namespace.as_borrowed()).await
    }

    async fn handle_file_xattr(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        name: String,
        namespace: Option<String>,
    ) -> Result<Vec<u8>> {
        let file = self.retained_file(context, file)?;
        file.0.xattr(&name, namespace.as_deref()).await
    }

    async fn handle_file_streams(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<Vec<StreamEntry>> {
        let file = self.retained_file(context, file)?;
        file.0.streams().await
    }

    async fn handle_file_set_xattr(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        name: String,
        namespace: Option<String>,
        value: Vec<u8>,
    ) -> Result<()> {
        let file = self.retained_file(context, file)?;
        file.0.set_xattr(&name, namespace.as_deref(), &value).await
    }

    async fn handle_file_remove_xattr(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        name: String,
        namespace: Option<String>,
    ) -> Result<()> {
        let file = self.retained_file(context, file)?;
        file.0.remove_xattr(&name, namespace.as_deref()).await
    }

    async fn handle_file_lock(
        &self,
        context: &mut CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
        request: FileLockRequest,
    ) -> Result<ResponseKind> {
        let retained = self.retained_file(context, file)?;
        let acquired = context
            .cancel_guard(async |_context| {
                retained
                    .0
                    .lock(request.range, request.mode, request.behavior)
                    .await
            })
            .await;
        let acquired = acquired.map_err(|_| {
            Error::new(
                ErrorKind::Interrupted,
                "file lock acquisition was cancelled",
            )
        })??;
        drop(retained);
        // The lock gets an opaque handle of its own rather than an id in a
        // table hanging off the file. Nothing needs it to be reachable from the
        // file: closing a file releases every lock held on it in band, and a
        // lock dropped without an explicit release still releases itself.
        Ok(ResponseKind::FileLock(
            acquired.map(|lock| context.register(RetainedFileLock(lock))),
        ))
    }

    async fn handle_file_unlock(
        &self,
        context: &CallContext<VfsProtocol>,
        lock: Cite<FileLockMarker>,
    ) -> Result<ResponseKind> {
        // Releasing consumes the lock, so a handle that is unknown or already
        // released is a no-op rather than an error, as it was when the peer
        // named locks by id.
        let Ok(Some(mut lock)) = context.unregister::<RetainedFileLock>(lock) else {
            return Ok(ResponseKind::FileUnlock);
        };
        lock.0.release().await?;
        Ok(ResponseKind::FileUnlock)
    }

    async fn handle_file_close(
        &self,
        context: &CallContext<VfsProtocol>,
        file: Cite<FileMarker>,
    ) -> Result<ResponseKind> {
        let retained = self.retained_file(context, file.clone())?;
        drop(retained);
        match context.unregister::<RetainedFile>(file) {
            // `close` releases every lock still held on the file in band, so
            // there is nothing to unwind here first.
            Ok(Some(file)) => file.0.close().await,
            Ok(None) => Err(Error::new(ErrorKind::ResourceBusy, "opaque file is in use")),
            Err(_) => Err(Error::new(ErrorKind::InvalidInput, "invalid opaque file")),
        }?;
        Ok(ResponseKind::FileClose)
    }

    async fn handle_read_dir(
        &self,
        context: &CallContext<VfsProtocol>,
        path: WirePath,
    ) -> Result<ResponseKind> {
        let read_dir = self.server.vfs.read_dir(Into::into(&path)).await?;
        Ok(ResponseKind::ReadDir(
            context.register(RetainedReadDir(Mutex::new(read_dir))),
        ))
    }

    async fn handle_read_dir_next(
        &self,
        context: &CallContext<VfsProtocol>,
        read_dir: Cite<ReadDirMarker>,
    ) -> Result<ResponseKind> {
        let retained = context
            .acquire::<RetainedReadDir>(read_dir.clone())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid opaque directory"))?;
        let mut read_dir_guard = retained.0.lock().await;
        let mut entries = Vec::with_capacity(64);
        let mut done = false;
        while entries.len() < 64 {
            match read_dir_guard.next_entry().await? {
                Some(entry) => entries.push(entry),
                None => {
                    done = true;
                    break;
                }
            }
        }
        drop(read_dir_guard);
        drop(retained);
        if done {
            let _ = context.unregister::<RetainedReadDir>(read_dir);
        }
        Ok(ResponseKind::ReadDirNext(ReadDirPage { entries, done }))
    }

    fn handle_read_dir_close(
        &self,
        context: &CallContext<VfsProtocol>,
        read_dir: Cite<ReadDirMarker>,
    ) -> Result<ResponseKind> {
        context
            .unregister::<RetainedReadDir>(read_dir)
            .map_err(|_| Self::invalid_opaque("directory"))?;
        Ok(ResponseKind::ReadDirClose)
    }

    async fn handle_unix_vfs(
        &self,
        context: &CallContext<VfsProtocol>,
        req: UnixVfsRequest,
    ) -> Result<ResponseKind> {
        #[cfg(unix)]
        if self.mode == SessionMode::Native && self.server.vfs.is_direct() {
            let handle: OwnedFd = async {
                let path = crate::path::native_path(Into::into(&req.path))?;
                let stream = UnixStream::connect(path).await?;
                Ok::<OwnedFd, Error>(stream.into_std()?.into())
            }
            .await?;
            return Ok(ResponseKind::UnixVfs(OpenVfsHandle::Native(OsHandle::new(
                handle,
            ))));
        }

        let vfs = self
            .server
            .vfs
            .unix_socket(Into::into(&req.path), req.key.as_deref())
            .await?;
        Ok(ResponseKind::UnixVfs(OpenVfsHandle::Opaque(
            context.register(RetainedVfs::plain(vfs)),
        )))
    }

    async fn handle_windows_admin(
        &self,
        context: &CallContext<VfsProtocol>,
        req: WindowsAdminRequest,
    ) -> Result<ResponseKind> {
        let vfs = self
            .server
            .vfs
            .windows_admin(Into::into(&req.cwd), req.env, req.elevate)
            .await?;
        Ok(ResponseKind::WindowsAdmin(
            context.register(RetainedVfs::plain(vfs)),
        ))
    }

    async fn handle_remove(&self, req: RemoveRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .remove(Into::into(&req.path), req.all, req.ignore)
            .await?;
        Ok(ResponseKind::Remove)
    }

    async fn handle_metadata(&self, req: MetadataRequest) -> Result<ResponseKind> {
        let metadata = self.server.vfs.metadata(Into::into(&req.path)).await?;
        Ok(ResponseKind::Metadata(metadata))
    }

    async fn handle_fs_metadata(&self, req: FsMetadataRequest) -> Result<ResponseKind> {
        let metadata = self
            .server
            .vfs
            .fs_metadata(Into::into(&req.path), req.follow)
            .await?;
        Ok(ResponseKind::FsMetadata(metadata))
    }

    async fn handle_sec_desc(&self, req: SecDescRequest) -> Result<ResponseKind> {
        let sec_desc = self
            .server
            .vfs
            .sec_desc(Into::into(&req.path), req.mask, req.follow)
            .await?;
        Ok(ResponseKind::SecDesc(sec_desc))
    }

    async fn handle_acl(&self, req: AclRequest) -> Result<ResponseKind> {
        let acl = self
            .server
            .vfs
            .acl(Into::into(&req.path), req.kind, req.default, req.follow)
            .await?;
        Ok(ResponseKind::Acl(acl))
    }

    async fn handle_set_acl(&self, req: SetAclRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .set_acl(
                Into::into(&req.path),
                req.kind,
                req.acl.as_ref(),
                req.default,
                req.follow,
            )
            .await?;
        Ok(ResponseKind::SetAcl)
    }

    async fn handle_set_sec_desc(&self, req: SetSecDescRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .set_sec_desc(Into::into(&req.path), &req.sec_desc, req.follow)
            .await?;
        Ok(ResponseKind::SetSecDesc)
    }

    async fn handle_create_dir(&self, req: CreateDirRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .create_dir(Into::into(&req.path), req.all)
            .await?;
        Ok(ResponseKind::CreateDir)
    }

    async fn handle_remove_dir(&self, req: RemoveDirRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .remove_dir(Into::into(&req.path), req.all, req.ignore)
            .await?;
        Ok(ResponseKind::RemoveDir)
    }

    async fn handle_copy(&self, req: CopyRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .copy(Into::into(&req.from), Into::into(&req.to), req.all)
            .await?;
        Ok(ResponseKind::Copy)
    }

    async fn handle_rename(&self, req: RenameRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .rename(Into::into(&req.from), Into::into(&req.to), req.replace)
            .await?;
        Ok(ResponseKind::Rename)
    }

    async fn handle_move(&self, req: MoveRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .move_(Into::into(&req.from), Into::into(&req.to), req.all)
            .await?;
        Ok(ResponseKind::Move)
    }

    async fn handle_symlink(&self, req: SymlinkRequest) -> Result<ResponseKind> {
        match req.kind {
            SymlinkKind::Infer => {
                self.server
                    .vfs
                    .symlink(
                        Into::into(&req.cwd),
                        Into::into(&req.src),
                        Into::into(&req.dst),
                    )
                    .await
            }
            SymlinkKind::Dir => {
                self.server
                    .vfs
                    .symlink_dir(Into::into(&req.src), Into::into(&req.dst))
                    .await
            }
            SymlinkKind::File => {
                self.server
                    .vfs
                    .symlink_file(Into::into(&req.src), Into::into(&req.dst))
                    .await
            }
        }?;
        Ok(ResponseKind::Symlink)
    }

    async fn handle_hard_link(&self, req: HardLinkRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .hard_link(Into::into(&req.src), Into::into(&req.dst))
            .await?;
        Ok(ResponseKind::HardLink)
    }

    async fn handle_symlink_metadata(&self, req: MetadataRequest) -> Result<ResponseKind> {
        let metadata = self
            .server
            .vfs
            .symlink_metadata(Into::into(&req.path))
            .await?;
        Ok(ResponseKind::SymlinkMetadata(metadata))
    }

    async fn handle_set_metadata(&self, req: SetMetadataRequest) -> Result<ResponseKind> {
        let paths: Vec<_> = req
            .paths
            .iter()
            .map(|path| Utf8TypedPath::from(path).to_path_buf())
            .collect();
        self.server.vfs.set_metadata(&paths, req.patch).await?;
        Ok(ResponseKind::SetMetadata)
    }

    async fn handle_canonicalize(&self, req: CanonicalizeRequest) -> Result<ResponseKind> {
        let path = self
            .server
            .vfs
            .canonicalize(Into::into(&req.path))
            .await?
            .into();
        Ok(ResponseKind::Canonicalize(path))
    }

    async fn handle_read_link(&self, req: ReadLinkRequest) -> Result<ResponseKind> {
        let path = self
            .server
            .vfs
            .read_link(Into::into(&req.path))
            .await?
            .into();
        Ok(ResponseKind::ReadLink(path))
    }

    async fn handle_access(&self, req: AccessRequest) -> Result<ResponseKind> {
        let mode = AccessFlags::from_bits(req.mode).unwrap_or(AccessFlags::empty());
        self.server.vfs.access(Into::into(&req.path), mode).await?;
        Ok(ResponseKind::Access)
    }

    async fn handle_glob(&self, req: GlobRequest) -> Result<ResponseKind> {
        let paths = self
            .server
            .vfs
            .glob(
                req.pattern,
                Into::into(&req.root),
                req.follow_symlinks,
                req.max_depth,
            )
            .await?
            .into_iter()
            .map(Into::into)
            .collect();
        Ok(ResponseKind::Glob(paths))
    }

    async fn handle_xattrs(&self, req: XattrsRequest) -> Result<ResponseKind> {
        let xattrs = self
            .server
            .vfs
            .xattrs(
                Into::into(&req.path),
                req.namespace.as_borrowed(),
                req.follow,
            )
            .await?;
        Ok(ResponseKind::Xattrs(xattrs))
    }

    async fn handle_xattr(&self, req: XattrRequest) -> Result<ResponseKind> {
        let xattr = self
            .server
            .vfs
            .xattr(
                Into::into(&req.path),
                &req.name,
                req.namespace.as_deref(),
                req.follow,
            )
            .await?;
        Ok(ResponseKind::Xattr(xattr))
    }

    async fn handle_set_xattr(&self, req: SetXattrRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .set_xattr(
                Into::into(&req.path),
                &req.name,
                req.namespace.as_deref(),
                &req.value,
                req.follow,
            )
            .await?;
        Ok(ResponseKind::SetXattr)
    }

    async fn handle_remove_xattr(&self, req: XattrRequest) -> Result<ResponseKind> {
        self.server
            .vfs
            .remove_xattr(
                Into::into(&req.path),
                &req.name,
                req.namespace.as_deref(),
                req.follow,
            )
            .await?;
        Ok(ResponseKind::RemoveXattr)
    }

    async fn handle_streams(&self, req: StreamsRequest) -> Result<ResponseKind> {
        Ok(ResponseKind::Streams(
            self.server
                .vfs
                .streams(Into::into(&req.path), req.follow)
                .await?,
        ))
    }
}

/// Reports a handoff that did not happen.
///
/// The handle it carries back is dropped here rather than restored: the
/// registration was already retired to take it, and a fresh one would answer to
/// an id the peer does not hold. The peer's handle is dead either way, so the
/// file is closed instead of leaked. The one failure this side *can* recover
/// from — a busy file — is caught before the file is taken at all, in
/// [`take_file`](Session::take_file).
fn handoff_error<H>(error: HandoffError<H>) -> Error {
    Into::into(error.into_error())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::report_handler_exit;
    use super::{Server, orderly_disconnect};
    use crate::{
        error::{Error, ErrorKind},
        path,
        protocol::{
            self, OpenFlags, OpenHandle, OpenRequest, Request, RequestKind, ResponseKind,
            VfsProtocol,
        },
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
            Ok::<(), Error>(())
        });
        report_handler_exit(handlers.join_next().await.unwrap());
    }

    #[test]
    fn orderly_connection_close_is_expected() {
        assert!(orderly_disconnect(&Error::new(
            ErrorKind::ConnectionReset,
            "closed"
        )));
        assert!(orderly_disconnect(&Error::new(
            ErrorKind::BrokenPipe,
            "closed"
        )));
        assert!(!orderly_disconnect(&Error::new(
            ErrorKind::InvalidData,
            "bad frame"
        )));
    }

    #[tokio::test]
    async fn remote_server_replies_without_serializing_a_handle() {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server =
            tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
        let client = protocol::rpc_builder(None)
            .client(client_stream)
            .await
            .unwrap()
            .bind::<VfsProtocol>();

        let temp = tempfile::NamedTempFile::new().unwrap();
        let response = client
            .call(request(RequestKind::Open(OpenRequest {
                path: path::typed_path(temp.path().to_path_buf())
                    .unwrap()
                    .to_path()
                    .into(),
                flags: OpenFlags::READ,
            })))
            .await
            .unwrap()
            .into_response()
            .unwrap();
        let ResponseKind::Open(OpenHandle::Opaque(file)) = response else {
            panic!("remote open did not return an opaque file");
        };
        let ResponseKind::FileClose = client
            .call(request(RequestKind::FileClose { file: file.cite() }))
            .await
            .unwrap()
            .into_response()
            .unwrap()
        else {
            panic!("file close returned the wrong response");
        };
        let error = client
            .call(request(RequestKind::FileClose { file: file.cite() }))
            .await
            .unwrap()
            .into_response()
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        let _ = client.call(request(RequestKind::Stop)).await.unwrap();
        client.close().await;
        server.await.unwrap().unwrap();
    }
}
