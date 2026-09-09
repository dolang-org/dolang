#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::{
    future::poll_fn,
    io::{self, IoSlice},
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, BufMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

/// Bound on the number of `IoSlice`s gathered from a chained `Buf` for one
/// vectored write attempt.
pub(crate) const MAX_VECTORED_SLICES: usize = 8;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(unix)]
pub(crate) use unix::{EncodeHandles, OutgoingHandles, ReceivedHandles};
#[cfg(windows)]
pub(crate) use windows::{EncodeHandles, OutgoingHandles, ReceivedHandles};

pub(crate) trait Sender: Send + 'static {
    type Send<'a>: SendFrame<'a>
    where
        Self: 'a;

    fn send(&mut self) -> Self::Send<'_>;

    /// Flushes any bytes buffered by the transport itself (as opposed to
    /// bytes still queued in `fragment::Scheduler`, which is a layer above
    /// this trait). Default no-op — raw sockets and pipes write straight
    /// through — `GenericSender` overrides this to flush its wrapped
    /// `AsyncWrite`. Callers are expected to call this once when they have no
    /// more writes to issue for a while (e.g. a writer task about to become
    /// idle or exit), not after every individual write: on transports like
    /// stdio, where writes are dispatched to a background thread, flushing
    /// makes the write visible to the peer but forces a round trip through
    /// that thread, which would serialize otherwise-independent writes if
    /// done after each one.
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Closes the outgoing direction after all writes have drained.
    ///
    /// The default is a no-op for transports whose sender drop already closes
    /// an independent write handle.
    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) trait Receiver: Send + 'static {
    type Recv<'a>: RecvFrame
    where
        Self: 'a;

    fn recv(&mut self) -> Self::Recv<'_>;
}

pub(crate) trait RecvFrame: Send {
    #[cfg(unix)]
    fn drain_fds(&mut self) -> Vec<OwnedFd>;
    fn poll_read_once<B: BufMut>(
        &mut self,
        _cx: &mut Context<'_>,
        _buffer: &mut B,
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "frame does not support direct polling",
        )))
    }

    async fn recv<B: BufMut>(&mut self, buffer: &mut B) -> io::Result<usize> {
        poll_fn(|cx| self.poll_read_once(cx, buffer)).await
    }
}

pub(crate) trait SendFrame<'frame>: Send {
    #[cfg(unix)]
    fn attach_fds(&mut self, fds: &'frame [OwnedFd]) -> io::Result<usize>;
    /// Attempts one nonblocking write of `buf`, following the same
    /// readiness/registration contract as `AsyncWrite::poll_write`. The one
    /// low-level primitive each transport must implement; `finish` (below)
    /// is provided in terms of it, mirroring how `AsyncWriteExt` methods are
    /// built on `AsyncWrite::poll_write`.
    ///
    /// Also used directly by streaming trailer leases, which hold their
    /// shared-state lock only for one synchronous poll.
    fn poll_write_once(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>>;

    /// Vectored variant. Default mirrors `AsyncWrite`'s default
    /// `poll_write_vectored`: write the first non-empty slice via
    /// `poll_write_once`. Transports with vectored-write support override this
    /// so a frame header and payload can be submitted as one write operation.
    fn poll_write_vectored_once(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let buf = bufs
            .iter()
            .find(|slice| !slice.is_empty())
            .map_or(&[][..], |slice| &**slice);
        self.poll_write_once(cx, buf)
    }

    /// Writes all of `buffer`'s remaining bytes, looping
    /// `poll_write_vectored_once` via `Buf::chunks_vectored`. Provided — no
    /// transport implements this directly. Does not flush; see
    /// [`Sender::flush`].
    async fn finish<B: Buf>(mut self, buffer: &mut B) -> io::Result<()>
    where
        Self: Sized,
    {
        while buffer.has_remaining() {
            let mut slices = [IoSlice::new(&[]); MAX_VECTORED_SLICES];
            let filled = buffer.chunks_vectored(&mut slices);
            let sent = poll_fn(|cx| self.poll_write_vectored_once(cx, &slices[..filled])).await?;
            if sent == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame",
                ));
            }
            buffer.advance(sent);
        }
        Ok(())
    }
}

pub(crate) enum AnySender {
    Generic(GenericSender),
    #[cfg(unix)]
    Unix(unix::UnixSender),
    #[cfg(windows)]
    Windows(windows::WindowsSender),
}

pub(crate) enum AnyReceiver {
    Generic(GenericReceiver),
    #[cfg(unix)]
    Unix(unix::UnixReceiver),
    #[cfg(windows)]
    Windows(windows::WindowsReceiver),
}

pub(crate) enum AnySend<'a> {
    Generic(GenericSend<'a>),
    #[cfg(unix)]
    Unix(unix::UnixSend<'a>),
    #[cfg(windows)]
    Windows(windows::WindowsSend<'a>),
}

pub(crate) enum AnyRecv<'a> {
    Generic(GenericRecv<'a>),
    #[cfg(unix)]
    Unix(unix::UnixRecv<'a>),
    #[cfg(windows)]
    Windows(windows::WindowsRecv<'a>),
}

#[cfg(windows)]
pub(crate) enum AnyAttachments {
    Generic,
    Windows(windows::WindowsAttachments),
}

impl AnyReceiver {
    #[cfg(windows)]
    pub(crate) fn duplicate_peer_handle(&self, value: usize) -> io::Result<OwnedHandle> {
        match self {
            Self::Generic(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "generic byte-stream transport does not support handle attachments",
            )),
            Self::Windows(receiver) => receiver.duplicate_peer_handle(value),
        }
    }

    pub(crate) fn set_max_handles_per_fragment(&mut self, value: usize) {
        #[cfg(not(unix))]
        let _ = value;
        #[cfg(unix)]
        if let Self::Unix(receiver) = self {
            receiver.set_max_fds_per_fragment(value);
        }
    }
}

impl AnySender {
    /// Whether ending the sender cannot signal EOF until the paired receiver
    /// is also dropped.
    pub(crate) fn requires_full_close(&self) -> bool {
        match self {
            Self::Generic(_) => false,
            #[cfg(unix)]
            Self::Unix(_) => false,
            #[cfg(windows)]
            Self::Windows(_) => true,
        }
    }
}

#[cfg(windows)]
impl AnySender {
    pub(crate) fn attachments(&self) -> AnyAttachments {
        match self {
            Self::Generic(_) => AnyAttachments::Generic,
            Self::Windows(sender) => AnyAttachments::Windows(sender.attachments()),
        }
    }
}

impl Sender for AnySender {
    type Send<'a> = AnySend<'a>;
    fn send(&mut self) -> Self::Send<'_> {
        match self {
            Self::Generic(sender) => AnySend::Generic(sender.send()),
            #[cfg(unix)]
            Self::Unix(sender) => AnySend::Unix(sender.send()),
            #[cfg(windows)]
            Self::Windows(sender) => AnySend::Windows(sender.send()),
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Generic(sender) => sender.flush().await,
            #[cfg(unix)]
            Self::Unix(sender) => sender.flush().await,
            #[cfg(windows)]
            Self::Windows(sender) => sender.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Generic(sender) => sender.shutdown().await,
            #[cfg(unix)]
            Self::Unix(sender) => sender.shutdown().await,
            #[cfg(windows)]
            Self::Windows(sender) => sender.shutdown().await,
        }
    }
}

impl<'frame> SendFrame<'frame> for AnySend<'frame> {
    #[cfg(unix)]
    fn attach_fds(&mut self, fds: &'frame [OwnedFd]) -> io::Result<usize> {
        match self {
            Self::Generic(frame) => frame.attach_fds(fds),
            Self::Unix(frame) => frame.attach_fds(fds),
        }
    }
    fn poll_write_once(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self {
            Self::Generic(frame) => frame.poll_write_once(cx, buf),
            #[cfg(unix)]
            Self::Unix(frame) => frame.poll_write_once(cx, buf),
            #[cfg(windows)]
            Self::Windows(frame) => frame.poll_write_once(cx, buf),
        }
    }
    fn poll_write_vectored_once(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self {
            Self::Generic(frame) => frame.poll_write_vectored_once(cx, bufs),
            #[cfg(unix)]
            Self::Unix(frame) => frame.poll_write_vectored_once(cx, bufs),
            #[cfg(windows)]
            Self::Windows(frame) => frame.poll_write_vectored_once(cx, bufs),
        }
    }
}

impl Receiver for AnyReceiver {
    type Recv<'a> = AnyRecv<'a>;

    fn recv(&mut self) -> Self::Recv<'_> {
        match self {
            Self::Generic(receiver) => AnyRecv::Generic(receiver.recv()),
            #[cfg(unix)]
            Self::Unix(receiver) => AnyRecv::Unix(receiver.recv()),
            #[cfg(windows)]
            Self::Windows(receiver) => AnyRecv::Windows(receiver.recv()),
        }
    }
}

impl RecvFrame for AnyRecv<'_> {
    #[cfg(unix)]
    fn drain_fds(&mut self) -> Vec<OwnedFd> {
        match self {
            Self::Generic(frame) => frame.drain_fds(),
            Self::Unix(frame) => frame.drain_fds(),
        }
    }
    fn poll_read_once<B: BufMut>(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut B,
    ) -> Poll<io::Result<usize>> {
        match self {
            Self::Generic(frame) => frame.poll_read_once(cx, buffer),
            #[cfg(unix)]
            Self::Unix(frame) => frame.poll_read_once(cx, buffer),
            #[cfg(windows)]
            Self::Windows(frame) => frame.poll_read_once(cx, buffer),
        }
    }
}

pub(crate) struct GenericSender(Pin<Box<dyn AsyncWrite + Send>>);
pub(crate) struct GenericReceiver(Pin<Box<dyn AsyncRead + Send>>);

pub(crate) fn generic<R, W>(reader: R, writer: W) -> (GenericSender, GenericReceiver)
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    (
        GenericSender(Box::pin(writer)),
        GenericReceiver(Box::pin(reader)),
    )
}

pub(crate) fn generic_duplex<T>(stream: T) -> (GenericSender, GenericReceiver)
where
    T: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    generic(reader, writer)
}

pub(crate) struct GenericSend<'a>(&'a mut GenericSender);
pub(crate) struct GenericRecv<'a>(&'a mut GenericReceiver);
impl Sender for GenericSender {
    type Send<'a> = GenericSend<'a>;

    fn send(&mut self) -> Self::Send<'_> {
        GenericSend(self)
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.0.as_mut().flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.0.as_mut().shutdown().await
    }
}

impl Receiver for GenericReceiver {
    type Recv<'a> = GenericRecv<'a>;

    fn recv(&mut self) -> Self::Recv<'_> {
        GenericRecv(self)
    }
}

impl RecvFrame for GenericRecv<'_> {
    #[cfg(unix)]
    fn drain_fds(&mut self) -> Vec<OwnedFd> {
        Vec::new()
    }
    fn poll_read_once<B: BufMut>(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut B,
    ) -> Poll<io::Result<usize>> {
        let chunk = buffer.chunk_mut();
        let mut read_buf = ReadBuf::uninit(unsafe {
            std::slice::from_raw_parts_mut(chunk.as_mut_ptr().cast(), chunk.len())
        });
        match self.0.0.as_mut().poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let n = read_buf.filled().len();
                // SAFETY: `poll_read` initialized the reported filled bytes.
                unsafe { buffer.advance_mut(n) };
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'frame> SendFrame<'frame> for GenericSend<'frame> {
    #[cfg(unix)]
    fn attach_fds(&mut self, fds: &'frame [OwnedFd]) -> io::Result<usize> {
        if fds.is_empty() {
            return Ok(0);
        }
        // Unreachable: `EncodeHandles::put_handle` rejects handle attachments
        // before a message reaches this point, so `fds` is always empty here.
        unreachable!("generic byte-stream transport does not support file descriptors")
    }
    fn poll_write_once(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.0.0.as_mut().poll_write(cx, buf)
    }

    fn poll_write_vectored_once(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.0.0.as_mut().poll_write_vectored(cx, bufs)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    #[should_panic(expected = "generic byte-stream transport does not support file descriptors")]
    async fn generic_send_rejects_file_descriptors() {
        let (stream, _) = tokio::io::duplex(64);
        let (mut sender, _) = generic_duplex(stream);
        let (fd, _) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = OwnedFd::from(fd);
        sender.send().attach_fds(&[fd]).unwrap();
    }

    #[tokio::test]
    async fn generic_receiver_has_no_file_descriptors() {
        let (stream, _) = tokio::io::duplex(64);
        let (_, mut receiver) = generic_duplex(stream);
        assert!(receiver.recv().drain_fds().is_empty());
    }

    #[tokio::test]
    async fn generic_poll_write_once_writes_directly() {
        let (stream, mut other) = tokio::io::duplex(64);
        let (mut sender, _) = generic_duplex(stream);
        let mut send = sender.send();
        poll_fn(|cx| send.poll_write_once(cx, b"direct"))
            .await
            .unwrap();
        let mut buf = [0u8; 6];
        other.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"direct");
    }
}
