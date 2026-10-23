//! Handing native byte I/O handles between extensions.
//!
//! A native object wrapping an OS or VFS handle provides the [`ByteIo`] interface to an
//! [interface query](crate::object::interface). A consumer checks the provider's
//! [`IoCaps`], then takes an owned [`ByteHandle`] over futures-io traits and narrows it
//! to the traits it needs.

use std::{
    fmt,
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::object::interface::InterfaceTag;

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

    /// Transfers the provider's handle, closing the provider.
    ///
    /// Buffered read-ahead travels with the handle as its unread
    /// [prefix](ByteHandle::with_prefix).
    fn take(&mut self) -> ByteHandle;
}

enum Io {
    Read(Box<dyn Read>),
    Write(Box<dyn Write>),
    ReadSeek(Box<dyn ReadSeek>),
    WriteSeek(Box<dyn WriteSeek>),
    ReadWrite(Box<dyn ReadWrite>),
    ReadWriteSeek(Box<dyn ReadWriteSeek>),
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
        match self.io {
            Io::Read(_) => IoCaps::READ,
            Io::Write(_) => IoCaps::WRITE,
            Io::ReadSeek(_) => IoCaps::READ | IoCaps::SEEK,
            Io::WriteSeek(_) => IoCaps::WRITE | IoCaps::SEEK,
            Io::ReadWrite(_) => IoCaps::READ | IoCaps::WRITE,
            Io::ReadWriteSeek(_) => IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK,
        }
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
