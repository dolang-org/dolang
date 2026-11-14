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
//!
//! Errors cross a handle as [`io::Error`]s. An exception from a Do stream method becomes
//! one through the [`IoError`] interface or the core table, and a consumer maps a
//! handle's error back with [`error`], which defers to the VM's
//! [I/O error handler](crate::vm::Register::io_error_handler).

pub(crate) mod bridge;

use std::{
    fmt,
    io::{self, IoSlice, SeekFrom},
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, ready},
};

pub use dolang_util::shared::Shared;
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use crate::{
    error::{Error, ErrorKind},
    object::{interface::InterfaceTag, native::Mut},
    stdlib::io::{Concat, ConcatType},
    strand::Strand,
    value::{Output, Value},
    vm::AllocExt,
    with_instance, with_slots,
};

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
        /// The handle [forks](ByteHandle::fork): it creates handles over the same backing
        /// store, each with a position of its own.
        const FORK = 1 << 3;
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
    /// through either see the other's. The handle's [forks](ByteHandle::fork) are the
    /// exception: they don't share it.
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

/// Gets a read handle over the concatenated bytes of `iterable`'s items, each followed by
/// `end`, as `io.Concat` reads them.
///
/// The handle pulls items as it reads, on strands owned by the handle, which keep the
/// iterable alive until the handle is dropped. An exception from the iterable reaches the
/// reader as an [`io::Error`] made by [`from_do`].
pub fn concat<'v, 's>(
    strand: &mut Strand<'v, 's>,
    iterable: &Value<'v>,
    end: &[u8],
) -> crate::error::Result<'v, 's, ByteHandle> {
    let ty = *strand.force_state::<ConcatType>();
    with_slots!(strand, |mut concat| {
        ty.create(strand, Concat::new(end.to_vec()), &mut concat);
        with_instance!(strand, ty.cast(&concat).unwrap(), |inst| {
            let mut borrow = inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), iterable);
        });
        get(strand, &concat, |_, _| Ok(()))
    })
}

/// Tag for the interface that turns a Do error into an [`io::Error`].
///
/// When a Do stream method raises, the bridge queries the exception for this, so a
/// native error class can keep its kind, code and message across a byte handle. An
/// exception that doesn't answer maps through the core table: a `ValueError` becomes
/// [`InvalidInput`](io::ErrorKind::InvalidInput), a timeout
/// [`TimedOut`](io::ErrorKind::TimedOut), and anything else
/// [`Other`](io::ErrorKind::Other).
pub struct IoError;

impl InterfaceTag for IoError {
    type Interface<'v, 'a> = dyn IntoIoError + 'a;
}

/// Converts a Do error to an [`io::Error`] for a byte handle.
pub trait IntoIoError {
    /// Returns the error as an [`io::Error`].
    ///
    /// The error should keep its message without any prefix naming its kind, so that
    /// mapping it back doesn't add one.
    fn io_error(&mut self) -> io::Error;
}

/// A core error class in the default mapping.
#[derive(Clone, Copy, Debug)]
enum CoreClass {
    Value,
    Data,
    TimedOut,
    Runtime,
}

/// A Do error that crossed a byte handle in its core form.
///
/// [`error`] recognizes it, so it comes back as its own class even when an
/// [I/O error handler](crate::vm::Register::io_error_handler) is installed.
#[derive(Debug)]
struct CoreError {
    class: CoreClass,
    message: String,
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

/// Converts a Do error raised by a stream method to an [`io::Error`].
///
/// The exception answers [`IoError`] if it can, and maps through the core table
/// otherwise.
pub(crate) fn from_do<'v, 's>(strand: &mut Strand<'v, 's>, mut err: Error<'v, 's>) -> io::Error {
    if let Some(exception) = err.exception(strand)
        && let Ok(err) = exception.query::<IoError, _>(strand, |_, iface| Ok(iface.io_error()))
    {
        return err;
    }
    let (kind, class, message) = match err.kind() {
        ErrorKind::Value => match err.value_message() {
            Some(message) => (io::ErrorKind::InvalidInput, CoreClass::Value, message),
            None => (
                io::ErrorKind::InvalidInput,
                CoreClass::Value,
                err.display(strand).to_string(),
            ),
        },
        ErrorKind::Data => match err.value_message() {
            Some(message) => (io::ErrorKind::InvalidData, CoreClass::Data, message),
            None => (
                io::ErrorKind::InvalidData,
                CoreClass::Data,
                err.display(strand).to_string(),
            ),
        },
        ErrorKind::TimedOut => (
            io::ErrorKind::TimedOut,
            CoreClass::TimedOut,
            err.display(strand).to_string(),
        ),
        _ => (
            io::ErrorKind::Other,
            CoreClass::Runtime,
            err.display(strand).to_string(),
        ),
    };
    io::Error::new(kind, CoreError { class, message })
}

/// Maps an I/O error to a Do error.
///
/// An error a Do stream raised through the bridge comes back as its core class. The
/// VM's [I/O error handler](crate::vm::Register::io_error_handler) maps anything else,
/// and without one, the core table applies: invalid input raises a `ValueError`,
/// invalid data a `DataError`, a timeout a `TimedOutError`, and anything else a
/// `RuntimeError` with the error's message.
pub fn error<'v, 's>(strand: &mut Strand<'v, 's>, err: io::Error) -> Error<'v, 's> {
    let err = match err.downcast::<CoreError>() {
        Ok(CoreError { class, message }) => return core_error(strand, class, message),
        Err(err) => err,
    };
    if let Some(handler) = strand.vm().io_error_handler.borrow().as_ref() {
        return handler(strand, err);
    }
    let class = match err.kind() {
        io::ErrorKind::InvalidInput => CoreClass::Value,
        io::ErrorKind::InvalidData => CoreClass::Data,
        io::ErrorKind::TimedOut => CoreClass::TimedOut,
        _ => CoreClass::Runtime,
    };
    core_error(strand, class, err.to_string())
}

/// Maps an I/O error from a decoder to a Do error.
///
/// Like [`error`], except that a library error describing the input raises a
/// `DataError`. Many decoders report malformed input as
/// [`io::ErrorKind::Other`] and truncated input as
/// [`io::ErrorKind::UnexpectedEof`]. An error of either kind that has no OS code
/// and did not come from a Do stream is taken to be such a report.
pub fn decode_error<'v, 's>(strand: &mut Strand<'v, 's>, err: io::Error) -> Error<'v, 's> {
    let described = matches!(
        err.kind(),
        io::ErrorKind::Other | io::ErrorKind::UnexpectedEof
    ) && err.raw_os_error().is_none()
        && !err.get_ref().is_some_and(|inner| inner.is::<CoreError>());
    if described {
        Error::data(strand, err.to_string())
    } else {
        error(strand, err)
    }
}

fn core_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    class: CoreClass,
    message: String,
) -> Error<'v, 's> {
    match class {
        CoreClass::Value => Error::value(strand, message),
        CoreClass::Data => Error::data(strand, message),
        CoreClass::TimedOut => Error::timed_out(strand),
        CoreClass::Runtime => Error::runtime(strand, message),
    }
}

/// [`io::Error`] extension trait.
pub trait IoErrorExt {
    /// Maps the error to a Do error with [`error`].
    fn into_do_io<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's>;

    /// Maps the error to a Do error with [`decode_error`].
    fn into_do_decode<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's>;
}

impl IoErrorExt for io::Error {
    fn into_do_io<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        error(strand, self)
    }

    fn into_do_decode<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
        decode_error(strand, self)
    }
}

/// [`io::Result`] extension trait.
pub trait IoResultExt<T> {
    /// Maps the error to a Do error with [`error`].
    fn into_do_io<'v, 's>(self, strand: &mut Strand<'v, 's>) -> crate::error::Result<'v, 's, T>;

    /// Maps the error to a Do error with [`decode_error`].
    fn into_do_decode<'v, 's>(self, strand: &mut Strand<'v, 's>)
    -> crate::error::Result<'v, 's, T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn into_do_io<'v, 's>(self, strand: &mut Strand<'v, 's>) -> crate::error::Result<'v, 's, T> {
        self.map_err(|err| error(strand, err))
    }

    fn into_do_decode<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
    ) -> crate::error::Result<'v, 's, T> {
        self.map_err(|err| decode_error(strand, err))
    }
}

/// Forks a seekable stream: creates a handle over the same backing store with a position of
/// its own.
///
/// A fork and the stream it came from may be used at once, including from different strands,
/// and each behaves like a separate open of the store taken when the fork was created. A
/// stream that buffers writes out pending writes first and doesn't share its buffer with the
/// fork.
pub trait AsyncFork {
    /// Attempts to create a fork whose position starts at the absolute `offset`, whatever
    /// this stream's own position.
    fn poll_fork(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        offset: u64,
    ) -> Poll<io::Result<ByteHandle>>;
}

impl<T: AsyncFork + Unpin + ?Sized> AsyncFork for Box<T> {
    fn poll_fork(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        offset: u64,
    ) -> Poll<io::Result<ByteHandle>> {
        Pin::new(&mut **self.get_mut()).poll_fork(cx, offset)
    }
}

/// Forks the stream while holding it, after any operation through another handle.
impl<T: AsyncFork + Unpin> AsyncFork for Shared<T> {
    fn poll_fork(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        offset: u64,
    ) -> Poll<io::Result<ByteHandle>> {
        self.get_mut()
            .poll_op(cx, |io, cx| io.poll_fork(cx, offset))
    }
}

/// An erased byte stream: every futures-io trait and [`AsyncFork`], whatever the handle's
/// [`IoCaps`].
///
/// A method outside the capabilities fails with [`io::ErrorKind::Unsupported`], except that
/// flushing and closing succeed trivially on a stream that does not write. The `into_*`
/// conversions of [`ByteHandle`] check the capabilities and upcast.
pub(crate) trait ByteStream: ReadWriteSeek + AsyncFork {}

/// Marks a trait [`Adapt`] forwards.
struct Yes;

/// Marks a trait [`Adapt`] answers with [`io::ErrorKind::Unsupported`].
struct No;

/// [`Adapt`]'s markers, held without owning them, so they don't affect its auto traits.
type Markers<R, W, S, F> = fn() -> (R, W, S, F);

/// Adapts a stream with some of the futures-io traits and [`AsyncFork`] to [`ByteStream`].
/// `R`, `W`, `S` and `F` say whether it reads, writes, seeks and forks.
struct Adapt<T, R, W, S, F = No> {
    io: T,
    marker: PhantomData<Markers<R, W, S, F>>,
}

impl<T, R, W, S, F> Adapt<T, R, W, S, F> {
    fn new(io: T) -> Self {
        Self {
            io,
            marker: PhantomData,
        }
    }
}

impl<T: AsyncRead + Unpin, W, S, F> AsyncRead for Adapt<T, Yes, W, S, F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl<T, W, S, F> AsyncRead for Adapt<T, No, W, S, F> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
}

impl<T: AsyncWrite + Unpin, R, S, F> AsyncWrite for Adapt<T, R, Yes, S, F> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_close(cx)
    }
}

impl<T, R, S, F> AsyncWrite for Adapt<T, R, No, S, F> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncSeek + Unpin, R, W, F> AsyncSeek for Adapt<T, R, W, Yes, F> {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.get_mut().io).poll_seek(cx, pos)
    }
}

impl<T, R, W, F> AsyncSeek for Adapt<T, R, W, No, F> {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
}

impl<T: AsyncFork + Unpin, R, W, S> AsyncFork for Adapt<T, R, W, S, Yes> {
    fn poll_fork(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        offset: u64,
    ) -> Poll<io::Result<ByteHandle>> {
        Pin::new(&mut self.get_mut().io).poll_fork(cx, offset)
    }
}

impl<T, R, W, S> AsyncFork for Adapt<T, R, W, S, No> {
    fn poll_fork(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _offset: u64,
    ) -> Poll<io::Result<ByteHandle>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
}

impl<T, R, W, S, F> ByteStream for Adapt<T, R, W, S, F> where Self: ReadWriteSeek + AsyncFork {}

/// An owned, erased byte I/O handle.
///
/// The `into_*` conversions narrow it to the traits a consumer needs. A mismatch hands the
/// handle back.
///
/// As an [`Input`](crate::value::Input), a handle becomes a Do `io.Io` that owns it: its
/// methods read, write and seek the handle, and its `close` closes it.
pub struct ByteHandle {
    io: Box<dyn ByteStream>,
    caps: IoCaps,
    prefix: Vec<u8>,
}

impl ByteHandle {
    fn new(io: impl ByteStream, caps: IoCaps) -> Self {
        Self {
            io: Box::new(io),
            caps,
            prefix: Vec::new(),
        }
    }

    /// Creates a reading handle.
    pub fn read(io: impl Read) -> Self {
        Self::new(Adapt::<_, Yes, No, No>::new(io), IoCaps::READ)
    }

    /// Creates a writing handle.
    pub fn write(io: impl Write) -> Self {
        Self::new(Adapt::<_, No, Yes, No>::new(io), IoCaps::WRITE)
    }

    /// Creates a seekable reading handle.
    pub fn read_seek(io: impl ReadSeek) -> Self {
        Self::new(
            Adapt::<_, Yes, No, Yes>::new(io),
            IoCaps::READ | IoCaps::SEEK,
        )
    }

    /// Creates a seekable writing handle.
    pub fn write_seek(io: impl WriteSeek) -> Self {
        Self::new(
            Adapt::<_, No, Yes, Yes>::new(io),
            IoCaps::WRITE | IoCaps::SEEK,
        )
    }

    /// Creates a reading and writing handle.
    pub fn read_write(io: impl ReadWrite) -> Self {
        Self::new(
            Adapt::<_, Yes, Yes, No>::new(io),
            IoCaps::READ | IoCaps::WRITE,
        )
    }

    /// Creates a seekable reading and writing handle.
    pub fn read_write_seek(io: impl ReadWriteSeek) -> Self {
        Self::new(
            Adapt::<_, Yes, Yes, Yes>::new(io),
            IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK,
        )
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

    /// Creates a handle with `caps` over `io`, which implements every trait and forks, as
    /// [`with_caps`](Self::with_caps) does, and adds [`IoCaps::FORK`].
    ///
    /// # Panics
    ///
    /// Panics if `caps` neither reads nor writes, or does not seek.
    pub fn forking(io: impl ReadWriteSeek + AsyncFork, caps: IoCaps) -> Self {
        assert!(
            caps.contains(IoCaps::SEEK),
            "fork of a handle that does not seek"
        );
        let read = caps.contains(IoCaps::READ);
        let write = caps.contains(IoCaps::WRITE);
        let io: Box<dyn ByteStream> = match (read, write) {
            (true, false) => Box::new(Adapt::<_, Yes, No, Yes, Yes>::new(io)),
            (false, true) => Box::new(Adapt::<_, No, Yes, Yes, Yes>::new(io)),
            (true, true) => Box::new(Adapt::<_, Yes, Yes, Yes, Yes>::new(io)),
            (false, false) => panic!("handle neither reads nor writes"),
        };
        Self {
            io,
            caps: caps & (IoCaps::READ | IoCaps::WRITE) | IoCaps::SEEK | IoCaps::FORK,
            prefix: Vec::new(),
        }
    }

    /// The handle's capabilities.
    pub fn caps(&self) -> IoCaps {
        self.caps
    }

    /// Creates a [fork](AsyncFork) whose position starts at the absolute `offset`. Without
    /// [`IoCaps::FORK`] it fails with [`io::ErrorKind::Unsupported`].
    pub async fn fork(&mut self, offset: u64) -> io::Result<ByteHandle> {
        std::future::poll_fn(|cx| Pin::new(&mut self.io).poll_fork(cx, offset)).await
    }

    /// The stream with its prefix, and the capabilities without forking, for a holder that
    /// keeps them beside it.
    pub(crate) fn into_prefixed(self) -> (Prefixed, IoCaps) {
        let caps = self.caps - IoCaps::FORK;
        (Prefixed::new(self.io, caps, self.prefix), caps)
    }

    /// The stream, its capabilities without forking, and its prefix.
    pub(crate) fn into_parts(self) -> (Box<dyn ByteStream>, IoCaps, Vec<u8>) {
        (self.io, self.caps - IoCaps::FORK, self.prefix)
    }

    /// The stream, serving the prefix first, if the handle has `need`. A handle that cannot
    /// seek drops its prefix unless `keep_prefix`.
    fn narrow(mut self, need: IoCaps, keep_prefix: bool) -> Result<Box<dyn ByteStream>, Self> {
        if !self.caps.contains(need) {
            return Err(self);
        }
        if !keep_prefix && !self.caps.contains(IoCaps::SEEK) {
            self.prefix = Vec::new();
        }
        Ok(if self.prefix.is_empty() {
            self.io
        } else {
            Box::new(self.into_prefixed().0)
        })
    }

    /// Converts to a reader.
    pub fn into_read(self) -> Result<Box<dyn Read>, Self> {
        self.narrow(IoCaps::READ, true)
            .map(|io| io as Box<dyn Read>)
    }

    /// Converts to a writer.
    ///
    /// A non-seekable handle discards its prefix.
    pub fn into_write(self) -> Result<Box<dyn Write>, Self> {
        self.narrow(IoCaps::WRITE, false)
            .map(|io| io as Box<dyn Write>)
    }

    /// Converts to a seekable reader.
    pub fn into_read_seek(self) -> Result<Box<dyn ReadSeek>, Self> {
        self.narrow(IoCaps::READ | IoCaps::SEEK, true)
            .map(|io| io as Box<dyn ReadSeek>)
    }

    /// Converts to a seekable writer.
    pub fn into_write_seek(self) -> Result<Box<dyn WriteSeek>, Self> {
        self.narrow(IoCaps::WRITE | IoCaps::SEEK, true)
            .map(|io| io as Box<dyn WriteSeek>)
    }

    /// Converts to a reader and writer.
    pub fn into_read_write(self) -> Result<Box<dyn ReadWrite>, Self> {
        self.narrow(IoCaps::READ | IoCaps::WRITE, true)
            .map(|io| io as Box<dyn ReadWrite>)
    }

    /// Converts to a seekable reader and writer.
    pub fn into_read_write_seek(self) -> Result<Box<dyn ReadWriteSeek>, Self> {
        self.narrow(IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK, true)
            .map(|io| io as Box<dyn ReadWriteSeek>)
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

/// Serves an unread prefix before a stream.
///
/// On a seekable stream, writes and seeks first move the stream back over the unread prefix
/// and drop it, so they land at the logical position. On one that cannot seek, the prefix
/// belongs to the read side and writes pass through.
pub(crate) struct Prefixed {
    prefix: Vec<u8>,
    pos: usize,
    caps: IoCaps,
    inner: Box<dyn ByteStream>,
}

impl Prefixed {
    pub(crate) fn new(inner: Box<dyn ByteStream>, caps: IoCaps, prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            pos: 0,
            caps,
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

    /// Moves a seekable stream back over the unread prefix and drops it.
    fn poll_realign(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.caps.contains(IoCaps::SEEK) {
            return Poll::Ready(Ok(()));
        }
        if self.unread() > 0 {
            let back = -i64::try_from(self.unread()).map_err(io::Error::other)?;
            ready!(Pin::new(&mut self.inner).poll_seek(cx, SeekFrom::Current(back)))?;
        }
        self.drop_prefix();
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Prefixed {
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

impl AsyncSeek for Prefixed {
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

impl AsyncWrite for Prefixed {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_realign(cx))?;
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_realign(cx))?;
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// A fork starts at an absolute offset, so the prefix doesn't matter.
impl AsyncFork for Prefixed {
    fn poll_fork(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        offset: u64,
    ) -> Poll<io::Result<ByteHandle>> {
        Pin::new(&mut self.get_mut().inner).poll_fork(cx, offset)
    }
}

impl ByteStream for Prefixed {}
