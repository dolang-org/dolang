//! The cursor-based I/O traits of [`File`], implemented once over operations
//! each backend starts.
//!
//! A backend only knows how to start a read, a write or a size query at an
//! offset and drive it to completion, and where to store the position, which
//! every clone of a file shares. Everything else about the cursor lives here:
//! seek resolution, read-ahead, and the rule that one operation is in flight
//! at a time.
//!
//! Operations are per clone. Clones moving the one cursor at once are not
//! serialized, as with file descriptors: each operation reads the position
//! when it starts and stores its own end when it finishes.

use std::{
    fmt,
    future::Future,
    io,
    mem::{self, MaybeUninit},
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll, ready},
};

use bytes::BytesMut;
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use super::{File, FileInner};
use crate::{client, direct, error::Result};

/// An operation of one backend or the other.
pub(crate) enum Either<D, C> {
    Direct(D),
    Client(C),
}

impl<T, D, C> Future for Either<D, C>
where
    D: Future<Output = T> + Unpin,
    C: Future<Output = T> + Unpin,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.get_mut() {
            Self::Direct(op) => Pin::new(op).poll(cx),
            Self::Client(op) => Pin::new(op).poll(cx),
        }
    }
}

impl Either<direct::Body, client::Body> {
    fn poll_read_uninit(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [MaybeUninit<u8>],
    ) -> Poll<io::Result<usize>> {
        match self {
            Self::Direct(body) => Poll::Ready(Ok(body.read_uninit(buf))),
            Self::Client(body) => body.poll_read_uninit(cx, buf),
        }
    }
}

/// A write in flight: bytes are handed over first, then the write completes.
pub(crate) trait WriteOp {
    /// Accepts bytes toward the length the write was started with.
    fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>>;
    /// Completes the write with the bytes accepted so far, returning how many
    /// were written and the position just past them.
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(usize, u64)>>;
}

impl<D: WriteOp, C: WriteOp> WriteOp for Either<D, C> {
    fn poll_send(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self {
            Self::Direct(op) => op.poll_send(cx, buf),
            Self::Client(op) => op.poll_send(cx, buf),
        }
    }

    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(usize, u64)>> {
        match self {
            Self::Direct(op) => op.poll_finish(cx),
            Self::Client(op) => op.poll_finish(cx),
        }
    }
}

type ReadOp = Either<direct::ReadOp, client::ReadOp>;
type Body = Either<direct::Body, client::Body>;
type Write = Either<direct::WriteOp, client::WriteOp>;
type SizeOp = Either<direct::SizeOp, client::SizeOp>;

/// One clone's state for the cursor-based traits, allocated on first use.
///
/// The position advances only as bytes are delivered, so read-ahead can simply
/// be dropped on a seek.
#[derive(Default)]
pub(super) struct Cursor {
    op: Op,
    /// Buffer recycled across reads and writes by backends that copy, so
    /// streaming does not allocate per call.
    scratch: BytesMut,
}

#[derive(Default)]
enum Op {
    #[default]
    Idle,
    /// A read started at the given offset.
    Read(ReadOp, u64),
    /// Bytes read at the position and not yet all delivered.
    Body {
        body: Body,
        /// Offset of the body's next byte. Once the position is anywhere
        /// else, another clone moved it, and the body is dropped.
        at: u64,
        /// Whether any byte has been delivered. A body that ends without
        /// delivering one means end of file.
        delivered: bool,
    },
    Write {
        op: Write,
        target: usize,
        sent: usize,
        /// Bytes already reported to the caller as written. The final chunk is
        /// reported only once the write completes, so a write that fails or
        /// lands short is never reported as done.
        reported: usize,
    },
    /// An end-relative seek resolving the file's length.
    Size(SizeOp, i64),
}

impl fmt::Debug for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = match self.op {
            Op::Idle => "idle",
            Op::Read(..) => "read",
            Op::Body { .. } => "body",
            Op::Write { .. } => "write",
            Op::Size(..) => "size",
        };
        f.debug_struct("Cursor")
            .field("op", &op)
            .finish_non_exhaustive()
    }
}

impl Cursor {
    /// Drops unread read-ahead, keeping its buffer.
    fn drop_body(&mut self) {
        if !matches!(self.op, Op::Body { .. }) {
            return;
        }
        if let Op::Body {
            body: Either::Direct(body),
            ..
        } = mem::take(&mut self.op)
        {
            self.scratch = body.into_scratch();
        }
    }

    /// Ends a write, keeping its buffer.
    fn end_write(&mut self) {
        if let Op::Write {
            op: Either::Direct(op),
            ..
        } = mem::take(&mut self.op)
        {
            self.scratch = op.into_scratch();
        }
    }

    fn is_busy(&self) -> bool {
        !matches!(self.op, Op::Idle | Op::Body { .. })
    }

    /// Abandons whatever is in flight. The client asks the peer to stop and
    /// waits for its answer, so that a request following this one does not
    /// find the file still in use there.
    async fn cancel(&mut self) {
        match mem::take(&mut self.op) {
            Op::Read(Either::Client(op), _) => op.cancel().await,
            Op::Write {
                op: Either::Client(op),
                ..
            } => op.cancel().await,
            Op::Size(Either::Client(op), _) => op.cancel().await,
            _ => {}
        }
    }
}

/// Applies `delta` to `base`, rejecting a negative result as `lseek` does.
fn offset_delta(base: u64, delta: i64) -> io::Result<u64> {
    base.checked_add_signed(delta).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "seek would move the cursor outside the representable range",
        )
    })
}

fn in_progress(what: &str) -> io::Error {
    io::Error::other(format!(
        "file {what} while another operation is in progress"
    ))
}

/// Polls a read to its body. The two backends' bodies differ, so this cannot
/// be `Either`'s own `Future`.
fn poll_read_op(op: &mut ReadOp, cx: &mut Context<'_>) -> Poll<io::Result<Body>> {
    Poll::Ready(match op {
        Either::Direct(op) => ready!(Pin::new(op).poll(cx)).map(Either::Direct),
        Either::Client(op) => ready!(Pin::new(op).poll(cx)).map(Either::Client),
    })
}

impl FileInner {
    /// The cursor position, shared by every clone.
    pub(crate) fn position(&self) -> &AtomicU64 {
        match self {
            Self::Direct(file) => file.position(),
            Self::Client(file) => file.position(),
        }
    }

    fn start_read(&self, offset: u64, len: usize, scratch: &mut BytesMut) -> Result<ReadOp> {
        Ok(match self {
            Self::Direct(file) => Either::Direct(file.start_read(offset, len, scratch)?),
            Self::Client(file) => Either::Client(file.start_read(offset, len)?),
        })
    }

    fn start_write(
        &self,
        offset: u64,
        len: usize,
        scratch: &mut BytesMut,
    ) -> Result<(Write, usize)> {
        Ok(match self {
            Self::Direct(file) => {
                let (op, target) = file.start_write(offset, len, scratch)?;
                (Either::Direct(op), target)
            }
            Self::Client(file) => (Either::Client(file.start_write(offset)?), len),
        })
    }

    fn start_size(&self) -> Result<SizeOp> {
        Ok(match self {
            Self::Direct(file) => Either::Direct(file.start_size()?),
            Self::Client(file) => Either::Client(file.start_size()?),
        })
    }
}

impl File {
    fn parts(&mut self) -> (&FileInner, &mut Cursor) {
        (&self.inner, self.cursor.get_or_insert_default())
    }

    /// Fails if a cursor operation is still in flight.
    pub(super) fn check_idle(&self) -> Result<()> {
        match &self.cursor {
            Some(cursor) if cursor.is_busy() => {
                Err(io::Error::other(format!("file operation {cursor:?} is still pending")).into())
            }
            _ => Ok(()),
        }
    }

    /// Abandons any cursor operation in flight.
    pub(super) async fn cancel_cursor(&mut self) {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.cancel().await;
        }
    }
}

impl File {
    /// Reads at the cursor into uninitialized storage, returning how many
    /// bytes at its front were filled. Zero means end of file, unless `buf`
    /// is empty.
    ///
    /// This is [`AsyncRead::poll_read`] without the requirement that `buf` be
    /// initialized, so a caller reading into fresh memory need not zero it
    /// first. The remote backend writes the reply straight into `buf`.
    pub fn poll_read_uninit(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [MaybeUninit<u8>],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let (inner, cursor) = self.parts();
        let pos = inner.position();
        loop {
            match &mut cursor.op {
                Op::Body { at, .. } if pos.load(Ordering::Relaxed) != *at => cursor.drop_body(),
                Op::Body {
                    body,
                    at,
                    delivered,
                } => {
                    let read = match ready!(body.poll_read_uninit(cx, buf)) {
                        Ok(read) => read,
                        Err(error) => {
                            cursor.drop_body();
                            return Poll::Ready(Err(error));
                        }
                    };
                    if read > 0 {
                        *delivered = true;
                        *at += read as u64;
                        pos.store(*at, Ordering::Relaxed);
                        return Poll::Ready(Ok(read));
                    }
                    let end_of_file = !*delivered;
                    cursor.drop_body();
                    if end_of_file {
                        return Poll::Ready(Ok(0));
                    }
                }
                Op::Read(op, at) => {
                    let at = *at;
                    let result = ready!(poll_read_op(op, cx));
                    cursor.op = Op::Idle;
                    cursor.op = Op::Body {
                        body: result?,
                        at,
                        delivered: false,
                    };
                }
                Op::Idle => {
                    let at = pos.load(Ordering::Relaxed);
                    let op = inner.start_read(at, buf.len(), &mut cursor.scratch)?;
                    cursor.op = Op::Read(op, at);
                }
                _ => return Poll::Ready(Err(in_progress("read polled"))),
            }
        }
    }
}

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // SAFETY: `poll_read_uninit` only writes initialized bytes, so `buf`
        // stays initialized.
        let buf = unsafe { &mut *(buf as *mut [u8] as *mut [MaybeUninit<u8>]) };
        self.get_mut().poll_read_uninit(cx, buf)
    }
}

impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let (inner, cursor) = self.get_mut().parts();
        loop {
            match &mut cursor.op {
                // A write invalidates whatever was read ahead of it.
                Op::Body { .. } => cursor.drop_body(),
                Op::Idle => {
                    let (op, target) = inner.start_write(
                        inner.position().load(Ordering::Relaxed),
                        buf.len(),
                        &mut cursor.scratch,
                    )?;
                    cursor.op = Op::Write {
                        op,
                        target,
                        sent: 0,
                        reported: 0,
                    };
                }
                Op::Write {
                    op,
                    target,
                    sent,
                    reported,
                } => {
                    if *sent < *target {
                        let chunk = &buf[..buf.len().min(*target - *sent)];
                        let sent_now = match ready!(op.poll_send(cx, chunk)) {
                            Ok(0) => Err(io::ErrorKind::WriteZero.into()),
                            result => result,
                        };
                        let sent_now = match sent_now {
                            Ok(sent_now) => sent_now,
                            Err(error) => {
                                cursor.end_write();
                                return Poll::Ready(Err(error));
                            }
                        };
                        *sent += sent_now;
                        if *sent < *target {
                            *reported += sent_now;
                            return Poll::Ready(Ok(sent_now));
                        }
                    }
                    let result = ready!(op.poll_finish(cx));
                    let reported = *reported;
                    cursor.end_write();
                    let (written, end) = result?;
                    inner.position().store(end, Ordering::Relaxed);
                    return Poll::Ready(written.checked_sub(reported).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "file write acknowledged fewer bytes than were accepted",
                        )
                    }));
                }
                _ => return Poll::Ready(Err(in_progress("write polled"))),
            }
        }
    }

    /// Each write reaches the file when it completes, so there is nothing
    /// buffered to push; flushing only has to settle work in flight.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let (inner, cursor) = self.get_mut().parts();
        match &mut cursor.op {
            Op::Idle | Op::Body { .. } => Poll::Ready(Ok(())),
            Op::Read(op, at) => {
                let at = *at;
                let result = ready!(poll_read_op(op, cx));
                cursor.op = Op::Idle;
                cursor.op = Op::Body {
                    body: result?,
                    at,
                    delivered: false,
                };
                Poll::Ready(Ok(()))
            }
            Op::Write { op, .. } => {
                let result = ready!(op.poll_finish(cx));
                cursor.end_write();
                let (_, end) = result?;
                inner.position().store(end, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
            Op::Size(op, delta) => {
                let delta = *delta;
                let result = ready!(Pin::new(op).poll(cx));
                cursor.op = Op::Idle;
                let end = offset_delta(result?, delta)?;
                inner.position().store(end, Ordering::Relaxed);
                Poll::Ready(Ok(()))
            }
        }
    }

    /// Only flushes: the file is closed by [`File::close`], or when dropped.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncSeek for File {
    /// An end-relative seek has to ask for the file's length, so it may be
    /// pending; polling again with the same `position` drives that query.
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        position: io::SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let (inner, cursor) = self.get_mut().parts();
        let pos = inner.position();
        loop {
            match (&mut cursor.op, position) {
                (Op::Size(op, delta), io::SeekFrom::End(requested)) if *delta == requested => {
                    let result = ready!(Pin::new(op).poll(cx));
                    cursor.op = Op::Idle;
                    pos.store(offset_delta(result?, requested)?, Ordering::Relaxed);
                    return Poll::Ready(Ok(pos.load(Ordering::Relaxed)));
                }
                _ => cursor.drop_body(),
            }
            if cursor.is_busy() {
                return Poll::Ready(Err(in_progress("seek started")));
            }
            match position {
                // Both of these resolve locally: no syscall, and for the remote
                // backend no round trip either.
                io::SeekFrom::Start(offset) => pos.store(offset, Ordering::Relaxed),
                io::SeekFrom::Current(delta) => {
                    pos.store(
                        offset_delta(pos.load(Ordering::Relaxed), delta)?,
                        Ordering::Relaxed,
                    );
                }
                // Only an end-relative seek needs the file's current length,
                // and that answer is stale the moment it is produced — as it
                // is for `lseek(SEEK_END)` too.
                io::SeekFrom::End(delta) => {
                    cursor.op = Op::Size(inner.start_size()?, delta);
                    continue;
                }
            }
            return Poll::Ready(Ok(pos.load(Ordering::Relaxed)));
        }
    }
}
