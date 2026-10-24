//! Handing native byte I/O handles between extensions.
//!
//! A native object wrapping an OS or VFS handle provides the [`ByteIo`] interface to an
//! [interface query](crate::object::interface). A consumer checks the provider's
//! [`IoCaps`], then gets an owned [`ByteHandle`] over futures-io traits and narrows it to
//! the traits it needs. [`get`] does both.
//!
//! The handle is lent: it shares the provider's stream, which stays open, and only the
//! provider's own `close` closes it. A provider with its own stack, such as a buffer or a
//! decoder, keeps it in a [`Shared`] and hands out clones, so the provider and its
//! consumers see one buffer and one position.
//!
//! A Do subclass of `io.Io` provides [`ByteIo`] through strands that call its methods,
//! and its handle sends them requests.

pub(crate) mod bridge;

use std::{
    fmt,
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll, ready},
};

pub use dolang_util::shared::Shared;
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::{error::Error, object::interface::InterfaceTag, strand::Strand, value::Value};

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
    type Interface<'v, 'a> = dyn IntoByteIo<'v> + 'a;
}

/// Hands out a provider's byte I/O handle.
///
/// A provider lends this from inside its borrowed state. The handle it [gets](IntoByteIo::get)
/// shares the provider's stream rather than taking it, so the provider stays usable, and
/// closing the provider ends the handle too. A handle's own `close` only flushes.
///
/// Each method takes the querying strand, so a provider can call back into the VM. A
/// provider that lends this from its borrowed state holds the borrow during each call, so
/// it must not run code that could borrow the same object.
pub trait IntoByteIo<'v> {
    /// The provider's capabilities, checked before getting the handle.
    fn caps<'s>(&mut self, strand: &mut Strand<'v, 's>) -> crate::error::Result<'v, 's, IoCaps>;

    /// Returns a handle sharing the provider's stream.
    ///
    /// A provider that buffers shares its buffer with the handle, so reads and writes
    /// through either see the other's.
    fn get<'s>(&mut self, strand: &mut Strand<'v, 's>) -> crate::error::Result<'v, 's, ByteHandle>;
}

/// Gets `value`'s byte handle.
///
/// `accept` checks the provider's capabilities and raises to refuse it. A value that
/// provides no [`ByteIo`] raises [`ErrorKind::Unsupported`](crate::error::ErrorKind), so
/// a caller can replace that error with its own. An error from the provider is raised as
/// is.
pub fn get<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    mut accept: impl FnMut(&mut Strand<'v, 's>, IoCaps) -> crate::error::Result<'v, 's, ()>,
) -> crate::error::Result<'v, 's, ByteHandle> {
    value.query::<ByteIo, _>(strand, |strand, io| {
        let caps = io.caps(strand)?;
        accept(strand, caps)?;
        io.get(strand)
    })
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

pub(crate) enum Io {
    Read(Box<dyn Read>),
    Write(Box<dyn Write>),
    ReadSeek(Box<dyn ReadSeek>),
    WriteSeek(Box<dyn WriteSeek>),
    ReadWrite(Box<dyn ReadWrite>),
    ReadWriteSeek(Box<dyn ReadWriteSeek>),
}

impl Io {
    pub(crate) fn caps(&self) -> IoCaps {
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
    pub(crate) io: Io,
    pub(crate) prefix: Vec<u8>,
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

    /// Creates a handle with `caps` over `io`, which implements every trait.
    ///
    /// # Panics
    ///
    /// Panics if `caps` neither reads nor writes.
    pub fn with_caps(io: impl ReadWriteSeek, caps: IoCaps) -> Self {
        let read = caps.contains(IoCaps::READ);
        let write = caps.contains(IoCaps::WRITE);
        let seek = caps.contains(IoCaps::SEEK);
        match (read, write, seek) {
            (true, false, false) => Self::read(io),
            (false, true, false) => Self::write(io),
            (true, false, true) => Self::read_seek(io),
            (false, true, true) => Self::write_seek(io),
            (true, true, false) => Self::read_write(io),
            (true, true, true) => Self::read_write_seek(io),
            (false, false, _) => panic!("handle neither reads nor writes"),
        }
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
