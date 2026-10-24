//! Handing native byte I/O handles between extensions.
//!
//! A native object wrapping an OS or VFS handle provides the [`ByteIo`] interface to an
//! [interface query](crate::object::interface). A consumer checks the provider's
//! [`IoCaps`], then takes an owned [`ByteHandle`] over futures-io traits and narrows it
//! to the traits it needs. [`take`] does both, flushing the provider first.
//!
//! [`buffered`] wraps a handle in a Do `io.Buffered`.

mod buffered;

use std::{
    fmt,
    future::poll_fn,
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::{error::Error, object::interface::InterfaceTag, strand::Strand, value::Value};

pub use buffered::buffered;
pub(crate) use buffered::configure;

bitflags::bitflags! {
    /// Capabilities of a [`ByteIo`] provider or [`ByteHandle`].
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct IoCaps: u8 {
        /// The handle reads.
        const READ = 1 << 0;
        /// The handle writes.
        const WRITE = 1 << 1;
        /// The handle seeks.
        const SEEK = 1 << 2;
    }
}

/// An owned reader.
pub trait Read: AsyncRead + Send + Unpin + 'static {}

/// An owned writer.
pub trait Write: AsyncWrite + Send + Unpin + 'static {}

/// An owned seekable reader.
pub trait ReadSeek: Read + AsyncSeek {}

/// An owned seekable writer.
pub trait WriteSeek: Write + AsyncSeek {}

/// An owned reader and writer.
pub trait ReadWrite: Read + Write {}

/// An owned seekable reader and writer.
pub trait ReadWriteSeek: ReadSeek + WriteSeek + ReadWrite {}

impl<T: AsyncRead + Send + Unpin + 'static> Read for T {}
impl<T: AsyncWrite + Send + Unpin + 'static> Write for T {}
impl<T: Read + AsyncSeek> ReadSeek for T {}
impl<T: Write + AsyncSeek> WriteSeek for T {}
impl<T: Read + Write> ReadWrite for T {}
impl<T: ReadSeek + WriteSeek> ReadWriteSeek for T {}

/// Tag for the native byte I/O handle interface.
pub struct ByteIo;

impl InterfaceTag for ByteIo {
    type Interface<'v> = dyn IntoByteIo + 'v;
}

/// Hands over a provider's byte I/O handle.
///
/// A provider lends this from inside its borrowed state, so [`take`](IntoByteIo::take)
/// cannot consume it. After `take`, every method of the same lent interface panics. A
/// provider whose handle was taken stops providing [`ByteIo`] and raises its usual closed
/// error when queried, so a second consumer gets an error rather than a panic.
pub trait IntoByteIo {
    /// The provider's capabilities, checked before taking.
    fn caps(&self) -> IoCaps;

    /// Writes out anything the provider holds back, such as buffered writes.
    ///
    /// The default has nothing to flush.
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = cx;
        Poll::Ready(Ok(()))
    }

    /// Transfers the provider's handle, closing the provider.
    ///
    /// A consumer calls this only after [`poll_flush`](IntoByteIo::poll_flush) returns
    /// `Ready(Ok(()))`. Buffered read-ahead travels with the handle as its unread
    /// [prefix](ByteHandle::with_prefix).
    fn take(&mut self) -> ByteHandle;
}

/// Takes `value`'s byte handle, flushing the provider first.
///
/// `accept` checks the provider's capabilities and raises to refuse it. A value that
/// provides no [`ByteIo`] raises [`ErrorKind::Unsupported`](crate::error::ErrorKind), so
/// a caller can replace that error with its own. A flush error is raised as [`error`]
/// maps it.
pub async fn take<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    mut accept: impl FnMut(&mut Strand<'v, 's>, IoCaps) -> crate::error::Result<'v, 's, ()>,
) -> crate::error::Result<'v, 's, ByteHandle> {
    poll_fn(|cx| {
        let res = value.query::<ByteIo, _>(strand, |strand, io| {
            accept(strand, io.caps())?;
            match io.poll_flush(cx) {
                Poll::Ready(Ok(())) => Ok(Poll::Ready(io.take())),
                Poll::Ready(Err(err)) => Err(error(strand, err)),
                Poll::Pending => Ok(Poll::Pending),
            }
        });
        match res {
            Ok(Poll::Ready(handle)) => Poll::Ready(Ok(handle)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    })
    .await
}

/// Maps an I/O error to the closest runtime error.
///
/// Malformed input raises a `ValueError` and a timeout a `TimeoutError`. Anything else
/// raises a `RuntimeError` with the error's message.
pub fn error<'v, 's>(strand: &mut Strand<'v, 's>, err: io::Error) -> Error<'v, 's> {
    match err.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            Error::value(strand, err.to_string())
        }
        io::ErrorKind::TimedOut => Error::timed_out(strand),
        _ => Error::runtime(strand, err),
    }
}

enum Io {
    Read(Box<dyn Read>),
    Write(Box<dyn Write>),
    ReadSeek(Box<dyn ReadSeek>),
    WriteSeek(Box<dyn WriteSeek>),
    ReadWrite(Box<dyn ReadWrite>),
    ReadWriteSeek(Box<dyn ReadWriteSeek>),
}

impl Io {
    fn caps(&self) -> IoCaps {
        match self {
            Io::Read(_) => IoCaps::READ,
            Io::Write(_) => IoCaps::WRITE,
            Io::ReadSeek(_) => IoCaps::READ | IoCaps::SEEK,
            Io::WriteSeek(_) => IoCaps::WRITE | IoCaps::SEEK,
            Io::ReadWrite(_) => IoCaps::READ | IoCaps::WRITE,
            Io::ReadWriteSeek(_) => IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK,
        }
    }
}

/// An owned, erased byte I/O handle.
///
/// The `into_*` conversions narrow it to the traits a consumer needs. A mismatch hands the
/// handle back.
pub struct ByteHandle {
    io: Io,
    prefix: Vec<u8>,
}

/// Boxes `io` as `$target`, serving `prefix` first if it is not empty.
macro_rules! narrow {
    ($target:ty; $io:expr, $prefix:expr) => {{
        let prefix = $prefix;
        let io: Box<$target> = if prefix.is_empty() {
            $io
        } else {
            Box::new(Prefixed::new($io, prefix))
        };
        io
    }};
}

impl ByteHandle {
    fn new(io: Io) -> Self {
        Self {
            io,
            prefix: Vec::new(),
        }
    }

    /// Creates a reading handle.
    pub fn read(io: impl Read) -> Self {
        Self::new(Io::Read(Box::new(io)))
    }

    /// Creates a writing handle.
    pub fn write(io: impl Write) -> Self {
        Self::new(Io::Write(Box::new(io)))
    }

    /// Creates a seekable reading handle.
    pub fn read_seek(io: impl ReadSeek) -> Self {
        Self::new(Io::ReadSeek(Box::new(io)))
    }

    /// Creates a seekable writing handle.
    pub fn write_seek(io: impl WriteSeek) -> Self {
        Self::new(Io::WriteSeek(Box::new(io)))
    }

    /// Creates a reading and writing handle.
    pub fn read_write(io: impl ReadWrite) -> Self {
        Self::new(Io::ReadWrite(Box::new(io)))
    }

    /// Creates a seekable reading and writing handle.
    pub fn read_write_seek(io: impl ReadWriteSeek) -> Self {
        Self::new(Io::ReadWriteSeek(Box::new(io)))
    }

    /// Sets bytes to read before the underlying I/O, such as a provider's read-ahead.
    ///
    /// On a seekable handle, the prefix must be the bytes immediately before the underlying
    /// position. The handle then reports positions relative to the start of the prefix, and
    /// realigns the underlying position and drops the prefix before its first seek or write.
    ///
    /// # Panics
    ///
    /// Panics if the handle does not read.
    pub fn with_prefix(mut self, prefix: Vec<u8>) -> Self {
        assert!(
            self.caps().contains(IoCaps::READ),
            "prefix on a handle that does not read"
        );
        self.prefix = prefix;
        self
    }

    /// The handle's capabilities.
    pub fn caps(&self) -> IoCaps {
        self.io.caps()
    }

    /// Converts to a reader.
    pub fn into_read(self) -> Result<Box<dyn Read>, Self> {
        let Self { io, prefix } = self;
        Ok(match io {
            Io::Read(io) => narrow!(dyn Read; io, prefix),
            Io::ReadSeek(io) => narrow!(dyn Read; io, prefix),
            Io::ReadWrite(io) => narrow!(dyn Read; io, prefix),
            Io::ReadWriteSeek(io) => narrow!(dyn Read; io, prefix),
            io => return Err(Self { io, prefix }),
        })
    }

    /// Converts to a writer.
    ///
    /// A non-seekable handle discards its prefix.
    pub fn into_write(self) -> Result<Box<dyn Write>, Self> {
        let Self { io, prefix } = self;
        Ok(match io {
            Io::Write(io) => io,
            Io::WriteSeek(io) => io,
            Io::ReadWrite(io) => io,
            Io::ReadWriteSeek(io) => narrow!(dyn Write; io, prefix),
            io => return Err(Self { io, prefix }),
        })
    }

    /// Converts to a seekable reader.
    pub fn into_read_seek(self) -> Result<Box<dyn ReadSeek>, Self> {
        let Self { io, prefix } = self;
        Ok(match io {
            Io::ReadSeek(io) => narrow!(dyn ReadSeek; io, prefix),
            Io::ReadWriteSeek(io) => narrow!(dyn ReadSeek; io, prefix),
            io => return Err(Self { io, prefix }),
        })
    }

    /// Converts to a seekable writer.
    pub fn into_write_seek(self) -> Result<Box<dyn WriteSeek>, Self> {
        let Self { io, prefix } = self;
        Ok(match io {
            Io::WriteSeek(io) => io,
            Io::ReadWriteSeek(io) => narrow!(dyn WriteSeek; io, prefix),
            io => return Err(Self { io, prefix }),
        })
    }

    /// Converts to a reader and writer.
    pub fn into_read_write(self) -> Result<Box<dyn ReadWrite>, Self> {
        let Self { io, prefix } = self;
        Ok(match io {
            Io::ReadWrite(io) => narrow!(dyn ReadWrite; io, prefix),
            Io::ReadWriteSeek(io) => narrow!(dyn ReadWrite; io, prefix),
            io => return Err(Self { io, prefix }),
        })
    }

    /// Converts to a seekable reader and writer.
    pub fn into_read_write_seek(self) -> Result<Box<dyn ReadWriteSeek>, Self> {
        let Self { io, prefix } = self;
        Ok(match io {
            Io::ReadWriteSeek(io) => narrow!(dyn ReadWriteSeek; io, prefix),
            io => return Err(Self { io, prefix }),
        })
    }
}

/// Forwards to the handle's traits, failing with [`io::ErrorKind::Unsupported`] where it
/// has none. Flushing and closing succeed trivially on a handle that does not write.
impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Io::Read(io) => Pin::new(io).poll_read(cx, buf),
            Io::ReadSeek(io) => Pin::new(io).poll_read(cx, buf),
            Io::ReadWrite(io) => Pin::new(io).poll_read(cx, buf),
            Io::ReadWriteSeek(io) => Pin::new(io).poll_read(cx, buf),
            Io::Write(_) | Io::WriteSeek(_) => Poll::Ready(Err(io::ErrorKind::Unsupported.into())),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Io::Write(io) => Pin::new(io).poll_write(cx, buf),
            Io::WriteSeek(io) => Pin::new(io).poll_write(cx, buf),
            Io::ReadWrite(io) => Pin::new(io).poll_write(cx, buf),
            Io::ReadWriteSeek(io) => Pin::new(io).poll_write(cx, buf),
            Io::Read(_) | Io::ReadSeek(_) => Poll::Ready(Err(io::ErrorKind::Unsupported.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Io::Write(io) => Pin::new(io).poll_flush(cx),
            Io::WriteSeek(io) => Pin::new(io).poll_flush(cx),
            Io::ReadWrite(io) => Pin::new(io).poll_flush(cx),
            Io::ReadWriteSeek(io) => Pin::new(io).poll_flush(cx),
            Io::Read(_) | Io::ReadSeek(_) => Poll::Ready(Ok(())),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Io::Write(io) => Pin::new(io).poll_close(cx),
            Io::WriteSeek(io) => Pin::new(io).poll_close(cx),
            Io::ReadWrite(io) => Pin::new(io).poll_close(cx),
            Io::ReadWriteSeek(io) => Pin::new(io).poll_close(cx),
            Io::Read(_) | Io::ReadSeek(_) => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncSeek for Io {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        match self.get_mut() {
            Io::ReadSeek(io) => Pin::new(io).poll_seek(cx, pos),
            Io::WriteSeek(io) => Pin::new(io).poll_seek(cx, pos),
            Io::ReadWriteSeek(io) => Pin::new(io).poll_seek(cx, pos),
            Io::Read(_) | Io::Write(_) | Io::ReadWrite(_) => {
                Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
            }
        }
    }
}

impl fmt::Debug for ByteHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteHandle")
            .field("caps", &self.caps())
            .field("prefix", &self.prefix.len())
            .finish()
    }
}

/// Serves an unread prefix before `inner`.
struct Prefixed<B> {
    prefix: Vec<u8>,
    pos: usize,
    inner: B,
}

impl<B> Prefixed<B> {
    fn new(inner: B, prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }

    fn unread(&self) -> usize {
        self.prefix.len() - self.pos
    }

    fn drop_prefix(&mut self) {
        self.prefix = Vec::new();
        self.pos = 0;
    }
}

impl<B: AsyncSeek + Unpin> Prefixed<B> {
    /// Moves the underlying position back over the unread prefix and drops it.
    fn poll_realign(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.unread() > 0 {
            let back = -i64::try_from(self.unread()).map_err(io::Error::other)?;
            ready!(Pin::new(&mut self.inner).poll_seek(cx, SeekFrom::Current(back)))?;
        }
        self.drop_prefix();
        Poll::Ready(Ok(()))
    }
}

impl<B: AsyncRead + Unpin> AsyncRead for Prefixed<B> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let rest = &this.prefix[this.pos..];
        if rest.is_empty() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        this.pos += n;
        if this.unread() == 0 {
            this.drop_prefix();
        }
        Poll::Ready(Ok(n))
    }
}

impl<B: AsyncSeek + Unpin> AsyncSeek for Prefixed<B> {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let pos = match pos {
            SeekFrom::Current(n) => {
                let adjusted = i64::try_from(this.unread())
                    .ok()
                    .and_then(|unread| n.checked_sub(unread))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                SeekFrom::Current(adjusted)
            }
            pos => pos,
        };
        let res = ready!(Pin::new(&mut this.inner).poll_seek(cx, pos));
        if res.is_ok() {
            this.drop_prefix();
        }
        Poll::Ready(res)
    }
}

impl AsyncWrite for Prefixed<Box<dyn ReadWrite>> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// Realigns before writing, so writes land at the logical position.
impl AsyncWrite for Prefixed<Box<dyn ReadWriteSeek>> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_realign(cx))?;
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}
