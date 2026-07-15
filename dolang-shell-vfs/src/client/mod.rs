use std::{
    collections::HashMap,
    future::Future,
    io,
    io::IsTerminal,
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
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::{
    io::{AsyncRead, AsyncSeek, AsyncWrite, AsyncWriteExt, ReadBuf},
    task::JoinHandle,
};

#[cfg(unix)]
use crate::protocol::{AccessRequest, UnixStreamSocketRequest};
use crate::{
    Attrs, Child, ChownIdentity, Command, FileHandle, FsMetadata, Metadata, Permissions,
    ProcessStatus, Query, ReadDir, SessionMode, StdioRecv, StdioSend, StreamEntry, Utf8TypedPath,
    Utf8TypedPathBuf, Vfs, WellKnownPath, XattrEntry,
    direct::DirectFile,
    protocol::{
        AttrsRequest, CanonicalizeRequest, ChownRequest, CopyRequest, CreateDirRequest,
        FsMetadataRequest, GlobRequest, HardLinkRequest, MetadataRequest, MoveRequest, OpenHandle,
        OpenHandlePreference, OpenRequest, QueryResponse, ReadDirResponse, ReadLinkRequest,
        RemoveDirRequest, RemoveRequest, RenameRequest, RequestKind, ResponseKind, SetAttrsRequest,
        SetPermissionsRequest, SetTimesRequest, SetXattrRequest, SpawnRequest, StdioRecvTarget,
        StdioSendTarget, StreamsRequest, SymlinkKind, SymlinkRequest, Timestamp, VfsProtocol,
        WellKnownPathRequest, WirePath, XattrNamespaceRequest, XattrRequest, XattrsRequest,
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
    file: Option<Opaque<crate::FileMarker>>,
    pending: Option<PendingFileOperation>,
}

struct PendingFileOperation {
    kind: FileOperationKind,
    call: Call<VfsProtocol>,
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
    fn from_std(file: std::fs::File, read: bool, write: bool, append: bool) -> Self {
        Self(ClientFileInner::Direct(DirectFile::from_std(
            file, read, write, append,
        )))
    }

    fn from_remote(client: Client, file: Opaque<crate::FileMarker>) -> Self {
        Self(ClientFileInner::Remote(RemoteFile {
            client,
            file: Some(file),
            pending: None,
        }))
    }
}

impl RemoteFile {
    fn opaque(&self) -> Opaque<crate::FileMarker> {
        self.file
            .as_ref()
            .expect("live remote file has no opaque identity")
            .clone()
    }

    fn poll_request(
        &mut self,
        cx: &mut Context<'_>,
        kind: FileOperationKind,
        request: impl FnOnce(Opaque<crate::FileMarker>) -> RequestKind,
    ) -> Poll<io::Result<ResponseKind>> {
        if self.pending.is_none() {
            self.pending = Some(PendingFileOperation {
                kind,
                call: self.client.call(request(self.opaque())),
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

impl Drop for RemoteFile {
    fn drop(&mut self) {
        self.pending.take();
        let Some(file) = self.file.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        runtime.spawn(async move {
            let _ = client.request(RequestKind::FileClose { file }).await;
        });
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
                        file: file.opaque(),
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
    async fn to_stdio_send(&self) -> crate::Result<StdioSend> {
        match &self.0 {
            ClientFileInner::Direct(file) => file.to_stdio_send().await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileToStdioSend {
                        file: file.opaque(),
                    })
                    .await?
                {
                    ResponseKind::FileToStdioSend(result) => result
                        .map(|stdio| {
                            StdioSend::Remote(RemoteStdioSend {
                                client: file.client.clone(),
                                stdio: Some(stdio),
                                pending: None,
                            })
                        })
                        .map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn to_stdio_recv(&self) -> crate::Result<StdioRecv> {
        match &self.0 {
            ClientFileInner::Direct(file) => file.to_stdio_recv().await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileToStdioRecv {
                        file: file.opaque(),
                    })
                    .await?
                {
                    ResponseKind::FileToStdioRecv(result) => result
                        .map(|stdio| {
                            StdioRecv::Remote(RemoteStdioRecv {
                                client: file.client.clone(),
                                stdio: Some(stdio),
                                pending: None,
                            })
                        })
                        .map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn close(self) -> crate::Result<()> {
        match self.0 {
            ClientFileInner::Direct(file) => file.close().await,
            ClientFileInner::Remote(mut file) => {
                file.cancel_pending().await;
                let opaque = file
                    .file
                    .take()
                    .expect("live remote file has no opaque identity");
                match file
                    .client
                    .request(RequestKind::FileClose { file: opaque })
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
                        file: file.opaque(),
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
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileMetadata {
                        file: file.opaque(),
                    })
                    .await?
                {
                    ResponseKind::FileMetadata(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn fs_metadata(&mut self) -> crate::Result<FsMetadata> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.fs_metadata().await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileFsMetadata {
                        file: file.opaque(),
                    })
                    .await?
                {
                    ResponseKind::FileFsMetadata(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn xattrs(
        &mut self,
        namespace: crate::XattrNamespace<'_>,
    ) -> crate::Result<Vec<XattrEntry>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.xattrs(namespace).await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileXattrs {
                        file: file.opaque(),
                        namespace: XattrNamespaceRequest::from(namespace),
                    })
                    .await?
                {
                    ResponseKind::FileXattrs(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<Vec<u8>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.xattr(name, namespace).await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileXattr {
                        file: file.opaque(),
                        name: name.to_owned(),
                        namespace: namespace.map(str::to_owned),
                    })
                    .await?
                {
                    ResponseKind::FileXattr(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn streams(&mut self) -> crate::Result<Vec<StreamEntry>> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.streams().await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileStreams {
                        file: file.opaque(),
                    })
                    .await?
                {
                    ResponseKind::FileStreams(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
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
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileSetXattr {
                        file: file.opaque(),
                        name: name.to_owned(),
                        namespace: namespace.map(str::to_owned),
                        value: value.to_vec(),
                    })
                    .await?
                {
                    ResponseKind::FileSetXattr(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
        }
    }

    async fn remove_xattr(&mut self, name: &str, namespace: Option<&str>) -> crate::Result<()> {
        match &mut self.0 {
            ClientFileInner::Direct(file) => file.remove_xattr(name, namespace).await,
            ClientFileInner::Remote(file) => {
                file.idle()?;
                match file
                    .client
                    .request(RequestKind::FileRemoveXattr {
                        file: file.opaque(),
                        name: name.to_owned(),
                        namespace: namespace.map(str::to_owned),
                    })
                    .await?
                {
                    ResponseKind::FileRemoveXattr(result) => result.map_err(Into::into),
                    response => Err(unexpected(response).into()),
                }
            }
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

    fn call(&self, request: RequestKind) -> Call<VfsProtocol> {
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
/// let child = client
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
    stdin: ClientRecv,
    stdout: ClientSend,
    stderr: ClientSend,
}

pub struct ClientChild {
    client: Client,
    state: ClientChildState,
    relays: ClientRelays,
}

#[derive(Default)]
struct ClientRelays {
    stdin: Option<JoinHandle<()>>,
    outputs: Vec<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum HostOutput {
    Stdout,
    Stderr,
}

#[derive(Default)]
struct PreparedRelays {
    stdin: Option<StdioSend>,
    outputs: Vec<(StdioRecv, HostOutput)>,
}

enum ClientChildState {
    Live(Opaque<crate::ChildMarker>),
    Exited(ProcessStatus),
    Lost(crate::protocol::WireError),
}

pub struct RemoteStdioSend {
    client: Client,
    stdio: Option<Opaque<crate::StdioSendMarker>>,
    pending: Option<(StdioSendOperation, Call<VfsProtocol>)>,
}

pub struct RemoteStdioRecv {
    client: Client,
    stdio: Option<Opaque<crate::StdioRecvMarker>>,
    pending: Option<Call<VfsProtocol>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StdioSendOperation {
    Write,
    Close,
}

impl std::fmt::Debug for RemoteStdioSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteStdioSend")
            .field("stdio", &self.stdio)
            .field("pending", &self.pending.as_ref().map(|p| p.0))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for RemoteStdioRecv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteStdioRecv")
            .field("stdio", &self.stdio)
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl AsyncWrite for RemoteStdioSend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.pending.is_none() {
            let Some(stdio) = &self.stdio else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "stdio send resource is closed",
                )));
            };
            self.pending = Some((
                StdioSendOperation::Write,
                self.client.call(RequestKind::StdioSendWrite {
                    stdio: stdio.clone(),
                    data: buf.to_vec(),
                }),
            ));
        }
        let (operation, call) = self.pending.as_mut().unwrap();
        if *operation != StdioSendOperation::Write {
            return Poll::Ready(Err(io::Error::other(
                "write polled while stdio send close is pending",
            )));
        }
        match Pin::new(call).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                match result.map_err(rpc_error)? {
                    ResponseKind::StdioSendWrite(result) => Poll::Ready(result.map_err(wire_io)),
                    response => Poll::Ready(Err(unexpected(response))),
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some((operation, call)) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        if *operation != StdioSendOperation::Write {
            return Poll::Ready(Err(io::Error::other(
                "flush polled while stdio send close is pending",
            )));
        }
        match Pin::new(call).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                match result.map_err(rpc_error)? {
                    ResponseKind::StdioSendWrite(result) => {
                        Poll::Ready(result.map(|_| ()).map_err(wire_io))
                    }
                    response => Poll::Ready(Err(unexpected(response))),
                }
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.stdio.is_none() {
            return Poll::Ready(Ok(()));
        }
        if self.pending.is_none() {
            let stdio = self.stdio.as_ref().unwrap().clone();
            self.pending = Some((
                StdioSendOperation::Close,
                self.client.call(RequestKind::StdioSendClose { stdio }),
            ));
        }
        let (operation, call) = self.pending.as_mut().unwrap();
        if *operation != StdioSendOperation::Close {
            return Poll::Ready(Err(io::Error::other(
                "shutdown polled while stdio send write is pending",
            )));
        }
        match Pin::new(call).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                match result.map_err(rpc_error)? {
                    ResponseKind::StdioSendClose(result) => match result.map_err(wire_io) {
                        Ok(()) => {
                            self.stdio.take();
                            Poll::Ready(Ok(()))
                        }
                        Err(error) => Poll::Ready(Err(error)),
                    },
                    response => Poll::Ready(Err(unexpected(response))),
                }
            }
        }
    }
}

impl AsyncRead for RemoteStdioRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pending.is_none() {
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            let Some(stdio) = &self.stdio else {
                return Poll::Ready(Ok(()));
            };
            self.pending = Some(self.client.call(RequestKind::StdioRecvRead {
                stdio: stdio.clone(),
                len: buf.remaining(),
            }));
        }
        match Pin::new(self.pending.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                match result.map_err(rpc_error)? {
                    ResponseKind::StdioRecvRead(result) => match result.map_err(wire_io) {
                        Ok(data) => {
                            buf.put_slice(&data);
                            Poll::Ready(Ok(()))
                        }
                        Err(error) => Poll::Ready(Err(error)),
                    },
                    response => Poll::Ready(Err(unexpected(response))),
                }
            }
        }
    }
}

async fn best_effort_close_stdio_send(client: Client, stdio: Opaque<crate::StdioSendMarker>) {
    for _ in 0..4 {
        let Ok(ResponseKind::StdioSendClose(result)) = client
            .request(RequestKind::StdioSendClose {
                stdio: stdio.clone(),
            })
            .await
        else {
            return;
        };
        match result {
            Ok(()) => return,
            Err(error)
                if crate::Error::from(error.clone()).kind() == io::ErrorKind::ResourceBusy =>
            {
                tokio::task::yield_now().await;
            }
            Err(_) => return,
        }
    }
}

async fn best_effort_close_stdio_recv(client: Client, stdio: Opaque<crate::StdioRecvMarker>) {
    for _ in 0..4 {
        let Ok(ResponseKind::StdioRecvClose(result)) = client
            .request(RequestKind::StdioRecvClose {
                stdio: stdio.clone(),
            })
            .await
        else {
            return;
        };
        match result {
            Ok(()) => return,
            Err(error)
                if crate::Error::from(error.clone()).kind() == io::ErrorKind::ResourceBusy =>
            {
                tokio::task::yield_now().await;
            }
            Err(_) => return,
        }
    }
}

impl Drop for RemoteStdioSend {
    fn drop(&mut self) {
        let Some(stdio) = self.stdio.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        runtime.spawn(best_effort_close_stdio_send(client, stdio));
    }
}

impl RemoteStdioSend {
    pub(crate) fn disarm_cleanup(&mut self) {
        self.stdio.take();
    }

    pub(crate) async fn try_clone(&self) -> io::Result<Self> {
        if self.pending.is_some() {
            return Err(io::Error::other(
                "cannot clone stdio send while an operation is pending",
            ));
        }
        let stdio = self.stdio.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "stdio send resource is closed")
        })?;
        match self
            .client
            .request(RequestKind::StdioSendClone {
                stdio: stdio.clone(),
            })
            .await
            .map_err(crate::Error::into_io_error)?
        {
            ResponseKind::StdioSendClone(result) => result
                .map(|stdio| Self {
                    client: self.client.clone(),
                    stdio: Some(stdio),
                    pending: None,
                })
                .map_err(wire_io),
            response => Err(unexpected(response)),
        }
    }
}

impl Drop for RemoteStdioRecv {
    fn drop(&mut self) {
        let Some(stdio) = self.stdio.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        runtime.spawn(best_effort_close_stdio_recv(client, stdio));
    }
}

impl RemoteStdioRecv {
    pub(crate) fn disarm_cleanup(&mut self) {
        self.stdio.take();
    }

    pub(crate) async fn try_clone(&self) -> io::Result<Self> {
        if self.pending.is_some() {
            return Err(io::Error::other(
                "cannot clone stdio receive while an operation is pending",
            ));
        }
        let stdio = self.stdio.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdio receive resource is closed",
            )
        })?;
        match self
            .client
            .request(RequestKind::StdioRecvClone {
                stdio: stdio.clone(),
            })
            .await
            .map_err(crate::Error::into_io_error)?
        {
            ResponseKind::StdioRecvClone(result) => result
                .map(|stdio| Self {
                    client: self.client.clone(),
                    stdio: Some(stdio),
                    pending: None,
                })
                .map_err(wire_io),
            response => Err(unexpected(response)),
        }
    }
}

enum ClientRecv {
    Null,
    Inherit,
    Native(DefaultHandle),
    Resource(StdioRecv),
}

enum ClientSend {
    Null,
    Inherit(HostOutput),
    Native(DefaultHandle),
    Resource(StdioSend),
}

impl<'a> CommandBuilder<'a> {
    fn new(client: &'a Client, program: Utf8TypedPath<'_>) -> Self {
        Self {
            client,
            program: program.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin: ClientRecv::Null,
            stdout: ClientSend::Null,
            stderr: ClientSend::Null,
        }
    }

    async fn prepare_recv(
        client: &Client,
        stdio: ClientRecv,
        relays: &mut PreparedRelays,
    ) -> crate::Result<(StdioRecvTarget, Option<StdioRecv>)> {
        match stdio {
            ClientRecv::Null => Ok((StdioRecvTarget::Null, None)),
            ClientRecv::Inherit => {
                let (send, recv) = client.pipe().await?;
                relays.stdin = Some(send);
                let StdioRecv::Remote(remote) = recv else {
                    return Err(io::Error::other(
                        "remote pipe unexpectedly returned a native receive endpoint",
                    )
                    .into());
                };
                Self::prepare_remote_recv(client, remote)
            }
            ClientRecv::Native(handle) => {
                if client.mode == SessionMode::Remote {
                    return client.unsupported("native process stdio");
                }
                Ok((StdioRecvTarget::Native(OsHandle::new(handle)), None))
            }
            ClientRecv::Resource(stdio) => match stdio {
                StdioRecv::Native(_) => {
                    if client.mode == SessionMode::Remote {
                        return client.unsupported("native process stdio");
                    }
                    let handle = stdio.into_blocking_handle().await?;
                    Ok((StdioRecvTarget::Native(OsHandle::new(handle)), None))
                }
                StdioRecv::Remote(remote) => Self::prepare_remote_recv(client, remote),
            },
        }
    }

    fn prepare_remote_recv(
        client: &Client,
        remote: RemoteStdioRecv,
    ) -> crate::Result<(StdioRecvTarget, Option<StdioRecv>)> {
        if !client.rpc.is_same_session(&remote.client.rpc) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdio receive belongs to a different VFS session",
            )
            .into());
        }
        let opaque = remote.stdio.as_ref().unwrap().clone();
        let stdio = StdioRecv::Remote(remote);
        Ok((StdioRecvTarget::Opaque(opaque), Some(stdio)))
    }

    async fn prepare_send(
        client: &Client,
        stdio: ClientSend,
        relays: &mut PreparedRelays,
    ) -> crate::Result<(StdioSendTarget, Option<StdioSend>)> {
        match stdio {
            ClientSend::Null => Ok((StdioSendTarget::Null, None)),
            ClientSend::Inherit(output) => {
                let (send, recv) = client.pipe().await?;
                relays.outputs.push((recv, output));
                let StdioSend::Remote(remote) = send else {
                    return Err(io::Error::other(
                        "remote pipe unexpectedly returned a native send endpoint",
                    )
                    .into());
                };
                Self::prepare_remote_send(client, remote)
            }
            ClientSend::Native(handle) => {
                if client.mode == SessionMode::Remote {
                    return client.unsupported("native process stdio");
                }
                Ok((StdioSendTarget::Native(OsHandle::new(handle)), None))
            }
            ClientSend::Resource(stdio) => match stdio {
                StdioSend::Native(_) => {
                    if client.mode == SessionMode::Remote {
                        return client.unsupported("native process stdio");
                    }
                    let handle = stdio.into_blocking_handle().await?;
                    Ok((StdioSendTarget::Native(OsHandle::new(handle)), None))
                }
                StdioSend::Remote(remote) => Self::prepare_remote_send(client, remote),
            },
        }
    }

    fn prepare_remote_send(
        client: &Client,
        remote: RemoteStdioSend,
    ) -> crate::Result<(StdioSendTarget, Option<StdioSend>)> {
        if !client.rpc.is_same_session(&remote.client.rpc) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdio send belongs to a different VFS session",
            )
            .into());
        }
        let opaque = remote.stdio.as_ref().unwrap().clone();
        let stdio = StdioSend::Remote(remote);
        Ok((StdioSendTarget::Opaque(opaque), Some(stdio)))
    }

    async fn prepare_outputs(
        client: &Client,
        stdout: ClientSend,
        stderr: ClientSend,
        relays: &mut PreparedRelays,
    ) -> crate::Result<(
        (StdioSendTarget, Option<StdioSend>),
        (StdioSendTarget, Option<StdioSend>),
    )> {
        if client.mode == SessionMode::Remote
            && matches!(stdout, ClientSend::Inherit(HostOutput::Stdout))
            && matches!(stderr, ClientSend::Inherit(HostOutput::Stdout))
        {
            let (send, recv) = client.pipe().await?;
            let stderr = send.try_clone().await?;
            relays.outputs.push((recv, HostOutput::Stdout));
            let stdout = Self::prepare_send(client, ClientSend::Resource(send), relays).await?;
            let stderr = Self::prepare_send(client, ClientSend::Resource(stderr), relays).await?;
            Ok((stdout, stderr))
        } else {
            let stdout = Self::prepare_send(client, stdout, relays).await?;
            let stderr = Self::prepare_send(client, stderr, relays).await?;
            Ok((stdout, stderr))
        }
    }
}

async fn relay_stdin(mut send: StdioSend) {
    let mut stdin = tokio::io::stdin();
    let _ = tokio::io::copy(&mut stdin, &mut send).await;
    let _ = send.shutdown().await;
}

async fn relay_output<W>(mut recv: StdioRecv, mut output: W)
where
    W: AsyncWrite + Unpin,
{
    let _ = tokio::io::copy(&mut recv, &mut output).await;
    let _ = output.flush().await;
}

impl PreparedRelays {
    fn start(self) -> ClientRelays {
        let stdin = self.stdin.map(|send| tokio::spawn(relay_stdin(send)));
        let outputs = self
            .outputs
            .into_iter()
            .map(|(recv, output)| match output {
                HostOutput::Stdout => tokio::spawn(relay_output(recv, tokio::io::stdout())),
                HostOutput::Stderr => tokio::spawn(relay_output(recv, tokio::io::stderr())),
            })
            .collect();
        ClientRelays { stdin, outputs }
    }
}

impl ClientRelays {
    fn abort_stdin(&mut self) {
        if let Some(stdin) = self.stdin.take() {
            stdin.abort();
        }
    }

    fn finish(&mut self) {
        self.abort_stdin();
        self.outputs.clear();
    }
}

impl ClientChild {
    fn result(&self) -> Option<crate::Result<ProcessStatus>> {
        match &self.state {
            ClientChildState::Live(_) => None,
            ClientChildState::Exited(status) => Some(Ok(*status)),
            ClientChildState::Lost(error) => Some(Err(error.clone().into())),
        }
    }

    fn store_result(
        &mut self,
        result: &std::result::Result<ProcessStatus, crate::protocol::WireError>,
    ) {
        self.state = match result {
            Ok(status) => ClientChildState::Exited(*status),
            Err(error) => ClientChildState::Lost(error.clone()),
        };
    }
}

impl Drop for ClientChild {
    fn drop(&mut self) {
        self.relays.finish();
        let ClientChildState::Live(child) = &self.state else {
            return;
        };
        let child = child.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        runtime.spawn(async move {
            let _ = client.request(RequestKind::ChildClose { child }).await;
        });
    }
}

impl Child for ClientChild {
    async fn wait(&mut self) -> crate::Result<ProcessStatus> {
        if let Some(result) = self.result() {
            return result;
        }
        let ClientChildState::Live(child) = &self.state else {
            unreachable!();
        };
        match self
            .client
            .request(RequestKind::ChildWait {
                child: child.clone(),
            })
            .await?
        {
            ResponseKind::ChildWait(result) => {
                self.relays.finish();
                self.store_result(&result);
                self.result().unwrap()
            }
            response => Err(unexpected(response).into()),
        }
    }

    async fn terminate(mut self) -> crate::Result<ProcessStatus> {
        self.relays.abort_stdin();
        if let Some(result) = self.result() {
            return result;
        }
        let ClientChildState::Live(child) = &self.state else {
            unreachable!();
        };
        match self
            .client
            .request(RequestKind::ChildTerminate {
                child: child.clone(),
            })
            .await?
        {
            ResponseKind::ChildTerminate(result) => {
                self.relays.finish();
                self.store_result(&result);
                self.result().unwrap()
            }
            response => Err(unexpected(response).into()),
        }
    }
}

impl<'a> Command for CommandBuilder<'a> {
    type Child = ClientChild;
    type StdioSend = StdioSend;
    type StdioRecv = StdioRecv;

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

    fn stdin(&mut self, stdio: StdioRecv) -> io::Result<&mut Self> {
        if let StdioRecv::Remote(remote) = &stdio
            && !self.client.rpc.is_same_session(&remote.client.rpc)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdio receive belongs to a different VFS session",
            ));
        }
        self.stdin = ClientRecv::Resource(stdio);
        Ok(self)
    }

    fn stdout(&mut self, stdio: StdioSend) -> io::Result<&mut Self> {
        if let StdioSend::Remote(remote) = &stdio
            && !self.client.rpc.is_same_session(&remote.client.rpc)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdio send belongs to a different VFS session",
            ));
        }
        self.stdout = ClientSend::Resource(stdio);
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdin = if self.client.mode == SessionMode::Remote {
            if std::io::stdin().is_terminal() {
                ClientRecv::Null
            } else {
                ClientRecv::Inherit
            }
        } else {
            ClientRecv::Native(clone_stdin_handle()?)
        };
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdout = if self.client.mode == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stdout)
        } else {
            ClientSend::Native(clone_stdout_handle()?)
        };
        Ok(self)
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.stdin = ClientRecv::Null;
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.stdout = ClientSend::Null;
        self
    }

    fn stderr(&mut self, stdio: StdioSend) -> io::Result<&mut Self> {
        if let StdioSend::Remote(remote) = &stdio
            && !self.client.rpc.is_same_session(&remote.client.rpc)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdio send belongs to a different VFS session",
            ));
        }
        self.stderr = ClientSend::Resource(stdio);
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.stderr = if self.client.mode == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stderr)
        } else {
            ClientSend::Native(clone_stderr_handle()?)
        };
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.stderr = if self.client.mode == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stdout)
        } else {
            ClientSend::Native(clone_stdout_handle()?)
        };
        Ok(self)
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.stderr = ClientSend::Null;
        self
    }

    async fn spawn(self) -> crate::Result<Self::Child> {
        let Self {
            client,
            program,
            args,
            env,
            cwd,
            stdin,
            stdout,
            stderr,
        } = self;
        let mut relays = PreparedRelays::default();
        let (stdin, mut stdin_resource) = Self::prepare_recv(client, stdin, &mut relays).await?;
        let ((stdout, mut stdout_resource), (stderr, mut stderr_resource)) =
            Self::prepare_outputs(client, stdout, stderr, &mut relays).await?;
        let req = SpawnRequest {
            program,
            args,
            env,
            cwd,
            stdin,
            stdout,
            stderr,
        };
        match client.request(RequestKind::Spawn(req)).await? {
            ResponseKind::Spawn(result) => {
                if let Some(stdio) = &mut stdin_resource {
                    stdio.disarm_remote_cleanup();
                }
                if let Some(stdio) = &mut stdout_resource {
                    stdio.disarm_remote_cleanup();
                }
                if let Some(stdio) = &mut stderr_resource {
                    stdio.disarm_remote_cleanup();
                }
                result
                    .map(|child| ClientChild {
                        client: client.clone(),
                        state: ClientChildState::Live(child),
                        relays: relays.start(),
                    })
                    .map_err(Into::into)
            }
            response => Err(unexpected(response).into()),
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
                OpenHandle::Native(handle) => Ok(ClientFile::from_std(
                    handle.into_inner().into(),
                    self.read,
                    self.write,
                    self.append,
                )),
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
    type StdioSend = StdioSend;
    type StdioRecv = StdioRecv;
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

    async fn pipe(&self) -> crate::Result<(StdioSend, StdioRecv)> {
        if self.mode == SessionMode::Native {
            return crate::Direct::default().pipe().await;
        }
        match self.request(RequestKind::Pipe).await? {
            ResponseKind::Pipe(result) => result
                .map(|pipe| {
                    (
                        StdioSend::Remote(RemoteStdioSend {
                            client: self.clone(),
                            stdio: Some(pipe.send),
                            pending: None,
                        }),
                        StdioRecv::Remote(RemoteStdioRecv {
                            client: self.clone(),
                            stdio: Some(pipe.recv),
                            pending: None,
                        }),
                    )
                })
                .map_err(Into::into),
            response => Err(unexpected(response).into()),
        }
    }

    async fn query(&self) -> crate::Result<Query> {
        Client::query(self).await
    }

    async fn read_dir(&self, path: Utf8TypedPath<'_>) -> crate::Result<ReadDir> {
        match self
            .request(RequestKind::ReadDir { path: path.into() })
            .await?
        {
            ResponseKind::ReadDir(result) => result
                .map(|ReadDirResponse { entries }| ReadDir::from_entries(entries))
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

#[cfg(test)]
mod tests {
    use std::io;

    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{Client, ClientChildState, ClientFileInner};
    use crate::{
        Child as _, Command as _, FileHandle as _, Server, Vfs as _, protocol::RequestKind,
    };

    #[cfg(unix)]
    fn successful_command(client: &Client) -> super::CommandBuilder<'_> {
        let mut command =
            client.command(crate::Utf8TypedPath::Unix(crate::Utf8UnixPath::new("sh")));
        command.arg("-c").arg("exit 0");
        command
    }

    #[cfg(windows)]
    fn successful_command(client: &Client) -> super::CommandBuilder<'_> {
        let mut command = client.command(crate::Utf8TypedPath::Windows(
            crate::Utf8WindowsPath::new("cmd"),
        ));
        command.arg("/C").arg("exit 0");
        command
    }

    async fn open_remote_file(
        client: &Client,
        path: crate::Utf8TypedPath<'_>,
    ) -> super::ClientFile {
        let mut options = client.open_options();
        options.read(true).write(true).create(true).truncate(true);
        crate::OpenOptions::open(&options, path).await.unwrap()
    }

    #[tokio::test]
    async fn dropping_remote_file_unregisters_opaque_identity() {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(Server::new(server_stream).serve());
        let client = Client::new(client_stream);
        let temp = tempdir().unwrap();
        let path = crate::typed_path(temp.path().join("file")).unwrap();
        let file = open_remote_file(&client, path.to_path()).await;
        let opaque = match &file.0 {
            ClientFileInner::Remote(file) => file.opaque(),
            ClientFileInner::Direct(_) => panic!("remote open returned a direct file"),
        };

        drop(file);

        for attempt in 0..100 {
            let response = client
                .request(RequestKind::FileMetadata {
                    file: opaque.clone(),
                })
                .await
                .unwrap();
            let crate::protocol::ResponseKind::FileMetadata(result) = response else {
                panic!("file metadata returned the wrong response");
            };
            match result {
                Err(error) => {
                    assert_eq!(
                        crate::Error::from(error).kind(),
                        io::ErrorKind::InvalidInput
                    );
                    break;
                }
                Ok(_) if attempt < 99 => tokio::task::yield_now().await,
                Ok(_) => panic!("dropped opaque file remained registered"),
            }
        }

        client.stop().await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn opaque_pipe_rejects_wrong_type_and_stale_identity() {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(Server::new(server_stream).serve());
        let client = Client::new(client_stream);
        let (mut send, mut recv) = client.pipe().await.unwrap();
        let send_opaque = match &send {
            crate::StdioSend::Remote(send) => send.stdio.as_ref().unwrap().clone(),
            crate::StdioSend::Native(_) => panic!("remote pipe returned a native send end"),
        };

        let encoded = postcard::to_allocvec(&send_opaque).unwrap();
        let wrong: dolang_rpc::Opaque<crate::StdioRecvMarker> =
            postcard::from_bytes(&encoded).unwrap();
        let response = client
            .request(RequestKind::StdioRecvClose { stdio: wrong })
            .await
            .unwrap();
        let crate::protocol::ResponseKind::StdioRecvClose(result) = response else {
            panic!("stdio receive close returned the wrong response");
        };
        assert_eq!(
            crate::Error::from(result.unwrap_err()).kind(),
            io::ErrorKind::InvalidInput
        );

        send.write_all(b"still live").await.unwrap();
        let mut data = [0; 10];
        recv.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"still live");
        send.shutdown().await.unwrap();

        let response = client
            .request(RequestKind::StdioSendClose { stdio: send_opaque })
            .await
            .unwrap();
        let crate::protocol::ResponseKind::StdioSendClose(result) = response else {
            panic!("stdio send close returned the wrong response");
        };
        assert_eq!(
            crate::Error::from(result.unwrap_err()).kind(),
            io::ErrorKind::InvalidInput
        );

        client.stop().await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[test]
    fn dropping_remote_file_without_runtime_does_not_panic() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (file, client, server, temp) = runtime.block_on(async {
            let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
            let server = tokio::spawn(Server::new(server_stream).serve());
            let client = Client::new(client_stream);
            let temp = tempdir().unwrap();
            let path = crate::typed_path(temp.path().join("file")).unwrap();
            let file = open_remote_file(&client, path.to_path()).await;
            (file, client, server, temp)
        });

        drop(runtime);
        drop(file);
        drop(client);
        drop(server);
        drop(temp);
    }

    #[tokio::test]
    async fn explicit_close_consumes_remote_cleanup_identity() {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(Server::new(server_stream).serve());
        let client = Client::new(client_stream);
        let temp = tempdir().unwrap();
        let path = crate::typed_path(temp.path().join("file")).unwrap();
        let file = open_remote_file(&client, path.to_path()).await;

        file.close().await.unwrap();

        client.stop().await.unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn child_wait_caches_wire_error() {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(Server::new(server_stream).serve());
        let client = Client::new(client_stream);
        let mut child = successful_command(&client).spawn().await.unwrap();
        let ClientChildState::Live(opaque) = &child.state else {
            panic!("new child is not live");
        };
        let response = client
            .request(RequestKind::ChildClose {
                child: opaque.clone(),
            })
            .await
            .unwrap();
        let crate::protocol::ResponseKind::ChildClose(Ok(())) = response else {
            panic!("child close returned the wrong response");
        };

        let first = child.wait().await.unwrap_err();
        let second = child.wait().await.unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(second.kind(), first.kind());
        assert_eq!(second.to_string(), first.to_string());

        client.stop().await.unwrap();
        server.await.unwrap().unwrap();
    }
}
