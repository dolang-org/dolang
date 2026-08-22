use std::{
    collections::{HashMap, VecDeque},
    fmt,
    future::{Future, poll_fn},
    io::{self, IsTerminal},
    mem,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

#[cfg(unix)]
use std::os::unix::{
    io::{AsFd, OwnedFd},
    net::UnixStream as StdUnixStream,
};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, OwnedHandle};
#[cfg(all(docsrs, not(windows)))]
struct OwnedHandle;

use bytes::{Bytes, BytesMut};
#[cfg(unix)]
use dolang_rpc::auth::AuthKey;
use dolang_rpc::{
    client::Call,
    handle::{DefaultHandle, OsHandle},
    session::{Cite, Gift},
    trailer::{TrailerRecv, TrailerSend},
};
use dolang_winterop::security::{SecDesc, Sid};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(all(docsrs, not(windows)))]
struct NamedPipeServer;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    task::JoinHandle,
};
use typed_path::{Utf8TypedPath, Utf8TypedPathBuf};

use crate::{
    MAX_FILE_READ, STREAM_CHUNK_SIZE, SessionMode, Vfs, client, direct,
    directory::{self, DirEntry},
    error::{Error, ErrorKind, HandoffError, Result},
    extension::ExtensionSet,
    extension::VfsExtension,
    file::{self, AccessFlags, File as VfsFile, FileLockRequest, StreamEntry},
    file::{XattrEntry, XattrNamespace},
    metadata::{FsMetadata, Metadata, MetadataPatch},
    path::WellKnownPath,
    process::{
        self, ProcessControl, ProcessStatus, StdioRecv, StdioRecvInner, StdioSend, StdioSendInner,
        TerminationPolicy,
    },
    protocol::{
        AccessRequest, AclRequest, CanonicalizeRequest, CopyRequest, CreateDirRequest,
        ExtensionRequest, ExtensionResponse, FsMetadataRequest, GlobRequest, HardLinkRequest,
        MetadataRequest, MoveRequest, OpenFlags, OpenHandle, OpenRequest, OpenVfsHandle,
        QueryResponse, ReadLinkRequest, RemoveDirRequest, RemoveRequest, RenameRequest, Request,
        RequestKind, ResponseKind, SecDescRequest, SetAclRequest, SetMetadataRequest,
        SetSecDescRequest, SetXattrRequest, SpawnRequest, StdioRecvTarget, StdioSendTarget,
        StreamsRequest, SymlinkKind, SymlinkRequest, UnixVfsRequest, VfsProtocol,
        WellKnownPathRequest, WindowsAdminRequest, WirePath, XattrNamespaceRequest, XattrRequest,
        XattrsRequest, rpc_builder,
    },
    security::{Acl, AclKind, PrincipalId, PrincipalIdKind, SecurityInfo, SidName},
    session::{
        ChildMarker, FileLockMarker, FileMarker, Query, ReadDirMarker, StdioRecvMarker,
        StdioSendMarker, VfsMarker,
    },
    target::TargetInfo,
};

/// Client for a VFS agent session.
///
/// Clones share one RPC connection. Generic-stream constructors create an
/// opaque-only session; Unix-socket and Windows named-pipe constructors can
/// use native handles when the peer supports them. Prefer the [`Vfs`]
/// trait when code should work with local and remote backends alike.
#[derive(Clone)]
pub struct Client {
    shared: Arc<ClientShared>,
    vfs: Option<Gift<VfsMarker>>,
    #[cfg(windows)]
    pub(crate) process: Option<Arc<crate::windows::OwnedProcess>>,
}

struct ClientShared {
    rpc: dolang_rpc::client::Client<VfsProtocol>,
    mode: SessionMode,
    query: Query,
}

/// A file handle returned by a [`Client`] operation.
///
/// Holds an opaque remote file reference.
pub struct File {
    client: Client,
    file: Gift<FileMarker>,
    append: bool,
    cursor: OnceLock<Box<ClientCursor>>,
}

pub(crate) struct ReadDir {
    client: Client,
    handle: Option<Gift<ReadDirMarker>>,
    entries: VecDeque<DirEntry>,
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir")
            .field("handle", &self.handle)
            .field("buffered", &self.entries.len())
            .finish()
    }
}

impl ReadDir {
    fn new(client: Client, handle: Gift<ReadDirMarker>) -> Self {
        Self {
            client,
            handle: Some(handle),
            entries: VecDeque::new(),
        }
    }

    pub(crate) async fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        if let Some(entry) = self.entries.pop_front() {
            return Ok(Some(entry));
        }
        let Some(handle) = self.handle.as_ref().map(Gift::cite) else {
            return Ok(None);
        };
        match self
            .client
            .request(RequestKind::ReadDirNext { read_dir: handle })
            .await?
        {
            ResponseKind::ReadDirNext(page) => {
                if page.done {
                    self.handle = None;
                }
                self.entries = page.entries.into();
                Ok(self.entries.pop_front())
            }
            _ => Err(Error::new(
                ErrorKind::Other,
                "unexpected response for ReadDirNext",
            )),
        }
    }
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        let Some(read_dir) = self.handle.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        runtime.spawn(async move {
            let _ = client
                .request(RequestKind::ReadDirClose {
                    read_dir: read_dir.cite(),
                })
                .await;
        });
    }
}

struct ClientCursor {
    /// The seek position, maintained entirely on this side: the VFS protocol
    /// carries an explicit offset on every operation and the peer keeps no
    /// position of its own. Advanced by bytes actually transferred, so a short
    /// read at EOF leaves it where the data stopped, exactly as the kernel
    /// would.
    cursor: u64,
    /// Whether the peer opened this file for append. Append writes have to use
    /// [`RequestKind::FileAppend`], since the offset on a positional write is
    /// ignored on an append-mode description and the resulting position can
    /// only be learned from the peer.
    /// Distance from the end requested by a pending `SeekFrom::End`, applied
    /// once the [`RequestKind::FileSize`] reply lands.
    seek_delta: i64,
    pending: Option<PendingFileOperation>,
    read_body: Option<PendingTrailerRead>,
    write_body: Option<PendingTrailerWrite>,
}

pub(crate) struct FileLock {
    client: Client,
    /// The peer's handle for the held lock, taken once it is released.
    ///
    /// It names the lock directly, so releasing needs nothing from the file the
    /// lock was taken on — including the file still being open.
    lock: Option<Gift<FileLockMarker>>,
}

impl FileLock {
    pub(crate) async fn release(&mut self) -> Result<()> {
        let Some(lock) = self.lock.as_ref() else {
            return Ok(());
        };
        match self
            .client
            .request(RequestKind::FileUnlock { lock: lock.cite() })
            .await?
        {
            ResponseKind::FileUnlock => {
                self.lock = None;
                Ok(())
            }
            response => Err(unexpected(response).into()),
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let Some(lock) = self.lock.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        runtime.spawn(async move {
            let _ = client
                .request(RequestKind::FileUnlock { lock: lock.cite() })
                .await;
        });
    }
}

struct PendingFileOperation {
    kind: FileOperationKind,
    call: Call<VfsProtocol>,
}

struct PendingTrailerWrite {
    send: Option<TrailerSend<Call<VfsProtocol>>>,
    call: Option<Call<VfsProtocol>>,
    target: usize,
    sent: usize,
    unreported: usize,
}

struct PendingTrailerRead {
    recv: TrailerRecv,
    remaining: usize,
    read: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileOperationKind {
    Read,
    Size,
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientFile")
            .field("file", &self.file)
            .field(
                "pending",
                &self
                    .cursor
                    .get()
                    .and_then(|cursor| cursor.pending.as_ref())
                    .map(|pending| pending.kind),
            )
            .finish_non_exhaustive()
    }
}

impl File {
    fn new(client: Client, file: Gift<FileMarker>, append: bool) -> Self {
        Self {
            client,
            file,
            append,
            cursor: OnceLock::new(),
        }
    }

    fn cursor(&mut self) -> &mut ClientCursor {
        if self.cursor.get().is_none() {
            assert!(
                self.cursor
                    .set(Box::new(ClientCursor {
                        cursor: 0,
                        seek_delta: 0,
                        pending: None,
                        read_body: None,
                        write_body: None,
                    }))
                    .is_ok()
            );
        }
        self.cursor.get_mut().unwrap()
    }

    fn cite(&self) -> Cite<FileMarker> {
        self.file.cite()
    }

    /// Asks the peer to convert this file into a standard-output endpoint at
    /// `offset`, consuming it on that side.
    ///
    /// Takes `&self` rather than consuming, so that the caller keeps the handle
    /// to hand back when this fails. The peer only retires its registration on
    /// success; a failure — a busy file above all — leaves it registered and
    /// this handle usable.
    async fn stdio_send(&self, offset: u64) -> Result<StdioSend> {
        self.idle()?;
        match self
            .client
            .request(RequestKind::FileToStdioSend {
                file: self.cite(),
                offset,
            })
            .await?
        {
            ResponseKind::FileToStdioSend(stdio) => Ok({
                StdioSend::remote(RemoteStdioSend {
                    client: self.client.clone(),
                    stdio: Some(stdio),
                    pending: None,
                    write_body: None,
                })
            }),
            response => Err(unexpected(response).into()),
        }
    }

    /// Asks the peer to convert this file into a standard-input endpoint at
    /// `offset`. See [`stdio_send`](Self::stdio_send).
    async fn stdio_recv(&self, offset: u64) -> Result<StdioRecv> {
        self.idle()?;
        match self
            .client
            .request(RequestKind::FileToStdioRecv {
                file: self.cite(),
                offset,
            })
            .await?
        {
            ResponseKind::FileToStdioRecv(stdio) => Ok({
                StdioRecv::remote(RemoteStdioRecv {
                    client: self.client.clone(),
                    stdio: Some(stdio),
                    pending: None,
                    read_body: None,
                })
            }),
            response => Err(unexpected(response).into()),
        }
    }

    /// Reads at `offset` over the wire, appending into `buf`'s spare capacity.
    ///
    /// Detached from the handle: it takes a citation and its own clone of the
    /// client, so several may be outstanding at once. It deliberately touches
    /// none of `cursor`, `pending`, `read_body`, or `write_body` — positional
    /// operations and the cursor-based poll path share nothing but the file.
    pub(crate) fn read_at<'b>(
        &self,
        buf: &'b mut BytesMut,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        let client = self.client.clone();
        let file = self.cite();
        async move {
            // Taken for the duration and put back below, matching the direct
            // backend's contract rather than having two: a cancelled read
            // leaves the buffer empty on either route.
            let mut taken = mem::take(buf);
            let before = taken.len();
            // The transfer runs against the taken buffer; the buffer goes back
            // to the caller afterwards whether or not it succeeded, since a
            // failed read spoils nothing about the allocation.
            let dst = &mut taken;
            let result = async move {
                // Clamped, not looped: this is the low-level positional read,
                // and a short return is part of its contract. Looping belongs
                // to the callers that need every byte.
                let len = dst.spare_capacity_mut().len().min(MAX_FILE_READ);
                if len == 0 {
                    return Ok(());
                }
                let mut trailer = Self::begin_read(client, file, offset, len).await?;
                let mut remaining = len;
                while remaining > 0 {
                    // `read_buf` writes into the spare capacity and only grows
                    // the buffer once that is exhausted, which the loop
                    // condition prevents, so the transfer stays inside what was
                    // asked for.
                    let read = trailer.read_buf(dst).await?;
                    if read == 0 {
                        return Ok(());
                    }
                    remaining -= read;
                }
                Self::finish_read(&mut trailer).await
            }
            .await;
            let read = taken.len() - before;
            *buf = taken;
            Ok(result.map(|()| read)?)
        }
    }

    /// Writes `data` at `offset` over the wire, returning the byte count.
    pub(crate) fn write_at(
        &self,
        data: Bytes,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<> {
        let client = self.client.clone();
        let file = self.cite();
        let append = self.append;
        async move {
            if append {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "cannot write at an offset on a file opened for append",
                ));
            }
            match Self::send_write(client, RequestKind::FileWrite { file, offset }, &data).await? {
                ResponseKind::FileWrite(result) => Ok(result),
                response => Err(unexpected(response).into()),
            }
        }
    }

    /// Reads into the uninitialized `buf` starting at `offset`, returning how
    /// many bytes at its front were filled.
    ///
    /// The reply trailer lands directly in `buf`, which is the point: this
    /// route exists so a caller whose destination is not a [`BytesMut`] does
    /// not pay a copy out of one. There is no intermediate storage, so nothing
    /// is lost by cancelling and nothing has to be handed back.
    pub(crate) fn read_at_into<'b>(
        &self,
        buf: &'b mut [mem::MaybeUninit<u8>],
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        let client = self.client.clone();
        let file = self.cite();
        async move {
            let len = buf.len().min(MAX_FILE_READ);
            if len == 0 {
                return Ok(0);
            }
            let mut trailer = Self::begin_read(client, file, offset, len).await?;
            let mut dst = ReadBuf::uninit(&mut buf[..len]);
            while dst.remaining() > 0 {
                let before = dst.filled().len();
                poll_fn(|cx| Pin::new(&mut trailer).poll_read(cx, &mut dst)).await?;
                if dst.filled().len() == before {
                    // End of the reply, short of what was asked for.
                    return Ok(before);
                }
            }
            Self::finish_read(&mut trailer).await?;
            Ok(dst.filled().len())
        }
    }

    /// Writes `data` at `offset` over the wire from borrowed storage.
    pub(crate) fn write_at_from<'b>(
        &self,
        data: &'b [u8],
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        let client = self.client.clone();
        let file = self.cite();
        let append = self.append;
        async move {
            if append {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "cannot write at an offset on a file opened for append",
                ));
            }
            // Into the trailer straight from the caller's storage: `send_write`
            // only ever wanted a slice, so borrowing costs this path nothing
            // that owning it gained.
            match Self::send_write(client, RequestKind::FileWrite { file, offset }, data).await? {
                ResponseKind::FileWrite(result) => Ok(result),
                response => Err(unexpected(response).into()),
            }
        }
    }

    /// Appends `data` over the wire, returning the byte count and the position
    /// just past what was written.
    pub(crate) fn append(
        &self,
        data: Bytes,
    ) -> impl Future<Output = Result<(usize, u64)>> + Send + use<> {
        let client = self.client.clone();
        let file = self.cite();
        async move {
            match Self::send_write(client, RequestKind::FileAppend { file }, &data).await? {
                ResponseKind::FileAppend(result) => Ok(result),
                response => Err(unexpected(response).into()),
            }
        }
    }

    /// Issues the read and hands back the trailer its bytes arrive on.
    async fn begin_read(
        client: Client,
        file: Cite<FileMarker>,
        offset: u64,
        len: usize,
    ) -> Result<TrailerRecv> {
        let (response, trailer) = client
            .call(RequestKind::FileRead { file, offset, len })
            .await?
            .into_response_trailer();
        match response? {
            ResponseKind::FileRead => (),
            response => return Err(unexpected(response).into()),
        }
        trailer.ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "file read response is missing its data trailer",
            )
        })
    }

    /// Observes the end of a fully-consumed read trailer.
    ///
    /// The peer holds the file until the trailer's terminal fragment commits,
    /// so having taken every byte is not the end of the read. Observing the end
    /// here rather than dropping mid-stream also catches a byte past the
    /// requested length as the protocol violation it is.
    async fn finish_read(trailer: &mut TrailerRecv) -> io::Result<()> {
        let mut past_the_end = [0u8; 1];
        if trailer.read(&mut past_the_end).await? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file read response exceeds requested length",
            ));
        }
        Ok(())
    }

    async fn send_write(client: Client, request: RequestKind, data: &[u8]) -> Result<ResponseKind> {
        let mut send = client.call_with_trailer(request);
        send.write_all(data).await?;
        send.finish().await?.into_response()
    }

    fn poll_request(
        &mut self,
        cx: &mut Context<'_>,
        kind: FileOperationKind,
        request: impl FnOnce(Cite<FileMarker>) -> (RequestKind, Option<Vec<u8>>),
    ) -> Poll<io::Result<(ResponseKind, Option<TrailerRecv>)>> {
        if self.cursor().pending.is_none() {
            let (request, trailer) = request(self.cite());
            let call = {
                assert!(trailer.is_none());
                self.client.call(request)
            };
            self.cursor().pending = Some(PendingFileOperation { kind, call });
        }
        let pending = self.cursor().pending.as_mut().unwrap();
        if pending.kind != kind {
            return Poll::Ready(Err(io::Error::other(format!(
                "file operation {:?} polled while {:?} is pending",
                kind, pending.kind
            ))));
        }
        match Pin::new(&mut pending.call).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.cursor().pending = None;
                let (response, trailer) = result.map_err(Error::from)?.into_response_trailer();
                Poll::Ready(
                    response
                        .map(|response| (response, trailer))
                        .map_err(Into::into),
                )
            }
        }
    }

    fn idle(&self) -> Result<()> {
        let Some(cursor) = self.cursor.get() else {
            return Ok(());
        };
        if cursor
            .read_body
            .as_ref()
            .is_some_and(|body| body.remaining != 0)
            || cursor.write_body.is_some()
        {
            Err(io::Error::other("file trailer operation is still pending").into())
        } else if let Some(pending) = &cursor.pending {
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
        // A read trailer can simply be dropped. The peer releases the file
        // before it responds at all now that it reads the range up front, so
        // nothing it still counts as in flight depends on us draining this;
        // `TrailerRecv::drop` notifies the peer and refunds the pool on its own.
        let Some(cursor) = self.cursor.get_mut() else {
            return;
        };
        cursor.read_body = None;
        if let Some(mut pending) = cursor.write_body.take()
            && let Some(mut call) = pending.call.take()
        {
            call.cancel();
            let _ = call.await;
        }
        if let Some(mut pending) = cursor.pending.take() {
            pending.call.cancel();
            let _ = pending.call.await;
        }
    }
}

impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let file = &mut *self;
        loop {
            // This reads the trailer straight into the caller's `ReadBuf`,
            // which for the Do-side `fs.File` is arena memory the collector
            // adopts without a copy. Do not "simplify" this by delegating to
            // a positional `read_at`: that would land the data in an owned
            // `BytesMut` first and cost a chunk-sized copy per read, with
            // nothing failing to say so.
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            let cursor = file.cursor();
            if let Some(body) = cursor.read_body.as_mut() {
                let before = buf.filled().len();
                match Pin::new(&mut body.recv).poll_read(cx, buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        cursor.read_body = None;
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(())) => {
                        let read = buf.filled().len() - before;
                        if read > body.remaining {
                            cursor.read_body = None;
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "file read response exceeds requested length",
                            )));
                        }
                        body.remaining -= read;
                        body.read += read;
                        // Advance only by what the caller has actually
                        // seen. Bytes still buffered in the trailer are
                        // discarded by `start_seek`, so counting them here
                        // would push the cursor past the last byte anyone
                        // received.
                        cursor.cursor += read as u64;
                        if read > 0 {
                            return Poll::Ready(Ok(()));
                        }
                        let empty = body.read == 0;
                        cursor.read_body = None;
                        if empty {
                            return Poll::Ready(Ok(()));
                        }
                        continue;
                    }
                }
            }
            // A short read is always legal for `AsyncRead`, so a request
            // for more than one chunk simply comes back in several; the
            // peer has to have the whole reply in hand before it can answer
            // at all, which is what bounds this.
            let requested = buf.remaining().min(MAX_FILE_READ);
            let offset = file.cursor().cursor;
            match file.poll_request(cx, FileOperationKind::Read, |file| {
                (
                    RequestKind::FileRead {
                        file,
                        offset,
                        len: requested,
                    },
                    None,
                )
            }) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok((ResponseKind::FileRead, trailer))) => {
                    let Some(trailer) = trailer else {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "file read response is missing its data trailer",
                        )));
                    };
                    file.cursor().read_body = Some(PendingTrailerRead {
                        recv: trailer,
                        remaining: requested,
                        read: 0,
                    });
                }
                Poll::Ready(Ok((response, _))) => {
                    return Poll::Ready(Err(unexpected(response)));
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            }
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let file = &mut *self;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if file.cursor().write_body.is_none() {
            let request = if file.append {
                RequestKind::FileAppend { file: file.cite() }
            } else {
                RequestKind::FileWrite {
                    file: file.cite(),
                    offset: file.cursor().cursor,
                }
            };
            file.cursor().write_body = Some(PendingTrailerWrite {
                send: Some(file.client.call_with_trailer(request)),
                call: None,
                target: buf.len(),
                sent: 0,
                unreported: 0,
            });
        }
        let pending = file.cursor().write_body.as_mut().unwrap();
        if let Some(call) = pending.call.as_mut() {
            match Pin::new(call).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    let target = pending.target;
                    let unreported = pending.unreported;
                    file.cursor().write_body = None;
                    let response = result
                        .map_err(Error::from)?
                        .into_response()
                        .map_err(io::Error::from)?;
                    let written = ack_write(&mut file.cursor().cursor, response)?;
                    if written != target {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "file write response does not acknowledge the submitted trailer",
                        )));
                    }
                    return Poll::Ready(Ok(unreported));
                }
            }
        }
        let remaining = pending.target - pending.sent;
        let send = pending.send.as_mut().unwrap();
        match Pin::new(send).poll_write(cx, &buf[..buf.len().min(remaining)]) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(n)) => {
                pending.sent += n;
                if pending.sent == pending.target {
                    let send = pending.send.take().unwrap();
                    pending.call = Some(send.finish());
                    pending.unreported = n;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(Ok(n))
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let file = &mut *self;
        if let Some(pending) = file.cursor().write_body.as_mut() {
            if pending.call.is_none() {
                let send = pending.send.take().unwrap();
                pending.call = Some(send.finish());
            }
            let call = pending.call.as_mut().unwrap();
            return match Pin::new(call).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    file.cursor().write_body = None;
                    Poll::Ready(
                        result
                            .map_err(Error::from)
                            .map_err(io::Error::from)
                            .and_then(|result| {
                                ack_write(
                                    &mut file.cursor().cursor,
                                    result.into_response().map_err(io::Error::from)?,
                                )
                                .map(|_| ())
                            }),
                    )
                }
            };
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().poll_flush(cx)
    }
}

impl AsyncSeek for File {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        let file = &mut *self;
        if file
            .cursor()
            .read_body
            .as_ref()
            .is_some_and(|body| body.remaining != 0)
        {
            file.cursor().read_body.take();
        }
        file.idle().map_err(Error::into_io_error)?;
        file.cursor().seek_delta = 0;
        // `Start` and `Current` are answerable here: the cursor is
        // ours. Only `End` has to ask the peer how long the file is.
        let absolute = match position {
            io::SeekFrom::Start(offset) => Some(offset),
            io::SeekFrom::Current(delta) => Some(
                file.cursor()
                    .cursor
                    .checked_add_signed(delta)
                    .ok_or_else(negative_seek)?,
            ),
            io::SeekFrom::End(delta) => {
                file.cursor().seek_delta = delta;
                None
            }
        };
        match absolute {
            Some(offset) => file.cursor().cursor = offset,
            None => {
                file.cursor().pending = Some(PendingFileOperation {
                    kind: FileOperationKind::Size,
                    call: file
                        .client
                        .call(RequestKind::FileSize { file: file.cite() }),
                });
            }
        }
        Ok(())
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let file = &mut *self;
        // Nothing outstanding means the seek already landed in the
        // cursor, or there was no seek at all and the caller just wants
        // the position.
        if file.cursor().pending.is_none() {
            return Poll::Ready(Ok(file.cursor().cursor));
        }
        match file.poll_request(cx, FileOperationKind::Size, |file| {
            (RequestKind::FileSize { file }, None)
        }) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok((ResponseKind::FileSize(len), _))) => {
                let Some(offset) = len.checked_add_signed(file.cursor().seek_delta) else {
                    return Poll::Ready(Err(negative_seek()));
                };
                file.cursor().cursor = offset;
                Poll::Ready(Ok(offset))
            }
            Poll::Ready(Ok((response, _))) => Poll::Ready(Err(unexpected(response))),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }
}

impl File {
    pub(crate) async fn into_stdio_send(
        self,
        offset: u64,
    ) -> std::result::Result<StdioSend, HandoffError<Self>> {
        let file = self;
        match file.stdio_send(offset).await {
            // The peer consumed the file, so this handle simply goes away:
            // dropping its reference names an id the peer has already
            // retired, which it ignores.
            Ok(stdio) => Ok(stdio),
            Err(error) => Err(HandoffError::new(file, error)),
        }
    }

    pub(crate) async fn into_stdio_recv(
        self,
        offset: u64,
    ) -> std::result::Result<StdioRecv, HandoffError<Self>> {
        let file = self;
        match file.stdio_recv(offset).await {
            Ok(stdio) => Ok(stdio),
            Err(error) => Err(HandoffError::new(file, error)),
        }
    }

    pub(crate) async fn close(self) -> Result<()> {
        let mut file = self;
        file.cancel_pending().await;
        match file
            .client
            .request(RequestKind::FileClose {
                file: file.file.cite(),
            })
            .await?
        {
            ResponseKind::FileClose => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn set_size(&self, size: u64) -> Result<()> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileSetSize {
                file: file.cite(),
                size,
            })
            .await?
        {
            ResponseKind::FileSetSize => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn metadata(&self) -> Result<Metadata> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileMetadata { file: file.cite() })
            .await?
        {
            ResponseKind::FileMetadata(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn fs_metadata(&self) -> Result<FsMetadata> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileFsMetadata { file: file.cite() })
            .await?
        {
            ResponseKind::FileFsMetadata(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn acl(&self, kind: AclKind, default: bool) -> Result<Option<Acl>> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileAcl {
                file: file.cite(),
                kind,
                default,
            })
            .await?
        {
            ResponseKind::FileAcl(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn set_acl(
        &self,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
    ) -> Result<()> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileSetAcl {
                file: file.cite(),
                kind,
                acl: acl.cloned(),
                default,
            })
            .await?
        {
            ResponseKind::FileSetAcl => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn sec_desc(
        &self,
        mask: dolang_winterop::security::SecInfo,
    ) -> Result<SecDesc> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileSecDesc {
                file: file.cite(),
                mask,
            })
            .await?
        {
            ResponseKind::FileSecDesc(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn set_sec_desc(&self, sec_desc: &SecDesc) -> Result<()> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileSetSecDesc {
                file: file.cite(),
                sec_desc: sec_desc.clone(),
            })
            .await?
        {
            ResponseKind::FileSetSecDesc => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn xattrs(&self, namespace: XattrNamespace<'_>) -> Result<Vec<XattrEntry>> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileXattrs {
                file: file.cite(),
                namespace: XattrNamespaceRequest::from(namespace),
            })
            .await?
        {
            ResponseKind::FileXattrs(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn xattr(&self, name: &str, namespace: Option<&str>) -> Result<Vec<u8>> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileXattr {
                file: file.cite(),
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
            })
            .await?
        {
            ResponseKind::FileXattr(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn streams(&self) -> Result<Vec<StreamEntry>> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileStreams { file: file.cite() })
            .await?
        {
            ResponseKind::FileStreams(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn set_xattr(
        &self,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<()> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileSetXattr {
                file: file.cite(),
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
                value: value.to_vec(),
            })
            .await?
        {
            ResponseKind::FileSetXattr => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn remove_xattr(&self, name: &str, namespace: Option<&str>) -> Result<()> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileRemoveXattr {
                file: file.cite(),
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
            })
            .await?
        {
            ResponseKind::FileRemoveXattr => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn lock(&self, request: FileLockRequest) -> Result<Option<file::FileLock>> {
        let file = self;
        file.idle()?;
        match file
            .client
            .request(RequestKind::FileLock {
                file: file.cite(),
                request,
            })
            .await?
        {
            ResponseKind::FileLock(lock) => Ok({
                lock.map(|lock| {
                    file::FileLock::remote(FileLock {
                        client: file.client.clone(),
                        lock: Some(lock),
                    })
                })
            }),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn try_into_std(self) -> std::result::Result<std::fs::File, Self> {
        Err(self)
    }
}

fn negative_seek() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid seek to a negative or overflowing position",
    )
}

/// Applies a write acknowledgement to the cursor and reports the byte count.
///
/// A positional write lands exactly where it was aimed, so the cursor simply
/// advances by what the peer accepted. An append write does not: the offset it
/// used is the peer's business, so the reply carries the resulting position and
/// the cursor takes it verbatim.
fn ack_write(cursor: &mut u64, response: ResponseKind) -> io::Result<usize> {
    match response {
        ResponseKind::FileWrite(written) => {
            *cursor += written as u64;
            Ok(written)
        }
        ResponseKind::FileAppend((written, end)) => {
            *cursor = end;
            Ok(written)
        }
        response => Err(unexpected(response)),
    }
}

fn query_from_wire(response: QueryResponse) -> Query {
    let QueryResponse {
        env,
        cwd,
        current_exe,
        target,
        security,
        extensions,
    } = response;
    Query {
        env,
        cwd: cwd.into(),
        current_exe: current_exe.into(),
        target,
        security,
        extensions,
    }
}

impl Client {
    async fn initialize(
        rpc: dolang_rpc::client::Client<VfsProtocol>,
        mode: SessionMode,
        vfs: Option<Gift<VfsMarker>>,
    ) -> Result<Self> {
        let response = rpc
            .call(Request {
                vfs: vfs.as_ref().map(Gift::cite),
                kind: RequestKind::Query,
            })
            .await?
            .into_response()?;
        let ResponseKind::Query(result) = response else {
            return Err(unexpected(response).into());
        };
        let query = query_from_wire(result);
        Ok(Self {
            shared: Arc::new(ClientShared { rpc, mode, query }),
            vfs,
            #[cfg(windows)]
            process: None,
        })
    }

    pub(crate) fn is_same_vfs(&self, other: &Self) -> bool {
        self.shared.rpc.is_same_session(&other.shared.rpc) && self.vfs == other.vfs
    }

    pub(crate) fn mode(&self) -> SessionMode {
        self.shared.mode
    }

    /// Starts an opaque-only VFS client on a bidirectional byte stream.
    ///
    /// This transport cannot transfer native handles, so files, subprocesses,
    /// and stdio endpoints are represented by remote references and relays.
    pub(crate) async fn new<T>(stream: T) -> Result<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let rpc = rpc_builder(None).client(stream).await?.bind();
        Self::initialize(rpc, SessionMode::Remote, None).await
    }

    /// Starts an opaque-only VFS client on separate reader and writer streams.
    ///
    /// This has the same opaque-only behavior as [`new`](Self::new).
    pub(crate) async fn new_split<R, W>(reader: R, writer: W) -> Result<Self>
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let rpc = rpc_builder(None).client_split(reader, writer).await?.bind();
        Self::initialize(rpc, SessionMode::Remote, None).await
    }

    /// Closes this client's RPC session and releases its transport handles.
    ///
    /// Closing any clone closes the shared session, so remaining clones can no
    /// longer issue requests.
    pub async fn close(self) {
        self.shared.rpc.clone().close().await;
    }

    /// Connects to an agent daemon at a Unix-domain socket path.
    ///
    /// This transport supports native file-descriptor transfer.
    #[cfg(unix)]
    pub(crate) async fn connect(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::connect_with_key(path, None).await
    }

    /// Connects to an agent daemon at a Unix-domain socket path, proving
    /// knowledge of a pre-shared key.
    ///
    /// A socket that must be world-connectable cannot identify its peer from
    /// credentials alone, so `key` is what distinguishes the intended agent
    /// from anything else listening at that path — and, in the other
    /// direction, this client from anything else that reached the socket
    /// first. Both ends must agree: a key here requires a keyed agent, and an
    /// agent expecting one refuses an unkeyed client. See [`dolang_rpc::auth`].
    #[cfg(unix)]
    pub(crate) async fn connect_with_key(
        path: impl AsRef<std::path::Path>,
        key: Option<AuthKey>,
    ) -> Result<Self> {
        Self::from_std_stream(UnixStream::connect(path).await?.into_std()?, key).await
    }

    /// Connects using an existing Unix-domain stream.
    ///
    /// This transport supports native file-descriptor transfer.
    #[cfg(unix)]
    pub(crate) async fn from_stream(stream: UnixStream) -> Result<Self> {
        Self::from_std_stream(stream.into_std()?, None).await
    }

    #[cfg(unix)]
    async fn from_std_stream(stream: StdUnixStream, key: Option<AuthKey>) -> Result<Self> {
        let rpc = rpc_builder(key).client_unix(stream).await?.bind();
        Self::initialize(rpc, SessionMode::Native, None).await
    }

    /// Starts a VFS client on an already-connected Unix-domain socket file
    /// descriptor.
    ///
    /// This transport supports native file-descriptor transfer.
    #[cfg(unix)]
    pub(crate) async fn from_owned_fd(value: OwnedFd) -> Result<Self> {
        Self::from_owned_fd_with_key(value, None).await
    }

    /// Starts a VFS client on an already-connected Unix-domain socket file
    /// descriptor, proving knowledge of a pre-shared key.
    #[cfg(unix)]
    pub(crate) async fn from_owned_fd_with_key(
        value: OwnedFd,
        key: Option<AuthKey>,
    ) -> Result<Self> {
        let stream = StdUnixStream::from(value);
        stream.set_nonblocking(true)?;
        Self::from_std_stream(stream, key).await
    }

    /// Starts a VFS client on the server end of a connected Windows named pipe.
    ///
    /// # Safety
    ///
    /// `server_process` must identify the trusted process at the other end of
    /// the pipe. That process can transfer handles which this process adopts.
    #[cfg(any(windows, docsrs))]
    #[cfg_attr(docsrs, doc(cfg(windows)))]
    #[cfg_attr(all(docsrs, not(windows)), allow(private_interfaces))]
    pub(crate) async unsafe fn from_named_pipe_server(
        pipe: NamedPipeServer,
        server_process: OwnedHandle,
    ) -> Result<Self> {
        #[cfg(windows)]
        {
            let rpc = unsafe { rpc_builder(None).client_named_pipe_server(pipe, server_process) }
                .await?
                .bind();
            Self::initialize(rpc, SessionMode::Native, None).await
        }
        #[cfg(all(docsrs, not(windows)))]
        {
            let _ = (pipe, server_process);
            unreachable!()
        }
    }

    fn unsupported<T>(&self, operation: &str) -> Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{operation} is not supported by a remote VFS session"),
        )
        .into())
    }

    fn call(&self, request: RequestKind) -> Call<VfsProtocol> {
        self.shared.rpc.call(Request {
            vfs: self.vfs.as_ref().map(Gift::cite),
            kind: request,
        })
    }

    fn call_with_trailer(&self, request: RequestKind) -> TrailerSend<Call<VfsProtocol>> {
        self.shared.rpc.call_with_trailer(Request {
            vfs: self.vfs.as_ref().map(Gift::cite),
            kind: request,
        })
    }

    pub(crate) async fn request(&self, request: RequestKind) -> Result<ResponseKind> {
        self.call(request).await?.into_response()
    }

    async fn unix_vfs(&self, path: Utf8TypedPath<'_>, key: Option<&[u8]>) -> Result<Vfs> {
        // The key is sent because the peer may have to establish the nested
        // connection itself (the `Opaque` arm below), and which arm it takes
        // is its decision, not ours. When it returns a descriptor instead, we
        // authenticate locally and its copy goes unused.
        let request = UnixVfsRequest {
            path: path.into(),
            key: key.map(<[u8]>::to_vec),
        };
        match self.request(RequestKind::UnixVfs(request)).await? {
            ResponseKind::UnixVfs(result) => match result {
                OpenVfsHandle::Native(handle) => {
                    #[cfg(unix)]
                    {
                        let key = key
                            .map(AuthKey::new)
                            .transpose()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
                        Ok(Vfs::from_client(
                            Self::from_owned_fd_with_key(handle.into_inner(), key).await?,
                        ))
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = handle;
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "received a native Unix VFS connection on a non-Unix host",
                        )
                        .into())
                    }
                }
                OpenVfsHandle::Opaque(vfs) => Ok(Vfs::from_client(
                    Self::initialize(self.shared.rpc.clone(), self.shared.mode, Some(vfs)).await?,
                )),
            },
            response => Err(unexpected(response).into()),
        }
    }

    async fn windows_admin_vfs(
        &self,
        cwd: Utf8TypedPath<'_>,
        env: HashMap<String, Option<String>>,
        elevate: bool,
    ) -> Result<Vfs> {
        let request = WindowsAdminRequest {
            cwd: cwd.into(),
            env,
            elevate,
        };
        match self.request(RequestKind::WindowsAdmin(request)).await? {
            ResponseKind::WindowsAdmin(vfs) => {
                let client =
                    Self::initialize(self.shared.rpc.clone(), self.shared.mode, Some(vfs)).await?;
                #[cfg(windows)]
                let client = {
                    let mut client = client;
                    client.process.clone_from(&self.process);
                    client
                };
                Ok(Vfs::from_client(client))
            }
            response => Err(unexpected(response).into()),
        }
    }

    /// Calls a registered VFS extension.
    ///
    /// The extension must be linked into both this process and the peer
    /// serving the connection (whether that peer is a remote `dolang-vfs`
    /// process or, when `mode == SessionMode::Native`, this same process's
    /// direct backend).
    pub async fn call_extension<T: VfsExtension>(
        &self,
        request: T::Request,
    ) -> Result<T::Response> {
        let wire = RequestKind::Extension(ExtensionRequest {
            name: T::NAME.to_string(),
            version: T::VERSION,
            payload: Box::new(request),
        });
        match self.request(wire).await? {
            ResponseKind::Extension(ExtensionResponse { payload, .. }) => Ok(*payload
                .downcast::<T::Response>()
                .expect("response type matches the extension that produced it")),
            response => Err(unexpected(response).into()),
        }
    }

    /// Signal the daemon to stop accepting new connections.
    pub async fn stop(&self) -> Result<()> {
        let stop_result = match self.request(RequestKind::Stop).await {
            Ok(ResponseKind::Stop) => Ok(()),
            Ok(response) => Err(Error::from(unexpected(response))),
            Err(error) => Err(error),
        };
        #[cfg(windows)]
        if let Some(process) = &self.process {
            if stop_result.is_ok() {
                self.clone().close().await;
            } else {
                process.terminate();
            }
            let wait_result = process.stop().await.map_err(Error::from);
            return stop_result.and(wait_result);
        }
        stop_result
    }
}

fn unexpected(response: ResponseKind) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected RPC response: {response:?}"),
    )
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

/// Builder for constructing a process-spawn request on a remote VFS.
///
/// Configure arguments, environment, working directory, and standard streams,
/// then call [`spawn`](crate::process::Command::spawn). This concrete API accepts host
/// [`Path`] values; use [`Vfs::command`]
/// when the target's path syntax may differ from the host's.
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
pub struct Command<'a> {
    client: &'a Client,
    program: WirePath,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<WirePath>,
    stdin: ClientRecv,
    stdout: ClientSend,
    stderr: ClientSend,
    process_control: ProcessControl,
    termination_policy: TerminationPolicy,
}

/// A process spawned by a [`Client`].
///
/// It implements [`Child`]; any relay tasks for cross-domain
/// standard streams are owned by this value.
pub struct Child {
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
    Live(Gift<ChildMarker>),
    Exited(ProcessStatus),
}

/// A writable standard-stream endpoint owned by a remote VFS session.
///
/// This implements [`AsyncWrite`]. Shutting it down
/// closes the corresponding remote endpoint.
pub struct RemoteStdioSend {
    client: Client,
    stdio: Option<Gift<StdioSendMarker>>,
    pending: Option<(StdioSendOperation, Call<VfsProtocol>)>,
    write_body: Option<PendingTrailerWrite>,
}

/// A readable standard-stream endpoint owned by a remote VFS session.
///
/// This implements [`AsyncRead`].
pub struct RemoteStdioRecv {
    client: Client,
    stdio: Option<Gift<StdioRecvMarker>>,
    pending: Option<Call<VfsProtocol>>,
    read_body: Option<PendingTrailerRead>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StdioSendOperation {
    Close,
}

impl fmt::Debug for RemoteStdioSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteStdioSend")
            .field("stdio", &self.stdio)
            .field("pending", &self.pending.as_ref().map(|p| p.0))
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for RemoteStdioRecv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.pending.is_some() {
            return Poll::Ready(Err(io::Error::other(
                "write polled while stdio close is pending",
            )));
        }
        if self.write_body.is_none() {
            let Some(stdio) = self.stdio.as_ref().map(Gift::cite) else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "stdio send resource is closed",
                )));
            };
            self.write_body = Some(PendingTrailerWrite {
                send: Some(
                    self.client
                        .call_with_trailer(RequestKind::StdioSendWrite { stdio }),
                ),
                call: None,
                target: buf.len(),
                sent: 0,
                unreported: 0,
            });
        }
        let pending = self.write_body.as_mut().unwrap();
        if let Some(call) = pending.call.as_mut() {
            match Pin::new(call).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    let target = pending.target;
                    let unreported = pending.unreported;
                    self.write_body = None;
                    match result
                        .map_err(Error::from)?
                        .into_response()
                        .map_err(io::Error::from)?
                    {
                        ResponseKind::StdioSendWrite(result) => {
                            if result != target {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "stdio write response does not acknowledge the submitted trailer",
                                )));
                            }
                            return Poll::Ready(Ok(unreported));
                        }
                        response => return Poll::Ready(Err(unexpected(response))),
                    }
                }
            }
        }
        let remaining = pending.target - pending.sent;
        match Pin::new(pending.send.as_mut().unwrap())
            .poll_write(cx, &buf[..buf.len().min(remaining)])
        {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(n)) => {
                pending.sent += n;
                if pending.sent == pending.target {
                    pending.call = Some(pending.send.take().unwrap().finish());
                    pending.unreported = n;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(Ok(n))
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(pending) = self.write_body.as_mut() {
            if pending.call.is_none() {
                pending.call = Some(pending.send.take().unwrap().finish());
            }
            return match Pin::new(pending.call.as_mut().unwrap()).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    self.write_body = None;
                    Poll::Ready(
                        result
                            .map_err(Error::from)
                            .map_err(io::Error::from)
                            .and_then(|result| {
                                match result.into_response().map_err(io::Error::from)? {
                                    ResponseKind::StdioSendWrite(_) => Ok(()),
                                    response => Err(unexpected(response)),
                                }
                            }),
                    )
                }
            };
        }
        let Some((_operation, _call)) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        Poll::Ready(Err(io::Error::other(
            "flush polled while stdio send close is pending",
        )))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.pending.is_none() {
            match self.as_mut().poll_flush(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
        }
        if self.stdio.is_none() {
            return Poll::Ready(Ok(()));
        }
        if self.pending.is_none() {
            let stdio = self.stdio.as_ref().unwrap().cite();
            self.pending = Some((
                StdioSendOperation::Close,
                self.client.call(RequestKind::StdioSendClose { stdio }),
            ));
        }
        let (operation, call) = self.pending.as_mut().unwrap();
        debug_assert_eq!(*operation, StdioSendOperation::Close);
        match Pin::new(call).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                match result
                    .map_err(Error::from)?
                    .into_response()
                    .map_err(io::Error::from)?
                {
                    ResponseKind::StdioSendClose => {
                        self.stdio.take();
                        Poll::Ready(Ok(()))
                    }
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
        loop {
            if let Some(body) = self.read_body.as_mut() {
                let before = buf.filled().len();
                match Pin::new(&mut body.recv).poll_read(cx, buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        self.read_body = None;
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(())) => {
                        let read = buf.filled().len() - before;
                        if read > body.remaining {
                            self.read_body = None;
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "stdio read response exceeds requested length",
                            )));
                        }
                        body.remaining -= read;
                        body.read += read;
                        if read > 0 {
                            return Poll::Ready(Ok(()));
                        }
                        let empty = body.read == 0;
                        self.read_body = None;
                        if empty {
                            return Poll::Ready(Ok(()));
                        }
                        continue;
                    }
                }
            }
            if self.pending.is_none() {
                if buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
                let Some(stdio) = &self.stdio else {
                    return Poll::Ready(Ok(()));
                };
                self.pending = Some(self.client.call(RequestKind::StdioRecvRead {
                    stdio: stdio.cite(),
                    len: buf.remaining(),
                }));
            }
            match Pin::new(self.pending.as_mut().unwrap()).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    self.pending = None;
                    let (response, trailer) = result.map_err(Error::from)?.into_response_trailer();
                    match response.map_err(io::Error::from)? {
                        ResponseKind::StdioRecvRead => {
                            let Some(data) = trailer else {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "stdio read response is missing its data trailer",
                                )));
                            };
                            let requested = buf.remaining();
                            self.read_body = Some(PendingTrailerRead {
                                recv: data,
                                remaining: requested,
                                read: 0,
                            });
                            continue;
                        }
                        response => return Poll::Ready(Err(unexpected(response))),
                    }
                }
            }
        }
    }
}

impl RemoteStdioSend {
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) async fn try_clone(&self) -> Result<Self> {
        if self.pending.is_some() {
            return Err(Error::new(
                ErrorKind::ResourceBusy,
                "cannot clone stdio send while an operation is pending",
            ));
        }
        let stdio = self.stdio.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "stdio send resource is closed")
        })?;
        match self
            .client
            .request(RequestKind::StdioSendClone {
                stdio: stdio.cite(),
            })
            .await?
        {
            ResponseKind::StdioSendClone(stdio) => Ok(Self {
                client: self.client.clone(),
                stdio: Some(stdio),
                pending: None,
                write_body: None,
            }),
            response => Err(unexpected(response).into()),
        }
    }
}

impl RemoteStdioRecv {
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) async fn try_clone(&self) -> Result<Self> {
        if self.pending.is_some() {
            return Err(Error::new(
                ErrorKind::ResourceBusy,
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
                stdio: stdio.cite(),
            })
            .await?
        {
            ResponseKind::StdioRecvClone(stdio) => Ok(Self {
                client: self.client.clone(),
                stdio: Some(stdio),
                pending: None,
                read_body: None,
            }),
            response => Err(unexpected(response).into()),
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
    Stdout,
    Inherit(HostOutput),
    Native(DefaultHandle),
    Resource(StdioSend),
}

impl<'a> Command<'a> {
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
            process_control: ProcessControl::Foreground,
            termination_policy: TerminationPolicy::default(),
        }
    }

    async fn prepare_recv(
        client: &Client,
        stdio: ClientRecv,
        relays: &mut PreparedRelays,
    ) -> Result<StdioRecvTarget> {
        match stdio {
            ClientRecv::Null => Ok(StdioRecvTarget::Null),
            ClientRecv::Inherit => {
                let (send, recv) = client.pipe(None).await?;
                relays.stdin = Some(send);
                let StdioRecv(StdioRecvInner::Remote(remote)) = recv else {
                    return Err(io::Error::other(
                        "remote pipe unexpectedly returned a native receive endpoint",
                    )
                    .into());
                };
                Self::prepare_remote_recv(client, remote)
            }
            ClientRecv::Native(handle) => {
                if client.mode() == SessionMode::Remote {
                    return client.unsupported("native process stdio");
                }
                Ok(StdioRecvTarget::Native(OsHandle::new(handle)))
            }
            ClientRecv::Resource(stdio) => match stdio.0 {
                StdioRecvInner::Native(native) => {
                    if client.mode() == SessionMode::Remote {
                        return client.unsupported("native process stdio");
                    }
                    let handle = StdioRecv(StdioRecvInner::Native(native))
                        .into_blocking_handle()
                        .await?;
                    Ok(StdioRecvTarget::Native(OsHandle::new(handle)))
                }
                StdioRecvInner::Remote(remote) => Self::prepare_remote_recv(client, remote),
            },
        }
    }

    fn prepare_remote_recv(client: &Client, remote: RemoteStdioRecv) -> Result<StdioRecvTarget> {
        if !client.is_same_vfs(&remote.client) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "stdio receive belongs to a different VFS session",
            ));
        }
        Ok(StdioRecvTarget::Opaque(
            remote.stdio.as_ref().unwrap().cite(),
        ))
    }

    async fn prepare_send(
        client: &Client,
        stdio: ClientSend,
        relays: &mut PreparedRelays,
    ) -> Result<StdioSendTarget> {
        match stdio {
            ClientSend::Null => Ok(StdioSendTarget::Null),
            ClientSend::Stdout => Ok(StdioSendTarget::Stdout),
            ClientSend::Inherit(output) => {
                let (send, recv) = client.pipe(None).await?;
                relays.outputs.push((recv, output));
                let StdioSend(StdioSendInner::Remote(remote)) = send else {
                    return Err(io::Error::other(
                        "remote pipe unexpectedly returned a native send endpoint",
                    )
                    .into());
                };
                Self::prepare_remote_send(client, remote)
            }
            ClientSend::Native(handle) => {
                if client.mode() == SessionMode::Remote {
                    return client.unsupported("native process stdio");
                }
                Ok(StdioSendTarget::Native(OsHandle::new(handle)))
            }
            ClientSend::Resource(stdio) => match stdio.0 {
                StdioSendInner::Native(native) => {
                    if client.mode() == SessionMode::Remote {
                        return client.unsupported("native process stdio");
                    }
                    let handle = StdioSend(StdioSendInner::Native(native))
                        .into_blocking_handle()
                        .await?;
                    Ok(StdioSendTarget::Native(OsHandle::new(handle)))
                }
                StdioSendInner::Remote(remote) => Self::prepare_remote_send(client, remote),
            },
        }
    }

    fn prepare_remote_send(client: &Client, remote: RemoteStdioSend) -> Result<StdioSendTarget> {
        if !client.is_same_vfs(&remote.client) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "stdio send belongs to a different VFS session",
            ));
        }
        Ok(StdioSendTarget::Opaque(
            remote.stdio.as_ref().unwrap().cite(),
        ))
    }

    async fn prepare_outputs(
        client: &Client,
        stdout: ClientSend,
        stderr: ClientSend,
        relays: &mut PreparedRelays,
    ) -> Result<(StdioSendTarget, StdioSendTarget)> {
        let stdout = Self::prepare_send(client, stdout, relays).await?;
        let stderr = Self::prepare_send(client, stderr, relays).await?;
        Ok((stdout, stderr))
    }
}

async fn relay_stdin(mut send: StdioSend) {
    let mut stdin = BufReader::with_capacity(STREAM_CHUNK_SIZE, tokio::io::stdin());
    let _ = tokio::io::copy_buf(&mut stdin, &mut send).await;
    let _ = send.shutdown().await;
}

async fn relay_output<W>(recv: StdioRecv, mut output: W)
where
    W: AsyncWrite + Unpin,
{
    let mut recv = BufReader::with_capacity(STREAM_CHUNK_SIZE, recv);
    let _ = tokio::io::copy_buf(&mut recv, &mut output).await;
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

impl Child {
    fn result(&self) -> Option<Result<ProcessStatus>> {
        match &self.state {
            ClientChildState::Live(_) => None,
            ClientChildState::Exited(status) => Some(Ok(*status)),
        }
    }

    fn store_result(&mut self, status: ProcessStatus) {
        self.state = ClientChildState::Exited(status);
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // Reaping the child is the opaque's own business: dropping the last
        // handle on it releases the registration, which drops the retained
        // child on the server. Only the relays need winding down here.
        self.relays.finish();
    }
}

impl Child {
    pub(crate) async fn wait(&mut self) -> Result<ProcessStatus> {
        if let Some(result) = self.result() {
            return result;
        }
        let ClientChildState::Live(child) = &self.state else {
            unreachable!();
        };
        match self
            .client
            .request(RequestKind::ChildWait {
                child: child.cite(),
            })
            .await?
        {
            ResponseKind::ChildWait(result) => {
                self.relays.finish();
                self.store_result(result);
                self.result().unwrap()
            }
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn terminate(mut self) -> Result<Option<ProcessStatus>> {
        self.relays.abort_stdin();
        if let Some(result) = self.result() {
            return result.map(Some);
        }
        let ClientChildState::Live(child) = &self.state else {
            unreachable!();
        };
        match self
            .client
            .request(RequestKind::ChildTerminate {
                child: child.cite(),
            })
            .await?
        {
            ResponseKind::ChildTerminate(result) => {
                self.relays.finish();
                if let Some(status) = result {
                    self.state = ClientChildState::Exited(status);
                }
                Ok(result)
            }
            response => Err(unexpected(response).into()),
        }
    }
}

impl<'a> Command<'a> {
    pub(crate) fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(arg.to_owned());
        self
    }

    pub(crate) fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.env.insert(key.to_owned(), Some(val.to_owned()));
        self
    }

    pub(crate) fn env_remove(&mut self, key: &str) -> &mut Self {
        self.env.insert(key.to_owned(), None);
        self
    }

    pub(crate) fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    pub(crate) fn stdin(&mut self, stdio: StdioRecv) -> Result<&mut Self> {
        if let StdioRecvInner::Remote(remote) = &stdio.0
            && !self.client.is_same_vfs(&remote.client)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "stdio receive belongs to a different VFS session",
            ));
        }
        self.stdin = ClientRecv::Resource(stdio);
        Ok(self)
    }

    pub(crate) fn stdout(&mut self, stdio: StdioSend) -> Result<&mut Self> {
        if let StdioSendInner::Remote(remote) = &stdio.0
            && !self.client.is_same_vfs(&remote.client)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "stdio send belongs to a different VFS session",
            ));
        }
        self.stdout = ClientSend::Resource(stdio);
        Ok(self)
    }

    pub(crate) fn stdin_inherit(&mut self) -> Result<&mut Self> {
        self.stdin = if self.client.mode() == SessionMode::Remote {
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

    pub(crate) fn stdout_inherit(&mut self) -> Result<&mut Self> {
        self.stdout = if self.client.mode() == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stdout)
        } else {
            ClientSend::Native(clone_stdout_handle()?)
        };
        Ok(self)
    }

    pub(crate) fn stdout_inherit_stderr(&mut self) -> Result<&mut Self> {
        self.stdout = if self.client.mode() == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stderr)
        } else {
            ClientSend::Native(clone_stderr_handle()?)
        };
        Ok(self)
    }

    pub(crate) fn stdin_null(&mut self) -> &mut Self {
        self.stdin = ClientRecv::Null;
        self
    }

    pub(crate) fn stdout_null(&mut self) -> &mut Self {
        self.stdout = ClientSend::Null;
        self
    }

    pub(crate) fn stderr(&mut self, stdio: StdioSend) -> Result<&mut Self> {
        if let StdioSendInner::Remote(remote) = &stdio.0
            && !self.client.is_same_vfs(&remote.client)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "stdio send belongs to a different VFS session",
            ));
        }
        self.stderr = ClientSend::Resource(stdio);
        Ok(self)
    }

    pub(crate) fn stderr_inherit(&mut self) -> Result<&mut Self> {
        self.stderr = if self.client.mode() == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stderr)
        } else {
            ClientSend::Native(clone_stderr_handle()?)
        };
        Ok(self)
    }

    pub(crate) fn stderr_to_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = ClientSend::Stdout;
        Ok(self)
    }

    pub(crate) fn stderr_inherit_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = if self.client.mode() == SessionMode::Remote {
            ClientSend::Inherit(HostOutput::Stdout)
        } else {
            ClientSend::Native(clone_stdout_handle()?)
        };
        Ok(self)
    }

    pub(crate) fn stderr_null(&mut self) -> &mut Self {
        self.stderr = ClientSend::Null;
        self
    }

    pub(crate) fn process_control(&mut self, control: ProcessControl) -> &mut Self {
        self.process_control = control;
        self
    }

    pub(crate) fn termination_policy(&mut self, policy: TerminationPolicy) -> &mut Self {
        self.termination_policy = policy;
        self
    }

    pub(crate) async fn spawn(self) -> Result<Child> {
        let Self {
            client,
            program,
            args,
            env,
            cwd,
            stdin,
            stdout,
            stderr,
            process_control,
            termination_policy,
        } = self;
        let mut relays = PreparedRelays::default();
        let stdin = Self::prepare_recv(client, stdin, &mut relays).await?;
        let (stdout, stderr) = Self::prepare_outputs(client, stdout, stderr, &mut relays).await?;
        let req = SpawnRequest {
            program,
            args,
            env,
            cwd,
            stdin,
            stdout,
            stderr,
            process_control,
            termination_policy,
        };
        match client.request(RequestKind::Spawn(req)).await? {
            ResponseKind::Spawn(child) => Ok(Child {
                client: client.clone(),
                state: ClientChildState::Live(child),
                relays: relays.start(),
            }),
            response => Err(unexpected(response).into()),
        }
    }
}

/// Builder for opening files through a [`Client`].
///
/// Configure access and creation modes, then call
/// [`OpenOptions::open`](crate::file::OpenOptions::open). This concrete API accepts
/// host [`Path`] values; use
/// [`Vfs::open_options`] when the target's path
/// syntax may differ from the host's.
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
    flags: OpenFlags,
}

impl<'a> OpenOptions<'a> {
    fn new(client: &'a Client) -> Self {
        Self {
            client,
            flags: OpenFlags::empty(),
        }
    }
}

impl OpenOptions<'_> {
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.flags.set(OpenFlags::READ, read);
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.flags.set(OpenFlags::WRITE, write);
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.flags.set(OpenFlags::APPEND, append);
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE, create);
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE_NEW, create_new);
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.flags.set(OpenFlags::TRUNCATE, truncate);
        self
    }

    pub fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.flags.set(OpenFlags::NO_FOLLOW, no_follow);
        self
    }

    pub async fn open(&self, path: Utf8TypedPath<'_>) -> Result<VfsFile> {
        let req = OpenRequest {
            path: path.into(),
            flags: self.flags,
        };
        match self.client.request(RequestKind::Open(req)).await? {
            ResponseKind::Open(result) => match result {
                OpenHandle::Native(handle) => Ok(VfsFile::direct(direct::File::from_std(
                    handle.into_inner().into(),
                    self.flags.contains(OpenFlags::READ),
                    self.flags.contains(OpenFlags::WRITE),
                    self.flags.contains(OpenFlags::APPEND),
                ))),
                OpenHandle::Opaque(file) => Ok(VfsFile::client(client::File::new(
                    self.client.clone(),
                    file,
                    self.flags.contains(OpenFlags::APPEND),
                ))),
            },
            response => Err(unexpected(response).into()),
        }
    }
}

impl Client {
    pub fn env(&self) -> Box<dyn Iterator<Item = (String, String)> + '_> {
        Box::new(
            self.shared
                .query
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        )
    }

    pub fn cwd(&self) -> Utf8TypedPath<'_> {
        self.shared.query.cwd.to_path()
    }

    pub fn current_exe(&self) -> Utf8TypedPath<'_> {
        self.shared.query.current_exe.to_path()
    }

    pub fn target(&self) -> &TargetInfo {
        &self.shared.query.target
    }

    pub fn security(&self) -> &SecurityInfo {
        &self.shared.query.security
    }

    pub fn extensions(&self) -> &ExtensionSet {
        &self.shared.query.extensions
    }

    pub fn open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }

    pub fn command(&self, program: Utf8TypedPath<'_>) -> Command<'_> {
        Command::new(self, program)
    }

    pub async fn access(&self, path: Utf8TypedPath<'_>, mode: AccessFlags) -> Result<()> {
        let request = AccessRequest {
            path: path.to_path_buf().into(),
            mode: mode.bits(),
        };
        match self.request(RequestKind::Access(request)).await? {
            ResponseKind::Access => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn unix_socket(&self, path: Utf8TypedPath<'_>, key: Option<&[u8]>) -> Result<Vfs> {
        self.unix_vfs(path, key).await
    }

    pub async fn windows_admin(
        &self,
        cwd: Utf8TypedPath<'_>,
        env: HashMap<String, Option<String>>,
        elevate: bool,
    ) -> Result<Vfs> {
        self.windows_admin_vfs(cwd, env, elevate).await
    }

    pub async fn pipe(&self, buf_size: Option<usize>) -> Result<(StdioSend, StdioRecv)> {
        if self.mode() == SessionMode::Native {
            return process::pipe(buf_size).map_err(Into::into);
        }
        match self.request(RequestKind::Pipe { buf_size }).await? {
            ResponseKind::Pipe(pipe) => Ok({
                (
                    StdioSend::remote(RemoteStdioSend {
                        client: self.clone(),
                        stdio: Some(pipe.send),
                        pending: None,
                        write_body: None,
                    }),
                    StdioRecv::remote(RemoteStdioRecv {
                        client: self.clone(),
                        stdio: Some(pipe.recv),
                        pending: None,
                        read_body: None,
                    }),
                )
            }),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn user_name(&self, uid: u32) -> Result<String> {
        match self.request(RequestKind::UserName { uid }).await? {
            ResponseKind::UserName(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn user_id(&self, name: &str) -> Result<u32> {
        match self
            .request(RequestKind::UserId {
                name: name.to_owned(),
            })
            .await?
        {
            ResponseKind::UserId(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn group_name(&self, gid: u32) -> Result<String> {
        match self.request(RequestKind::GroupName { gid }).await? {
            ResponseKind::GroupName(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn group_id(&self, name: &str) -> Result<u32> {
        match self
            .request(RequestKind::GroupId {
                name: name.to_owned(),
            })
            .await?
        {
            ResponseKind::GroupId(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn sid_name(&self, sid: &Sid) -> Result<SidName> {
        match self
            .request(RequestKind::SidName { sid: sid.clone() })
            .await?
        {
            ResponseKind::SidName(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn account_name(&self, name: &str) -> Result<SidName> {
        match self
            .request(RequestKind::AccountName {
                name: name.to_owned(),
            })
            .await?
        {
            ResponseKind::AccountName(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn resolve_principal_id(
        &self,
        input: PrincipalId,
        want: PrincipalIdKind,
    ) -> Result<PrincipalId> {
        match self
            .request(RequestKind::ResolvePrincipalId { input, want })
            .await?
        {
            ResponseKind::ResolvePrincipalId(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub(crate) async fn read_dir(&self, path: Utf8TypedPath<'_>) -> Result<directory::ReadDir> {
        match self
            .request(RequestKind::ReadDir { path: path.into() })
            .await?
        {
            ResponseKind::ReadDir(read_dir) => Ok(directory::ReadDir::client(ReadDir::new(
                self.clone(),
                read_dir,
            ))),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> Result<Option<Utf8TypedPathBuf>> {
        let request = RequestKind::Which {
            program: program.into(),
            path: path.map(str::to_owned),
            cwd: cwd.map(Into::into),
        };
        match self.request(request).await? {
            ResponseKind::Which(result) => Ok(result.map(Into::into)),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn well_known_path(
        &self,
        key: WellKnownPath,
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> Result<Utf8TypedPathBuf> {
        let request = WellKnownPathRequest {
            key,
            app: app.map(str::to_owned),
            env: env.clone(),
        };
        match self.request(RequestKind::WellKnownPath(request)).await? {
            ResponseKind::WellKnownPath(result) => Ok(result.into()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn clear_cache(&self) -> Result<()> {
        match self.request(RequestKind::ClearCache).await? {
            ResponseKind::ClearCache => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>> {
        let request = XattrsRequest {
            path: path.into(),
            namespace: namespace.into(),
            follow,
        };
        match self.request(RequestKind::Xattrs(request)).await? {
            ResponseKind::Xattrs(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn streams(&self, path: Utf8TypedPath<'_>, follow: bool) -> Result<Vec<StreamEntry>> {
        let request = StreamsRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::Streams(request)).await? {
            ResponseKind::Streams(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>> {
        let request = XattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            follow,
        };
        match self.request(RequestKind::Xattr(request)).await? {
            ResponseKind::Xattr(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<()> {
        let request = SetXattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            value: value.to_vec(),
            follow,
        };
        match self.request(RequestKind::SetXattr(request)).await? {
            ResponseKind::SetXattr => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<()> {
        let request = XattrRequest {
            path: path.into(),
            name: name.to_owned(),
            namespace: namespace.map(str::to_owned),
            follow,
        };
        match self.request(RequestKind::RemoveXattr(request)).await? {
            ResponseKind::RemoveXattr => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn remove(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> Result<()> {
        let request = RemoveRequest {
            path: path.into(),
            all,
            ignore,
        };
        match self.request(RequestKind::Remove(request)).await? {
            ResponseKind::Remove => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata> {
        let request = MetadataRequest { path: path.into() };
        match self.request(RequestKind::Metadata(request)).await? {
            ResponseKind::Metadata(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn fs_metadata(&self, path: Utf8TypedPath<'_>, follow: bool) -> Result<FsMetadata> {
        let request = FsMetadataRequest {
            path: path.into(),
            follow,
        };
        match self.request(RequestKind::FsMetadata(request)).await? {
            ResponseKind::FsMetadata(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn acl(
        &self,
        path: Utf8TypedPath<'_>,
        kind: AclKind,
        default: bool,
        follow: bool,
    ) -> Result<Option<Acl>> {
        let request = AclRequest {
            path: path.into(),
            kind,
            default,
            follow,
        };
        match self.request(RequestKind::Acl(request)).await? {
            ResponseKind::Acl(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn set_acl(
        &self,
        path: Utf8TypedPath<'_>,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
        follow: bool,
    ) -> Result<()> {
        let request = SetAclRequest {
            path: path.into(),
            kind,
            acl: acl.cloned(),
            default,
            follow,
        };
        match self.request(RequestKind::SetAcl(request)).await? {
            ResponseKind::SetAcl => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        mask: dolang_winterop::security::SecInfo,
        follow: bool,
    ) -> Result<SecDesc> {
        let request = SecDescRequest {
            path: path.into(),
            mask,
            follow,
        };
        match self.request(RequestKind::SecDesc(request)).await? {
            ResponseKind::SecDesc(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn set_sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        sec_desc: &SecDesc,
        follow: bool,
    ) -> Result<()> {
        let request = SetSecDescRequest {
            path: path.into(),
            sec_desc: sec_desc.clone(),
            follow,
        };
        match self.request(RequestKind::SetSecDesc(request)).await? {
            ResponseKind::SetSecDesc => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> Result<()> {
        let request = CreateDirRequest {
            path: path.into(),
            all,
        };
        match self.request(RequestKind::CreateDir(request)).await? {
            ResponseKind::CreateDir => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn remove_dir(&self, path: Utf8TypedPath<'_>, all: bool, ignore: bool) -> Result<()> {
        let request = RemoveDirRequest {
            path: path.into(),
            ignore,
            all,
        };
        match self.request(RequestKind::RemoveDir(request)).await? {
            ResponseKind::RemoveDir => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn copy(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> Result<()> {
        let request = CopyRequest {
            from: from.into(),
            to: to.into(),
            all,
        };
        match self.request(RequestKind::Copy(request)).await? {
            ResponseKind::Copy => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn rename(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        replace: bool,
    ) -> Result<()> {
        let request = RenameRequest {
            from: from.into(),
            to: to.into(),
            replace,
        };
        match self.request(RequestKind::Rename(request)).await? {
            ResponseKind::Rename => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn move_(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> Result<()> {
        let request = MoveRequest {
            from: from.into(),
            to: to.into(),
            all,
        };
        match self.request(RequestKind::Move(request)).await? {
            ResponseKind::Move => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<()> {
        let request = SymlinkRequest {
            cwd: cwd.into(),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::Infer,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn hard_link(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> Result<()> {
        let request = HardLinkRequest {
            src: src.into(),
            dst: dst.into(),
        };
        match self.request(RequestKind::HardLink(request)).await? {
            ResponseKind::HardLink => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn symlink_dir(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> Result<()> {
        let request = SymlinkRequest {
            cwd: WirePath::empty_like(src),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::Dir,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn symlink_file(&self, src: Utf8TypedPath<'_>, dst: Utf8TypedPath<'_>) -> Result<()> {
        let request = SymlinkRequest {
            cwd: WirePath::empty_like(src),
            src: src.into(),
            dst: dst.into(),
            kind: SymlinkKind::File,
        };
        match self.request(RequestKind::Symlink(request)).await? {
            ResponseKind::Symlink => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata> {
        let request = MetadataRequest { path: path.into() };
        match self.request(RequestKind::SymlinkMetadata(request)).await? {
            ResponseKind::SymlinkMetadata(result) => Ok(result),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn set_metadata(
        &self,
        paths: &[Utf8TypedPathBuf],
        patch: MetadataPatch,
    ) -> Result<()> {
        let request = SetMetadataRequest {
            paths: paths.iter().map(|path| path.to_path().into()).collect(),
            patch,
        };
        match self.request(RequestKind::SetMetadata(request)).await? {
            ResponseKind::SetMetadata => Ok(()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf> {
        let request = CanonicalizeRequest { path: path.into() };
        match self.request(RequestKind::Canonicalize(request)).await? {
            ResponseKind::Canonicalize(result) => Ok(result.into()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn read_link(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf> {
        let request = ReadLinkRequest { path: path.into() };
        match self.request(RequestKind::ReadLink(request)).await? {
            ResponseKind::ReadLink(result) => Ok(result.into()),
            response => Err(unexpected(response).into()),
        }
    }

    pub async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<Utf8TypedPathBuf>> {
        let request = GlobRequest {
            pattern: pattern.into(),
            root: root.into(),
            follow_symlinks,
            max_depth,
        };
        match self.request(RequestKind::Glob(request)).await? {
            ResponseKind::Glob(result) => {
                Ok(result.into_iter().map(Utf8TypedPathBuf::from).collect())
            }
            response => Err(unexpected(response).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{Client, ClientChildState};
    use crate::{
        error::{Error, ErrorKind},
        protocol::{RequestKind, ResponseKind},
        server::Server,
    };

    #[test]
    fn rpc_errors_have_portable_io_kinds() {
        let cases = [
            (
                dolang_rpc::Error::Serialize("x".into()),
                ErrorKind::InvalidData,
            ),
            (
                dolang_rpc::Error::Deserialize("x".into()),
                ErrorKind::InvalidData,
            ),
            (
                dolang_rpc::Error::Protocol("x".into()),
                ErrorKind::InvalidData,
            ),
            (
                dolang_rpc::Error::Auth("x".into()),
                ErrorKind::PermissionDenied,
            ),
            (
                dolang_rpc::Error::ConnectionClosed,
                ErrorKind::ConnectionReset,
            ),
            (dolang_rpc::Error::Cancelled, ErrorKind::Interrupted),
            (
                dolang_rpc::Error::UnsupportedCapability,
                ErrorKind::Unsupported,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(Error::from(error).kind(), expected);
        }

        let io_error = io::Error::new(io::ErrorKind::WouldBlock, "wait");
        assert_eq!(
            Error::from(dolang_rpc::Error::Io(io_error)).kind(),
            ErrorKind::WouldBlock
        );
    }

    #[cfg(unix)]
    fn successful_command(client: &Client) -> super::Command<'_> {
        use typed_path::{Utf8TypedPath, Utf8UnixPath};

        let mut command = client.command(Utf8TypedPath::Unix(Utf8UnixPath::new("sh")));
        command.arg("-c").arg("exit 0");
        command
    }

    #[cfg(windows)]
    fn successful_command(client: &Client) -> super::Command<'_> {
        use typed_path::{Utf8TypedPath, Utf8WindowsPath};

        let mut command = client.command(Utf8TypedPath::Windows(Utf8WindowsPath::new("cmd")));
        command.arg("/C").arg("exit 0");
        command
    }

    #[tokio::test]
    async fn child_wait_caches_wire_error() {
        let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
        let server =
            tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
        let client = Client::new(client_stream).await.unwrap();
        let mut child = successful_command(&client).spawn().await.unwrap();
        let ClientChildState::Live(opaque) = &child.state else {
            panic!("new child is not live");
        };
        let response = client
            .request(RequestKind::ChildClose {
                child: opaque.cite(),
            })
            .await
            .unwrap();
        let ResponseKind::ChildClose = response else {
            panic!("child close returned the wrong response");
        };

        let first = child.wait().await.unwrap_err();
        let second = child.wait().await.unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(second.kind(), first.kind());
        assert_eq!(second.to_string(), first.to_string());

        client.stop().await.unwrap();
        client.close().await;
        server.await.unwrap().unwrap();
    }
}
