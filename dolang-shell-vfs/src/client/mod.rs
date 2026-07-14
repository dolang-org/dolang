use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(unix)]
use std::os::unix::{
    io::{AsFd, OwnedFd},
    net::UnixStream as StdUnixStream,
};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, OwnedHandle};

use dolang_rpc::{Call, DefaultHandle, Opaque, OsHandle};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(unix)]
use crate::protocol::{AccessRequest, UnixStreamSocketRequest};
use crate::{
    Attrs, Child, ChownIdentity, Command, FileHandle, FsMetadata, Metadata, Permissions, PipeRecv,
    PipeSend, ProcessStatus, Query, ReadDir, SessionMode, StreamEntry, Utf8TypedPath,
    Utf8TypedPathBuf, Vfs, WellKnownPath, XattrEntry,
    direct::DirectFile,
    protocol::{
        AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest, CreateDirRequest,
        FsMetadataRequest, GlobRequest, HardLinkRequest, MetadataRequest, MoveRequest, OpenHandle,
        OpenHandlePreference, OpenRequest, QueryResponse, ReadLinkRequest, RemoveDirRequest,
        RemoveRequest, RenameRequest, RequestKind, ResponseKind, SetAttrsRequest,
        SetPermissionsRequest, SetTimesRequest, SetXattrRequest, SpawnRequest, StreamsRequest,
        SymlinkKind, SymlinkRequest, Timestamp, VfsProtocol, WellKnownPathRequest, WirePath,
        XattrRequest, XattrsRequest,
    },
};

/// Client for connecting to the agent daemon and spawning processes.
#[derive(Clone)]
pub struct Client {
    rpc: dolang_rpc::Client<VfsProtocol>,
    mode: SessionMode,
}

pub struct ClientFile(ClientFileInner);

enum ClientFileInner {
    Direct(DirectFile),
    Remote(RemoteFile),
}

struct RemoteFile {
    client: Client,
    file: Opaque<crate::FileMarker>,
    pending: Option<PendingFileOperation>,
}

struct PendingFileOperation {
    kind: FileOperationKind,
    call: Call<ResponseKind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileOperationKind {
    Read,
    Write,
    Flush,
    Seek,
}

impl std::fmt::Debug for ClientFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ClientFile").field(&self.0).finish()
    }
}

impl std::fmt::Debug for ClientFileInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct(file) => file.fmt(f),
            Self::Remote(file) => file.fmt(f),
        }
    }
}

impl std::fmt::Debug for RemoteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFile")
            .field("file", &self.file)
            .field(
                "pending",
                &self.pending.as_ref().map(|pending| pending.kind),
            )
            .finish_non_exhaustive()
    }
}

impl ClientFile {
    fn from_std(file: std::fs::File) -> Self {
        Self(ClientFileInner::Direct(DirectFile::from_std(file)))
    }

    fn from_remote(client: Client, file: Opaque<crate::FileMarker>) -> Self {
        Self(ClientFileInner::Remote(RemoteFile {
            client,
            file,
            pending: None,
        }))
    }
}

impl RemoteFile {
    fn poll_request(
        &mut self,
        cx: &mut Context<'_>,
        kind: FileOperationKind,
        request: impl FnOnce(Opaque<crate::FileMarker>) -> RequestKind,
    ) -> Poll<io::Result<ResponseKind>> {
        if self.pending.is_none() {
            self.pending = Some(PendingFileOperation {
                kind,
                call: self.client.call(request(self.file)),
            });
        }
        let pending = self.pending.as_mut().unwrap();
        if pending.kind != kind {
            return Poll::Ready(Err(io::Error::other(format!(
                "file operation {:?} polled while {:?} is pending",
                kind, pending.kind
            ))));
        }
        match Pin::new(&mut pending.call).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                Poll::Ready(result.map_err(rpc_error))
            }
        }
    }

    fn idle(&self) -> crate::Result<()> {
        if let Some(pending) = &self.pending {
            Err(io::Error::other(format!(
                "file operation {:?} is still pending",
                pending.kind
            ))
            .into())
        } else {
            Ok(())
        }
    }

    async fn cancel_pending(&mut self) {
        if let Some(mut pending) = self.pending.take() {
            pending.call.cancel();
            let _ = pending.call.await;
        }
    }
}

impl AsyncRead for ClientFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => Pin::new(file).poll_read(cx, buf),
            ClientFileInner::Remote(file) => {
                if buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
                match file.poll_request(cx, FileOperationKind::Read, |file| RequestKind::FileRead {
                    file,
                    len: buf.remaining(),
                }) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(ResponseKind::FileRead(result))) => {
                        Poll::Ready(result.map_err(wire_io).and_then(|data| {
                            if data.len() > buf.remaining() {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "file read response exceeds requested length",
                                ));
                            }
                            buf.put_slice(&data);
                            Ok(())
                        }))
                    }
                    Poll::Ready(Ok(response)) => Poll::Ready(Err(unexpected(response))),
                    Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                }
            }
        }
    }
}

impl AsyncWrite for ClientFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => Pin::new(file).poll_write(cx, buf),
            ClientFileInner::Remote(file) => {
                if buf.is_empty() {
                    return Poll::Ready(Ok(0));
                }
                match file.poll_request(cx, FileOperationKind::Write, |file| {
                    RequestKind::FileWrite {
                        file,
                        data: buf.to_vec(),
                    }
                }) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(ResponseKind::FileWrite(result))) => {
                        Poll::Ready(result.map_err(wire_io).and_then(|written| {
                            if written > buf.len() {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "file write response exceeds submitted length",
                                ));
                            }
                            Ok(written)
                        }))
                    }
                    Poll::Ready(Ok(response)) => Poll::Ready(Err(unexpected(response))),
                    Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => Pin::new(file).poll_flush(cx),
            ClientFileInner::Remote(file) => {
                match file.poll_request(cx, FileOperationKind::Flush, |file| {
                    RequestKind::FileFlush { file }
                }) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(ResponseKind::FileFlush(result))) => {
                        Poll::Ready(result.map_err(wire_io))
                    }
                    Poll::Ready(Ok(response)) => Poll::Ready(Err(unexpected(response))),
                    Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                }
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => Pin::new(file).poll_shutdown(cx),
            ClientFileInner::Remote(_) => self.as_mut().poll_flush(cx),
        }
    }
}

impl AsyncSeek for ClientFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => Pin::new(file).start_seek(position),
            ClientFileInner::Remote(file) => {
                file.idle().map_err(crate::Error::into_io_error)?;
                file.pending = Some(PendingFileOperation {
                    kind: FileOperationKind::Seek,
                    call: file.client.call(RequestKind::FileSeek {
                        file: file.file,
                        position: position.into(),
                    }),
                });
                Ok(())
            }
        }
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => Pin::new(file).poll_complete(cx),
            ClientFileInner::Remote(file) => {
                match file.poll_request(cx, FileOperationKind::Seek, |file| RequestKind::FileSeek {
                    file,
                    position: io::SeekFrom::Current(0).into(),
                }) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(ResponseKind::FileSeek(result))) => {
                        Poll::Ready(result.map_err(wire_io))
                    }
                    Poll::Ready(Ok(response)) => Poll::Ready(Err(unexpected(response))),
                    Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                }
            }
        }
    }
}

impl FileHandle for ClientFile {
    async fn try_clone(&self) -> crate::Result<Self> {
        match &self.0 {
            ClientFileInner::Direct(file) => file
                .try_clone()
                .await
                .map(ClientFileInner::Direct)
                .map(Self),
            ClientFileInner::Remote(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cloning opaque files is not supported",
            )
            .into()),
        }
    }

    async fn close(self) -> crate::Result<()> {
        match self.0 {
            ClientFileInner::Direct(file) => file.close().await,
            ClientFileInner::Remote(mut file) => {
                file.cancel_pending().await;
                match file
                    .client
                    .request(RequestKind::FileClose { file: file.file })
                    .await?
                {
                    ResponseKind::FileClose(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn set_len(&mut self, size: u64) -> crate::Result<()> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.set_len(size).await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileSetLen {
                        file: file.file,
                        len: size,
                    })
                    .await?
                {
                    ResponseKind::FileSetLen(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn metadata(&mut self) -> crate::Result<Metadata> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.metadata().await,
            ClientFileInner::Remote(_) => unsupported_file_operation("file metadata"),
        }
    }

    async fn fs_metadata(&mut self) -> crate::Result<FsMetadata> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.fs_metadata().await,
            ClientFileInner::Remote(_) => unsupported_file_operation("filesystem metadata"),
        }
    }

    async fn xattrs(
        &mut self,
        namespace: crate::XattrNamespace<'_>,
    ) -> crate::Result<Vec<XattrEntry>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.xattrs(namespace).await,
            ClientFileInner::Remote(_) => unsupported_file_operation("extended attributes"),
        }
    }

    async fn xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<Vec<u8>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.xattr(name, namespace).await,
            ClientFileInner::Remote(_) => unsupported_file_operation("extended attributes"),
        }
    }

    async fn streams(&mut self) -> crate::Result<Vec<StreamEntry>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.streams().await,
            ClientFileInner::Remote(_) => unsupported_file_operation("alternate streams"),
        }
    }

    async fn set_xattr(
        &mut self,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> crate::Result<()> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.set_xattr(name, namespace, value).await,
            ClientFileInner::Remote(_) => unsupported_file_operation("extended attributes"),
        }
    }

    async fn remove_xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<()> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.remove_xattr(name, namespace).await,
            ClientFileInner::Remote(_) => unsupported_file_operation("extended attributes"),
        }
    }

    async fn try_into_std(self) -> std::result::Result<std::fs::File, Self> {
        match self.0 {
            ClientFileInner::Direct(file) => file
                .try_into_std()
                .await
                .map_err(|file| Self(ClientFileInner::Direct(file))),
            ClientFileInner::Remote(file) => Err(Self(ClientFileInner::Remote(file))),
        }
    }
}

fn unsupported_file_operation<T>(operation: &str) -> crate::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{operation} is not supported for opaque files"),
    )
    .into())
}

fn wire_io(error: crate::protocol::WireError) -> io::Error {
    crate::Error::from(error).into_io_error()
}

impl Client {
    /// Starts an opaque-only VFS client on a bidirectional byte stream.
    pub fn new<T>(stream: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            rpc: dolang_rpc::Client::new(stream),
            mode: SessionMode::Remote,
        }
    }

    /// Connect to an agent daemon at the given socket path.
    #[cfg(unix)]
    pub async fn connect(path: impl AsRef<Path>) -> crate::Result<Self> {
        Self::from_stream(UnixStream::connect(path).await?).await
    }

    /// Connect to an agent using opaque-only generic framing over a Unix socket.
    #[cfg(unix)]
    pub async fn connect_remote(path: impl AsRef<Path>) -> crate::Result<Self> {
        Ok(Self::new(UnixStream::connect(path).await?))
    }

    /// Connect using an existing `UnixStream`.
    #[cfg(unix)]
    pub async fn from_stream(stream: UnixStream) -> crate::Result<Self> {
        Self::from_std_stream(stream.into_std()?)
    }

    #[cfg(unix)]
    fn from_std_stream(stream: StdUnixStream) -> crate::Result<Self> {
        let rpc = dolang_rpc::Client::from_unix_stream(stream)?;
        Ok(Self {
            rpc,
            mode: SessionMode::Native,
        })
    }

    /// Use an owned Unix socket descriptor with opaque-only generic framing.
    #[cfg(unix)]
    pub fn from_remote_fd(value: OwnedFd) -> crate::Result<Self> {
        let stream = StdUnixStream::from(value);
        stream.set_nonblocking(true)?;
        Ok(Self::new(UnixStream::from_std(stream)?))
    }

    /// Starts a VFS client on the server end of a connected Windows named pipe.
    ///
    /// # Safety
    ///
    /// `server_process` must identify the trusted process at the other end of
    /// the pipe. That process can transfer handles which this process adopts.
    #[cfg(windows)]
    pub unsafe fn from_named_pipe_server(
        pipe: NamedPipeServer,
        server_process: OwnedHandle,
    ) -> crate::Result<Self> {
        let rpc = unsafe { dolang_rpc::Client::from_named_pipe_server(pipe, server_process)? };
        Ok(Self {
            rpc,
            mode: SessionMode::Native,
        })
    }

    fn unsupported<T>(&self, operation: &str) -> crate::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{operation} is not supported by a remote VFS session"),
        )
        .into())
    }

    fn call(&self, request: RequestKind) -> Call<ResponseKind> {
        self.rpc.call(request)
    }

    async fn request(&self, request: RequestKind) -> crate::Result<ResponseKind> {
        self.call(request)
            .await
            .map_err(rpc_error)
            .map_err(Into::into)
    }

    /// Create a Unix stream socket in the agent namespace and return its descriptor.
    ///
    /// If `bind` is provided, the socket is bound to that pathname first. If `connect`
    /// is provided, the socket is then connected to that pathname. Either or both may
    /// be omitted.
    #[cfg(unix)]
    pub async fn unix_stream_socket<B, C>(
        &self,
        bind: Option<B>,
        connect: Option<C>,
    ) -> crate::Result<OwnedFd>
    where
        B: AsRef<Path>,
        C: AsRef<Path>,
    {
        if self.mode == SessionMode::Remote {
            return self.unsupported("Unix stream sockets");
        }
        let req = UnixStreamSocketRequest {
            bind: bind
                .map(|p| WirePath::try_from(p.as_ref().to_path_buf()))
                .transpose()?,
            connect: connect
                .map(|p| WirePath::try_from(p.as_ref().to_path_buf()))
                .transpose()?,
        };

        match self.request(RequestKind::UnixStreamSocket(req)).await? {
            ResponseKind::UnixStreamSocket(result) => {
                result.map(OsHandle::into_inner).map_err(crate::Error::from)
            }
            response => Err(unexpected(response).into()),
        }
    }

    /// Check file accessibility.
    ///
    /// Mode is a bitmask of accessibility flags from [`AccessFlags`](crate::AccessFlags):
    /// - `AccessFlags::F_OK`: Test for existence
    /// - `AccessFlags::R_OK`: Test for read permission
    /// - `AccessFlags::W_OK`: Test for write permission
    /// - `AccessFlags::X_OK`: Test for execute permission
    #[cfg(unix)]
    pub async fn access(
        &self,
        path: impl AsRef<Path>,
        mode: crate::AccessFlags,
    ) -> crate::Result<()> {
        let request = AccessRequest {
            path: path.as_ref().to_path_buf().try_into()?,
            mode: mode.bits(),
        };
        match self.request(RequestKind::Access(request)).await? {
            ResponseKind::Access(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    /// Query the daemon's environment variables and current working directory.
    pub async fn query(&self) -> crate::Result<Query> {
        match self.request(RequestKind::Query).await? {
            ResponseKind::Query(result) => result
                .map(|QueryResponse { env, cwd, target }| Query {
                    env,
                    cwd: cwd.into(),
                    target,
                })
                .map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    /// Resolve a program path using the daemon's PATH resolution.
    pub async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> crate::Result<Option<PathBuf>> {
        let request = RequestKind::Which {
            program: program.as_ref().to_path_buf().try_into()?,
            path: path.map(str::to_owned),
            cwd: cwd
                .map(|path| WirePath::try_from(path.to_path_buf()))
                .transpose()?,
        };
        match self.request(request).await? {
            ResponseKind::Which(result) => result
                .map_err(crate::Error::from)?
                .map(TryInto::try_into)
                .transpose(),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> crate::Result<PathBuf> {
        let request = WellKnownPathRequest {
            key,
            env: env.clone(),
        };
        match self.request(RequestKind::WellKnownPath(request)).await? {
            ResponseKind::WellKnownPath(result) => result.map_err(crate::Error::from)?.try_into(),
            response => Err(unexpected(response).into()),
        }
    }

    /// Signal the daemon to stop accepting new connections.
    pub async fn stop(&self) -> crate::Result<()> {
        match self.request(RequestKind::Stop).await? {
            ResponseKind::Stop => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    /// Clear the server's path resolution cache.
    pub async fn clear_cache(&self) -> crate::Result<()> {
        match self.request(RequestKind::ClearCache).await? {
            ResponseKind::ClearCache(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }
}

#[cfg(unix)]
impl TryFrom<OwnedFd> for Client {
    type Error = crate::Error;

    fn try_from(value: OwnedFd) -> Result<Self, Self::Error> {
        let stream = StdUnixStream::from(value);
        stream.set_nonblocking(true)?;
        Self::from_std_stream(stream)
    }
}

fn rpc_error(error: dolang_rpc::Error) -> io::Error {
    match error {
        dolang_rpc::Error::Io(error) => error,
        dolang_rpc::Error::ConnectionClosed => {
            io::Error::new(io::ErrorKind::ConnectionReset, error.to_string())
        }
        dolang_rpc::Error::Cancelled => {
            io::Error::new(io::ErrorKind::Interrupted, error.to_string())
        }
        error => io::Error::other(error),
    }
}

fn unexpected(response: ResponseKind) -> io::Error {
    io::Error::other(format!("unexpected RPC response: {response:?}"))
}

fn clone_stdin_handle() -> io::Result<DefaultHandle> {
    #[cfg(unix)]
    {
        std::io::stdin().as_fd().try_clone_to_owned()
    }
    #[cfg(windows)]
    {
        std::io::stdin().as_handle().try_clone_to_owned()
    }
}

fn clone_stdout_handle() -> io::Result<DefaultHandle> {
    #[cfg(unix)]
    {
        std::io::stdout().as_fd().try_clone_to_owned()
    }
    #[cfg(windows)]
    {
        std::io::stdout().as_handle().try_clone_to_owned()
    }
}

fn clone_stderr_handle() -> io::Result<DefaultHandle> {
    #[cfg(unix)]
    {
        std::io::stderr().as_fd().try_clone_to_owned()
    }
    #[cfg(windows)]
    {
        std::io::stderr().as_handle().try_clone_to_owned()
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
    program: WirePath,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<WirePath>,
    stdin_handle: Option<DefaultHandle>,
    stdout_handle: Option<DefaultHandle>,
    stderr_handle: Option<DefaultHandle>,
    stdin_file: Option<ClientFile>,
    stdout_file: Option<ClientFile>,
    stderr_file: Option<ClientFile>,
}

pub struct ClientChild<'a> {
    inner: Option<Call<ResponseKind>>,
    marker: std::marker::PhantomData<&'a Client>,
}

impl<'a> CommandBuilder<'a> {
    fn new(client: &'a Client, program: Utf8TypedPath<'_>) -> Self {
        Self {
            client,
            program: program.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin_handle: None,
            stdout_handle: None,
            stderr_handle: None,
            stdin_file: None,
            stdout_file: None,
            stderr_file: None,
        }
    }
}

impl Child for ClientChild<'_> {
    async fn wait(&mut self) -> crate::Result<ProcessStatus> {
        let result = match self.inner.take() {
            Some(inner) => inner.await.map_err(rpc_error).map_err(crate::Error::from),
            None => return Err(io::Error::other("child already waited").into()),
        }?;
        match result {
            ResponseKind::Spawn(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn terminate(self) -> crate::Result<ProcessStatus> {
        let Some(mut inner) = self.inner else {
            return Err(io::Error::other("child already waited").into());
        };
        inner.cancel();
        match inner.await.map_err(rpc_error).map_err(crate::Error::from)? {
            ResponseKind::Spawn(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }
}

impl<'a> Command for CommandBuilder<'a> {
    type Child = ClientChild<'a>;
    type File = ClientFile;
    type PipeSend = PipeSend;
    type PipeRecv = PipeRecv;

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

    fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        self.stdin_file = None;
        self.stdin_handle = Some(pipe.into_blocking_handle()?);
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stdout_file = None;
        self.stdout_handle = Some(pipe.into_blocking_handle()?);
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdin_file = None;
        self.stdin_handle = Some(clone_stdin_handle()?);
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdout_file = None;
        self.stdout_handle = Some(clone_stdout_handle()?);
        Ok(self)
    }

    fn stdin_handle(&mut self, handle: ClientFile) -> io::Result<&mut Self> {
        self.stdin_handle = None;
        self.stdin_file = Some(handle);
        Ok(self)
    }

    fn stdout_handle(&mut self, handle: ClientFile) -> io::Result<&mut Self> {
        self.stdout_handle = None;
        self.stdout_file = Some(handle);
        Ok(self)
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.stdin_file = None;
        self.stdin_handle = None;
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.stdout_file = None;
        self.stdout_handle = None;
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stderr_file = None;
        self.stderr_handle = Some(pipe.into_blocking_handle()?);
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.stderr_file = None;
        self.stderr_handle = Some(clone_stderr_handle()?);
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.stderr_file = None;
        self.stderr_handle = Some(clone_stdout_handle()?);
        Ok(self)
    }

    fn stderr_handle(&mut self, handle: ClientFile) -> io::Result<&mut Self> {
        self.stderr_handle = None;
        self.stderr_file = Some(handle);
        Ok(self)
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.stderr_file = None;
        self.stderr_handle = None;
        self
    }

    async fn spawn(mut self) -> crate::Result<Self::Child> {
        if self.client.mode == SessionMode::Remote {
            return self.client.unsupported("process spawning");
        }
        if let Some(file) = self.stdin_file.take() {
            self.stdin_handle = Some(file.try_into_std().await.unwrap().into());
        }
        if let Some(file) = self.stdout_file.take() {
            self.stdout_handle = Some(file.try_into_std().await.unwrap().into());
        }
        if let Some(file) = self.stderr_file.take() {
            self.stderr_handle = Some(file.try_into_std().await.unwrap().into());
        }
        let req = SpawnRequest {
            program: self.program,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            stdin_fd: self.stdin_handle.map(OsHandle::new),
            stdout_fd: self.stdout_handle.map(OsHandle::new),
            stderr_fd: self.stderr_handle.map(OsHandle::new),
        };
        Ok(ClientChild {
            inner: Some(self.client.call(RequestKind::Spawn(req))),
            marker: std::marker::PhantomData,
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

    async fn open_wire(&self, path: WirePath) -> crate::Result<ClientFile> {
        let req = OpenRequest {
            path,
            read: self.read,
            write: self.write,
            append: self.append,
            create: self.create,
            create_new: self.create_new,
            truncate: self.truncate,
            no_follow: self.no_follow,
            handle_preference: if self.client.mode == SessionMode::Remote {
                OpenHandlePreference::Opaque
            } else {
                OpenHandlePreference::NativePreferred
            },
        };

        match self.client.request(RequestKind::Open(req)).await? {
            ResponseKind::Open(result) => match result.map_err(crate::Error::from)? {
                OpenHandle::Native(handle) => Ok(ClientFile::from_std(handle.into_inner().into())),
                OpenHandle::Opaque(file) => Ok(ClientFile::from_remote(self.client.clone(), file)),
            },
            response => Err(unexpected(response).into()),
        }
    }

    /// Open the file at the given path.
    pub async fn open(&self, path: impl AsRef<Path>) -> crate::Result<ClientFile> {
        self.open_wire(path.as_ref().to_path_buf().try_into()?)
            .await
    }
}

impl crate::OpenOptions for OpenOptions<'_> {
    type File = ClientFile;

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

    async fn open(&self, path: Utf8TypedPath<'_>) -> crate::Result<ClientFile> {
        self.open_wire(path.into()).await
    }
}

impl Vfs for Client {
    type File = ClientFile;
    type PipeSend = PipeSend;
    type PipeRecv = PipeRecv;
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

    fn command(&self, program: Utf8TypedPath<'_>) -> Self::Command<'_> {
        CommandBuilder::new(self, program)
    }

    fn pipe(&self) -> io::Result<(PipeSend, PipeRecv)> {
        crate::pipe::pipe()
    }

    async fn query(&self) -> crate::Result<Query> {
        Client::query(self).await
    }

    async fn read_dir(&self, path: Utf8TypedPath<'_>) -> crate::Result<ReadDir> {
        if self.mode == SessionMode::Remote {
            return self.unsupported("reading directories");
        }
        match self
            .request(RequestKind::ReadDir { path: path.into() })
            .await?
        {
            ResponseKind::ReadDir(result) => result
                .map(ReadDir::from_entries)
                .map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> crate::Result<Option<Utf8TypedPathBuf>> {
        let request = RequestKind::Which {
            program: program.into(),
            path: path.map(str::to_owned),
            cwd: cwd.map(Into::into),
        };
        match self.request(request).await? {
            ResponseKind::Which(result) => result
                .map(|path| path.map(Into::into))
                .map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> crate::Result<Utf8TypedPathBuf> {
        let request = WellKnownPathRequest {
            key,
            env: env.clone(),
        };
        match self.request(RequestKind::WellKnownPath(request)).await? {
            ResponseKind::WellKnownPath(result) => {
                result.map(Into::into).map_err(crate::Error::from)
            }
            response => Err(unexpected(response).into()),
        }
    }

    async fn clear_cache(&self) -> crate::Result<()> {
        Client::clear_cache(self).await
    }

    async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: crate::XattrNamespace<'_>,
        follow: bool,
    ) -> crate::Result<Vec<XattrEntry>> {
        let request = XattrsRequest {
            path: path.into(),
            namespace: namespace.into(),
            follow,
        };
        match self.request(RequestKind::Xattrs(request)).await? {
            ResponseKind::Xattrs(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn streams(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> crate::Result<Vec<StreamEntry>> {
        let request = StreamsRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::Streams(request)).await? {
            ResponseKind::Streams(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> crate::Result<Vec<u8>> {
        let request = XattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            follow,
        };
        match self.request(RequestKind::Xattr(request)).await? {
            ResponseKind::Xattr(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> crate::Result<()> {
        let request = SetXattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            value: value.to_vec(),
            follow,
        };
        match self.request(RequestKind::SetXattr(request)).await? {
            ResponseKind::SetXattr(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> crate::Result<()> {
        let request = XattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            follow,
        };
        match self.request(RequestKind::RemoveXattr(request)).await? {
            ResponseKind::RemoveXattr(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn remove(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> crate::Result<()> {
        let request = RemoveRequest {
            path: path.into(),
            all,
            ignore,
        };
        match self.request(RequestKind::Remove(request)).await? {
            ResponseKind::Remove(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn metadata(&self, path: Utf8TypedPath<'_>) -> crate::Result<Metadata> {
        let request = MetadataRequest { path: path.into() };
        match self.request(RequestKind::Metadata(request)).await? {
            ResponseKind::Metadata(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn fs_metadata(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> crate::Result<FsMetadata> {
        let request = FsMetadataRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::FsMetadata(request)).await? {
            ResponseKind::FsMetadata(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> crate::Result<()> {
        let request = CreateDirRequest {
            path: path.into(),
            all,
        };
        match self.request(RequestKind::CreateDir(request)).await? {
            ResponseKind::CreateDir(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn remove_dir(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> crate::Result<()> {
        let request = RemoveDirRequest {
            path: path.into(),
            ignore,
            all,
        };
        match self.request(RequestKind::RemoveDir(request)).await? {
            ResponseKind::RemoveDir(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn copy(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> crate::Result<()> {
        let request = CopyRequest {
            from: from.into(),
            to: to.into(),
            all,
        };
        match self.request(RequestKind::Copy(request)).await? {
            ResponseKind::Copy(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn rename(&self, from: Utf8TypedPath<'_>, to: Utf8TypedPath<'_>) -> crate::Result<()> {
        let request = RenameRequest {
            from: from.into(),
            to: to.into(),
        };
        match self.request(RequestKind::Rename(request)).await? {
            ResponseKind::Rename(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn move_(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> crate::Result<()> {
        let request = MoveRequest {
            from: from.into(),
            to: to.into(),
            all,
        };
        match self.request(RequestKind::Move(request)).await? {
            ResponseKind::Move(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        let request = SymlinkRequest {
            cwd: cwd.into(),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::Infer,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn hard_link(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> crate::Result<()> {
        let request = HardLinkRequest {
            src: src.into(),
            dst: dst.into(),
        };
        match self.request(RequestKind::HardLink(request)).await? {
            ResponseKind::HardLink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink_dir(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        let request = SymlinkRequest {
            cwd: WirePath::empty_like(src),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::Dir,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink_file(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> crate::Result<()> {
        let request = SymlinkRequest {
            cwd: WirePath::empty_like(src),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::File,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> crate::Result<Metadata> {
        let request = MetadataRequest { path: path.into() };
        match self.request(RequestKind::SymlinkMetadata(request)).await? {
            ResponseKind::SymlinkMetadata(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn attrs(&self, path: Utf8TypedPath<'_>, follow: bool) -> crate::Result<Attrs> {
        let request = AttrsRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::Attrs(request)).await? {
            ResponseKind::Attrs(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_attrs(&self, path: Utf8TypedPath<'_>, attrs: Attrs) -> crate::Result<()> {
        let request = SetAttrsRequest {
            path: path.into(),
            attrs,
        };
        match self.request(RequestKind::SetAttrs(request)).await? {
            ResponseKind::SetAttrs(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> crate::Result<Utf8TypedPathBuf> {
        let request = CanonicalizeRequest { path: path.into() };
        match self.request(RequestKind::Canonicalize(request)).await? {
            ResponseKind::Canonicalize(result) => {
                result.map_err(crate::Error::from).map(Into::into)
            }
            response => Err(unexpected(response).into()),
        }
    }

    async fn read_link(&self, path: Utf8TypedPath<'_>) -> crate::Result<Utf8TypedPathBuf> {
        let request = ReadLinkRequest { path: path.into() };
        match self.request(RequestKind::ReadLink(request)).await? {
            ResponseKind::ReadLink(result) => result.map_err(crate::Error::from).map(Into::into),
            response => Err(unexpected(response).into()),
        }
    }

    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> crate::Result<Vec<Utf8TypedPathBuf>> {
        let request = GlobRequest {
            pattern: pattern.into(),
            root: root.into(),
            follow_symlinks,
            max_depth,
        };
        match self.request(RequestKind::Glob(request)).await? {
            ResponseKind::Glob(result) => Ok(result
                .map_err(crate::Error::from)?
                .into_iter()
                .map(Utf8TypedPathBuf::from)
                .collect()),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_permissions(
        &self,
        path: Utf8TypedPath<'_>,
        perm: Permissions,
    ) -> crate::Result<()> {
        let request = SetPermissionsRequest {
            path: path.into(),
            mode: perm.mode(),
        };
        match self.request(RequestKind::SetPermissions(request)).await? {
            ResponseKind::SetPermissions(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn set_times(
        &self,
        path: Utf8TypedPath<'_>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> crate::Result<()> {
        let request = SetTimesRequest {
            path: path.into(),
            accessed: accessed.map(|(secs, nanos)| Timestamp { secs, nanos }),
            modified: modified.map(|(secs, nanos)| Timestamp { secs, nanos }),
            created: created.map(|(secs, nanos)| Timestamp { secs, nanos }),
        };
        match self.request(RequestKind::SetTimes(request)).await? {
            ResponseKind::SetTimes(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }

    async fn chown(
        &self,
        path: Utf8TypedPath<'_>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> crate::Result<()> {
        let request = ChownRequest {
            path: path.into(),
            user,
            group,
            follow,
        };
        match self.request(RequestKind::Chown(request)).await? {
            ResponseKind::Chown(result) => result.map_err(crate::Error::from),
            response => Err(unexpected(response).into()),
        }
    }
}
