//! The `io` module, `io.Buffered`, a buffer over a [`ByteHandle`], [`Stream`], the
//! anonymous `io.Io` a handle becomes as an [`Input`], `io.Concat`, an iterable read as
//! bytes, [`Cursor`], a stream over in-memory data, and [`Window`], a window of a seekable
//! stream.
//!
//! A seekable handle that reads and writes holds read-ahead or pending writes, never both:
//! a write first moves the handle back over unread bytes, and a read or seek first flushes.
//! A handle that cannot seek keeps the two buffers apart, since its reads and writes are
//! separate streams. Reads and writes at least a buffer long skip the buffer once it is
//! drained or flushed.
//!
//! The buffer sits in a [`Shared`] with its handle, so the handle `Buffered` lends reads
//! and writes through the same buffer.

use std::{
    io::{self, IoSlice, SeekFrom},
    mem::{self, MaybeUninit},
    ops::Deref,
    pin::Pin,
    str,
    task::{Context, Poll, ready},
};

use dolang_util::{shared::Locked, utf8};
use futures::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite,
    AsyncWriteExt,
};

use crate::{
    BYTE_STREAM_CHUNK_SIZE,
    arg::{Arg, Args},
    error::{Error, ErrorKind, Result},
    gc::with_pin,
    io::{
        ByteHandle, ByteIo, ByteStream, IntoByteIo, IoCaps, IoErrorExt as _, IoResultExt as _,
        Prefixed, Shared,
    },
    object::{
        interface::Request,
        native::{Instance, Mut, Object, Type, TypeBuilder},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        BinEmbryo, Input, Output, Slot, TypeObject, Value,
        private::Sealed,
        view::{PinBin, PinStr},
    },
    vm::{Alloc, Builder, StateTag},
    with_instance, with_slots,
};

/// Buffer capacity, and the size from which reads and writes skip the buffer.
const CAPACITY: usize = 8 * 1024;

/// A buffer over an owned byte handle.
struct BufIo {
    io: Box<dyn ByteStream>,
    caps: IoCaps,
    /// Read-ahead, unread from `rpos`.
    rbuf: Vec<u8>,
    rpos: usize,
    /// Pending writes.
    wbuf: Vec<u8>,
}

impl BufIo {
    fn unread(&self) -> usize {
        self.rbuf.len() - self.rpos
    }

    fn seekable(&self) -> bool {
        self.caps.contains(IoCaps::SEEK)
    }

    fn consume(&mut self, n: usize) {
        self.rpos += n;
        if self.rpos == self.rbuf.len() {
            self.drop_read_ahead();
        }
    }

    fn drop_read_ahead(&mut self) {
        self.rbuf.clear();
        self.rpos = 0;
    }

    /// Returns bytes taken from the handle to the front of the read-ahead.
    fn push_back(&mut self, bytes: &[u8]) {
        if bytes.len() <= self.rpos {
            self.rpos -= bytes.len();
            self.rbuf[self.rpos..self.rpos + bytes.len()].copy_from_slice(bytes);
        } else {
            let mut rbuf = Vec::with_capacity(bytes.len() + self.unread());
            rbuf.extend_from_slice(bytes);
            rbuf.extend_from_slice(&self.rbuf[self.rpos..]);
            self.rbuf = rbuf;
            self.rpos = 0;
        }
    }

    /// Writes out pending writes.
    ///
    /// Reads call this first. On a seekable handle they must, so the read starts after the
    /// writes. On one that cannot seek, a peer may be waiting for them before it answers.
    fn poll_flush_buf(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.wbuf.is_empty() {
            let n = ready!(Pin::new(&mut self.io).poll_write(cx, &self.wbuf))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.wbuf.drain(..n);
        }
        Poll::Ready(Ok(()))
    }

    /// Prepares to write, moving a seekable handle back over unread bytes.
    fn poll_begin_write(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.seekable() {
            return Poll::Ready(Ok(()));
        }
        if self.unread() > 0 {
            let back = -i64::try_from(self.unread()).map_err(io::Error::other)?;
            ready!(Pin::new(&mut self.io).poll_seek(cx, SeekFrom::Current(back)))?;
        }
        self.drop_read_ahead();
        Poll::Ready(Ok(()))
    }

    /// Appends `n` bytes to `out`, or the rest of the stream when `n` is `None`, stopping
    /// short only at the end of the stream.
    async fn read_into<'v>(
        &mut self,
        alloc: &mut dyn Alloc<'v>,
        out: &mut BinEmbryo<'v>,
        n: Option<usize>,
    ) -> io::Result<()> {
        let start = out.len();
        loop {
            let want = match n {
                Some(n) => n - (out.len() - start),
                None => usize::MAX,
            };
            if want == 0 {
                return Ok(());
            }
            if self.unread() == 0 && want >= CAPACITY {
                let chunk = want.min(BYTE_STREAM_CHUNK_SIZE);
                if read_spare(self, alloc, out, chunk).await? == 0 {
                    return Ok(());
                }
            } else {
                let avail = self.fill_buf().await?;
                if avail.is_empty() {
                    return Ok(());
                }
                let take = avail.len().min(want);
                out.extend(alloc, &avail[..take]);
                self.consume(take);
            }
        }
    }

    /// Appends one line to `out`, terminator included. Nothing is appended at the end of
    /// the stream.
    async fn read_line_into<'v>(
        &mut self,
        alloc: &mut dyn Alloc<'v>,
        out: &mut BinEmbryo<'v>,
    ) -> io::Result<()> {
        loop {
            let avail = self.fill_buf().await?;
            if avail.is_empty() {
                return Ok(());
            }
            let (take, done) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (avail.len(), false),
            };
            out.extend(alloc, &avail[..take]);
            self.consume(take);
            if done {
                return Ok(());
            }
        }
    }
}

/// Reads once from `reader` into at most `n` bytes appended to `out`, returning the count.
async fn read_spare<'v>(
    reader: &mut (impl AsyncRead + Unpin),
    alloc: &mut dyn Alloc<'v>,
    out: &mut BinEmbryo<'v>,
    n: usize,
) -> io::Result<usize> {
    out.reserve(alloc, n);
    let spare = &mut out.spare_capacity_mut()[..n];
    spare.fill(MaybeUninit::new(0));
    // SAFETY: every byte of `spare` was just initialized.
    let spare = unsafe { &mut *(spare as *mut [MaybeUninit<u8>] as *mut [u8]) };
    let read = reader.read(spare).await?;
    // SAFETY: `read` bytes at the front of the spare capacity are initialized.
    unsafe { out.advance(read) };
    Ok(read)
}

impl AsyncRead for BufIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.unread() == 0 && buf.len() >= CAPACITY {
            ready!(this.poll_flush_buf(cx))?;
            return Pin::new(&mut this.io).poll_read(cx, buf);
        }
        let avail = ready!(Pin::new(&mut *this).poll_fill_buf(cx))?;
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        this.consume(n);
        Poll::Ready(Ok(n))
    }
}

/// Refills the read-ahead only once it is drained. Empty means end of stream.
impl AsyncBufRead for BufIo {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.unread() == 0 {
            ready!(this.poll_flush_buf(cx))?;
            this.rbuf.resize(CAPACITY, 0);
            match Pin::new(&mut this.io).poll_read(cx, &mut this.rbuf) {
                Poll::Ready(Ok(n)) => this.rbuf.truncate(n),
                res => {
                    this.rbuf.clear();
                    return res.map_ok(|_| &[][..]);
                }
            }
        }
        Poll::Ready(Ok(&this.rbuf[this.rpos..]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.get_mut().consume(amt);
    }
}

impl AsyncWrite for BufIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_begin_write(cx))?;
        if this.wbuf.len() + buf.len() > CAPACITY {
            ready!(this.poll_flush_buf(cx))?;
        }
        if buf.len() >= CAPACITY {
            return Pin::new(&mut this.io).poll_write(cx, buf);
        }
        this.wbuf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    /// Buffers every piece, so a stream that writes them at once gets them together.
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_begin_write(cx))?;
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if this.wbuf.len() + len > CAPACITY {
            ready!(this.poll_flush_buf(cx))?;
        }
        if len >= CAPACITY {
            return Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        }
        for buf in bufs {
            this.wbuf.extend_from_slice(buf);
        }
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_buf(cx))?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_buf(cx))?;
        Pin::new(&mut this.io).poll_close(cx)
    }
}

/// `SeekFrom::Current(0)` reports the logical position, keeping both buffers: the
/// handle's position, less unread bytes, plus pending writes.
impl AsyncSeek for BufIo {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        if pos == SeekFrom::Current(0) {
            let pos = ready!(Pin::new(&mut this.io).poll_seek(cx, pos))?;
            let pos = (pos + this.wbuf.len() as u64)
                .checked_sub(this.unread() as u64)
                .ok_or_else(|| io::Error::other("position is before buffered data"));
            return Poll::Ready(pos);
        }
        ready!(this.poll_flush_buf(cx))?;
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
        let pos = ready!(Pin::new(&mut this.io).poll_seek(cx, pos))?;
        this.drop_read_ahead();
        Poll::Ready(Ok(pos))
    }
}

/// A buffer over an owned byte handle, shared with the handles it lends.
pub(crate) struct Buffered {
    /// The buffer, until it is closed.
    io: Option<Shared<BufIo>>,
    caps: IoCaps,
}

impl Buffered {
    /// Wraps `handle`, serving its prefix first.
    fn new(handle: ByteHandle) -> Self {
        let (io, caps, prefix) = handle.into_parts();
        Self {
            caps,
            io: Some(Shared::new(BufIo {
                io,
                caps,
                rbuf: prefix,
                rpos: 0,
                wbuf: Vec::new(),
            })),
        }
    }

    /// Raises unless the buffer is open and its handle has `need`.
    fn check<'v, 's>(&self, strand: &mut Strand<'v, 's>, need: IoCaps) -> Result<'v, 's, ()> {
        check_caps(strand, "buffer", self.io.is_some(), self.caps, need)
    }

    /// Capabilities while open; a closed buffer has none.
    fn open_caps(&self) -> IoCaps {
        open_caps(&self.io, self.caps)
    }

    /// Holds the buffer for a method, after any operation through a lent handle.
    async fn lock<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Locked<'_, BufIo>> {
        let io = self.io.as_mut().expect("buffer is closed");
        io.lock().await.into_do_io(strand)
    }

    /// Flushes and closes the buffer, dropping it even if flushing fails.
    ///
    /// The handle's own `close` only flushes a lent handle, so the source stays open.
    async fn close(&mut self) -> io::Result<()> {
        close_shared(&mut self.io).await
    }
}

/// Raises unless the stream, called `noun` in messages, is open and has `need`.
fn check_caps<'v, 's>(
    strand: &mut Strand<'v, 's>,
    noun: &str,
    open: bool,
    caps: IoCaps,
    need: IoCaps,
) -> Result<'v, 's, ()> {
    let missing = need - caps;
    let problem = if !open {
        "closed"
    } else if missing.contains(IoCaps::READ) {
        "not readable"
    } else if missing.contains(IoCaps::WRITE) {
        "not writable"
    } else if missing.contains(IoCaps::SEEK) {
        "not seekable"
    } else {
        return Ok(());
    };
    Err(Error::state_error(strand, format!("{noun} is {problem}")))
}

/// Capabilities while open; a closed stream has none.
fn open_caps<T>(io: &Option<Shared<T>>, caps: IoCaps) -> IoCaps {
    if io.is_some() { caps } else { IoCaps::empty() }
}

/// Closes a shared stream for every handle, then closes the stream itself. Closing a closed
/// stream does nothing.
async fn close_shared<T: AsyncWrite + Unpin>(io: &mut Option<Shared<T>>) -> io::Result<()> {
    let Some(mut shared) = io.take() else {
        return Ok(());
    };
    match shared.close().await {
        Some(mut io) => io.close().await,
        None => Ok(()),
    }
}

impl<'v> IntoByteIo<'v> for Buffered {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(self.caps)
    }

    /// Lends the buffer itself, so the consumer reads and writes through it.
    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        let io = self.io.as_ref().expect("buffer is closed").clone();
        Ok(ByteHandle::with_caps(io, self.caps))
    }
}

fn invalid_utf8<'v, 's>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::data(strand, "invalid UTF-8 data")
}

/// Parses a byte count.
fn size_value<'v, 's>(strand: &mut Strand<'v, 's>, size: Slot<'v, '_>) -> Result<'v, 's, usize> {
    size.to_i64(strand)
        .ok()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::type_error(strand, "size must be a non-negative integer"))
}

/// Parses an optional byte count.
fn size_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    size: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<usize>> {
    size.map(|size| size_value(strand, size)).transpose()
}

/// A `Str` or `Bin` a write takes its bytes from, pinned for the write.
pub(super) enum Piece<'v> {
    Str(PinStr<'v>),
    Bin(PinBin<'v>),
}

impl Deref for Piece<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Piece::Str(s) => s.as_bytes(),
            Piece::Bin(b) => b,
        }
    }
}

/// Pins the arguments of a `write`: any number of `Str` or `Bin` pieces.
fn pieces<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
) -> Result<'v, 's, Vec<Piece<'v>>> {
    let mut pieces = Vec::with_capacity(args.len());
    for arg in args {
        let value: &'a Value<'v> = match arg {
            Arg::Pos(value) => value.into_ref(),
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };
        let piece = if let Some(b) = value.as_bin(strand.vm()) {
            Piece::Bin(b.pin())
        } else if let Some(s) = value.as_str(strand.vm()) {
            Piece::Str(s.pin())
        } else {
            return Err(Error::type_error(strand, "expected `Str` or `Bin`"));
        };
        pieces.push(piece);
    }
    Ok(pieces)
}

/// Slices over pinned pieces, and their total length.
fn slices<'a>(pieces: &'a [Piece<'_>]) -> (Vec<IoSlice<'a>>, usize) {
    let slices = pieces
        .iter()
        .map(|piece| IoSlice::new(piece))
        .collect::<Vec<_>>();
    let len = pieces.iter().map(|piece| piece.len()).sum();
    (slices, len)
}

/// Parses `seek`'s arguments: exactly one of a relative `offset`, `start:` or `end:`.
fn seek_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    offset: Option<Slot<'v, '_>>,
    start: Option<Slot<'v, '_>>,
    end: Option<Slot<'v, '_>>,
    start_sym: Sym<'v, '_>,
    end_sym: Sym<'v, '_>,
) -> Result<'v, 's, SeekFrom> {
    Ok(match (offset, start, end) {
        (Some(offset), None, None) => SeekFrom::Current(
            offset
                .to_i64(strand)
                .map_err(|_| Error::type_error(strand, "seek offset must be an integer"))?,
        ),
        (None, Some(start), None) => {
            let start = start
                .to_i64(strand)
                .map_err(|_| Error::type_error(strand, "start must be an integer"))?;
            SeekFrom::Start(
                u64::try_from(start)
                    .map_err(|_| Error::value(strand, "start offset must be non-negative"))?,
            )
        }
        (None, None, Some(end)) => SeekFrom::End(
            end.to_i64(strand)
                .map_err(|_| Error::type_error(strand, "end must be an integer"))?,
        ),
        (None, None, None) => return Err(Error::missing_positional(strand, 0)),
        (Some(_), Some(_), _) => return Err(Error::unexpected_key(strand, start_sym)),
        (Some(_), None, Some(_)) | (None, Some(_), Some(_)) => {
            return Err(Error::unexpected_key(strand, end_sym));
        }
    })
}

impl Buffered {
    /// Reads a line as a `Str`, or `""` at the end of the stream.
    async fn read_line<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.check(strand, IoCaps::READ)?;
        let mut io = self.lock(strand).await?;
        let mut buf = BinEmbryo::new();
        if let Err(err) = io.read_line_into(strand, &mut buf).await {
            io.push_back(buf.as_slice());
            return Err(err.into_do_io(strand));
        }
        if str::from_utf8(buf.as_slice()).is_err() {
            io.push_back(buf.as_slice());
            return Err(invalid_utf8(strand));
        }
        // SAFETY: validated above.
        unsafe { buf.finish_str_unchecked(strand, out) };
        Ok(())
    }

    /// Reads up to `size` bytes, or the rest of the stream, as a `Str`.
    ///
    /// A character split by `size` is left for the next read. When `size` is too small for
    /// the first character, the whole character is read.
    async fn read_str<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        size: Option<usize>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.check(strand, IoCaps::READ)?;
        let mut io = self.lock(strand).await?;
        let mut buf = BinEmbryo::new();
        if let Err(err) = io.read_into(strand, &mut buf, size).await {
            io.push_back(buf.as_slice());
            return Err(err.into_do_io(strand));
        }
        loop {
            let eof = size.is_none_or(|size| buf.len() < size);
            let valid = match utf8::complete_prefix(buf.as_slice()) {
                Ok(text) if text.len() == buf.len() => break,
                Ok(text) if !eof => text.len(),
                _ => {
                    io.push_back(buf.as_slice());
                    return Err(invalid_utf8(strand));
                }
            };
            if valid > 0 {
                io.push_back(&buf.as_slice()[valid..]);
                buf.truncate(valid);
                break;
            }
            // The first character is incomplete, so read on until it is whole.
            let before = buf.len();
            if let Err(err) = io.read_into(strand, &mut buf, Some(1)).await {
                io.push_back(buf.as_slice());
                return Err(err.into_do_io(strand));
            }
            if buf.len() == before {
                io.push_back(buf.as_slice());
                return Err(invalid_utf8(strand));
            }
        }
        // SAFETY: validated above.
        unsafe { buf.finish_str_unchecked(strand, out) };
        Ok(())
    }
}

/// Registers the `io` module.
pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    // Eager: converting a `ByteHandle` needs `Stream` with only a `Vm`, which can't force
    let stream = builder.register_type::<Stream>();
    builder.register_state::<StreamType>(stream);
    builder.lazy::<ConcatType>(&["io"], |reg| {
        let buffered = reg.register_type::<Buffered>();
        let concat = reg.register_type::<Concat>();
        reg.register_state::<ConcatType>(concat);
        let cursor = reg.register_type::<Cursor>();
        let window = reg.register_type::<Window>();
        let io = reg.singletons().io.dup();
        reg.module("io")
            .value("Io", &io)
            .value("Buffered", buffered)
            .value("Concat", concat)
            .value("Cursor", cursor)
            .value("Window", window)
            .commit();
    });
}

/// Writes `value` as `put` does: a `Str` or `Bin` as its bytes, anything else as its
/// string form.
async fn put_value<'v, 's>(
    this: &mut Buffered,
    strand: &mut Strand<'v, 's>,
    value: &Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut io = this.lock(strand).await?;
    let res = if let Some(b) = value.as_bin(strand.vm()) {
        let b = b.pin();
        io.write_all(&b).await
    } else if let Some(s) = value.as_str(strand.vm()) {
        let s = s.pin();
        io.write_all(s.as_bytes()).await
    } else {
        let s = value.to_string(strand)?;
        io.write_all(s.as_bytes()).await
    };
    res.into_do_io(strand)
}

impl<'v> Object<'v> for Buffered {
    const NAME: &'v str = "Buffered";
    const MODULE: &'v str = "io";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([source], []) = unpack!(strand, args, 1, 0)?;
        let handle = crate::io::get(strand, &source, |_, _| Ok(())).map_err(|err| {
            if err.kind() == ErrorKind::Unsupported {
                Error::type_error(strand, "expected an `io.Io` value for `source`")
            } else {
                err
            }
        })?;
        this.create(strand, Buffered::new(handle), out);
        Ok(())
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        borrow.read_line(strand, Slot::reborrow(&mut out)).await?;
        Ok(out.as_str(strand).is_some_and(|line| !line.is_empty()))
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        borrow.check(strand, IoCaps::WRITE)?;
        put_value(&mut borrow, strand, &value).await
    }

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            borrow.check(request, IoCaps::empty())?;
            request.provide::<ByteIo>(&mut *borrow)?;
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let start_sym = builder.sym("start");
        let end_sym = builder.sym("end");
        let builder = builder
            .supertype(TypeObject::Io)
            .supertype(TypeObject::Iter);
        builder
            .get("readable", |this, strand, out| {
                let caps = this.borrow(strand)?.open_caps();
                Output::set(strand, out, caps.contains(IoCaps::READ));
                Ok(())
            })
            .get("writable", |this, strand, out| {
                let caps = this.borrow(strand)?.open_caps();
                Output::set(strand, out, caps.contains(IoCaps::WRITE));
                Ok(())
            })
            .get("seekable", |this, strand, out| {
                let caps = this.borrow(strand)?.open_caps();
                Output::set(strand, out, caps.contains(IoCaps::SEEK));
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size = size_arg(strand, size)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::READ)?;
                let mut io = borrow.lock(strand).await?;
                let mut buf = BinEmbryo::new();
                if let Err(err) = io.read_into(strand, &mut buf, size).await {
                    io.push_back(buf.as_slice());
                    return Err(err.into_do_io(strand));
                }
                buf.finish(strand, out);
                Ok(())
            })
            .method("read_str", async move |this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size = size_arg(strand, size)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.read_str(strand, size, out).await
            })
            .method("read_line", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.read_line(strand, out).await
            })
            .method("lines", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Output::set(strand, out, this);
                Ok(())
            })
            .method("write", async move |this, strand, args, out| {
                let pieces = pieces(strand, args)?;
                let (mut slices, len) = slices(&pieces);
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::WRITE)?;
                let mut io = borrow.lock(strand).await?;
                let res = io.write_all_vectored(&mut slices).await;
                res.into_do_io(strand)?;
                Output::set(strand, out, len);
                Ok(())
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::empty())?;
                let res = borrow.lock(strand).await?.flush().await;
                res.into_do_io(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let res = borrow.close().await;
                res.into_do_io(strand)
            })
            .method("tell", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let res = borrow.lock(strand).await?.stream_position().await;
                let pos = res.into_do_io(strand)?;
                Output::set(strand, out, pos);
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let pos = seek_arg(strand, offset, start, end, start_sym, end_sym)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let res = borrow.lock(strand).await?.seek(pos).await;
                let pos = res.into_do_io(strand)?;
                Output::set(strand, out, pos);
                Ok(())
            })
    }
}

/// An owned byte handle as a Do `io.Io`, without a buffer.
///
/// Each `read` polls the handle once, after serving its prefix. Unlike [`Buffered`], it owns
/// its stream, so `close` closes the handle.
pub(crate) struct Stream {
    /// The handle, until it is closed.
    io: Option<Shared<Prefixed>>,
    caps: IoCaps,
}

/// The registered [`Stream`] type, for the [`Input`] impl of [`ByteHandle`].
struct StreamType;

impl StateTag for StreamType {
    type State<'v> = Type<'v, Stream>;
}

impl Stream {
    fn new(handle: ByteHandle) -> Self {
        let (io, caps) = handle.into_prefixed();
        Self {
            caps,
            io: Some(Shared::new(io)),
        }
    }

    /// Raises unless the stream is open and has `need`.
    fn check<'v, 's>(&self, strand: &mut Strand<'v, 's>, need: IoCaps) -> Result<'v, 's, ()> {
        check_caps(strand, "stream", self.io.is_some(), self.caps, need)
    }

    /// Holds the handle for a method, after any operation through a lent handle.
    async fn lock<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Locked<'_, Prefixed>> {
        let io = self.io.as_mut().expect("stream is closed");
        io.lock().await.into_do_io(strand)
    }
}

impl<'v> IntoByteIo<'v> for Stream {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(self.caps)
    }

    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        let io = self.io.as_ref().expect("stream is closed").clone();
        Ok(ByteHandle::with_caps(io, self.caps))
    }
}

/// Wraps the handle in a [`Stream`] that owns it.
impl<'v> Input<'v> for ByteHandle {
    fn input_take<'b>(
        &'b mut self,
        mut alloc: &mut dyn Alloc<'v>,
        out: Slot<'v, '_>,
        _: Sealed,
    ) -> Option<Slot<'v, 'b>> {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        let handle = mem::replace(self, ByteHandle::read(futures::io::empty()));
        let ty = *vm.state::<StreamType>();
        ty.create(&mut alloc, Stream::new(handle), out);
        None
    }
}

impl<'v> Object<'v> for Stream {
    const NAME: &'v str = "Stream";
    const MODULE: &'v str = "io";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            borrow.check(request, IoCaps::empty())?;
            request.provide::<ByteIo>(&mut *borrow)?;
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let start_sym = builder.sym("start");
        let end_sym = builder.sym("end");
        builder
            .supertype(TypeObject::Io)
            .get("readable", |this, strand, out| {
                let this = this.borrow(strand)?;
                let caps = open_caps(&this.io, this.caps);
                Output::set(strand, out, caps.contains(IoCaps::READ));
                Ok(())
            })
            .get("writable", |this, strand, out| {
                let this = this.borrow(strand)?;
                let caps = open_caps(&this.io, this.caps);
                Output::set(strand, out, caps.contains(IoCaps::WRITE));
                Ok(())
            })
            .get("seekable", |this, strand, out| {
                let this = this.borrow(strand)?;
                let caps = open_caps(&this.io, this.caps);
                Output::set(strand, out, caps.contains(IoCaps::SEEK));
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size_value(strand, size)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::READ)?;
                let mut io = borrow.lock(strand).await?;
                let mut buf = BinEmbryo::new();
                let chunk = size.min(BYTE_STREAM_CHUNK_SIZE);
                read_spare(&mut *io, strand, &mut buf, chunk)
                    .await
                    .into_do_io(strand)?;
                buf.finish(strand, out);
                Ok(())
            })
            .method("write", async move |this, strand, args, out| {
                let pieces = pieces(strand, args)?;
                let (slices, _) = slices(&pieces);
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::WRITE)?;
                let mut io = borrow.lock(strand).await?;
                let res = io.write_vectored(&slices).await;
                let written = res.into_do_io(strand)?;
                Output::set(strand, out, written);
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let pos = seek_arg(strand, offset, start, end, start_sym, end_sym)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let res = borrow.lock(strand).await?.seek(pos).await;
                let pos = res.into_do_io(strand)?;
                Output::set(strand, out, pos);
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let res = close_shared(&mut borrow.io).await;
                res.into_do_io(strand)
            })
    }
}

/// A Do iterable read as the concatenation of its items' bytes, each followed by `end`.
///
/// Slot 0 holds the iterable until the first read, then its iterator until it ends. Items
/// are pulled only as reads need them. `Concat` answers no [`ByteIo`] itself: `Io` serves
/// it through its own `read`, on the strands of the handle's owner.
pub(crate) struct Concat {
    end: Vec<u8>,
    /// Bytes converted but not yet read.
    pending: Vec<u8>,
    state: ConcatState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConcatState {
    /// Slot 0 holds the iterable.
    Ready,
    /// Slot 0 holds the iterator.
    Iterating,
    Done,
    Closed,
}

/// The registered [`Concat`] type, for [`crate::io::concat`].
pub(crate) struct ConcatType;

impl StateTag for ConcatType {
    type State<'v> = Type<'v, Concat>;
}

impl Concat {
    pub(crate) fn new(end: Vec<u8>) -> Self {
        Self {
            end,
            pending: Vec::new(),
            state: ConcatState::Ready,
        }
    }

    /// Pulls items until `size` bytes are pending or the iterator ends.
    async fn fill<'v, 's>(
        this: &mut Mut<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
        size: usize,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut item| {
            if this.state == ConcatState::Ready {
                Mut::slot::<0>(this)
                    .iter(strand, Slot::reborrow(&mut item))
                    .await?;
                Output::set(strand, Mut::slot_mut::<0>(this), &item);
                this.state = ConcatState::Iterating;
            }
            while this.state == ConcatState::Iterating && this.pending.len() < size {
                if !Mut::slot::<0>(this)
                    .next(strand, Slot::reborrow(&mut item))
                    .await?
                {
                    this.state = ConcatState::Done;
                    Mut::slot_mut::<0>(this).store(Value::NIL);
                    break;
                }
                if let Some(b) = item.as_bin(strand.vm()) {
                    this.pending.extend_from_slice(&b.pin());
                } else if let Some(s) = item.as_str(strand.vm()) {
                    this.pending.extend_from_slice(s.pin().as_bytes());
                } else {
                    let text = item.to_string(strand)?;
                    this.pending.extend_from_slice(text.as_bytes());
                }
                let Self { end, pending, .. } = &mut **this;
                pending.extend_from_slice(end);
            }
            Ok(())
        })
    }
}

impl<'v> Object<'v> for Concat {
    const NAME: &'v str = "Concat";
    const MODULE: &'v str = "io";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let end_sym = Sym::well_known(sym::END);
        let ([items], [end]) = unpack!(strand, args, 1, 0, end_sym = None)?;
        let end = match end {
            None => Vec::new(),
            Some(end) => {
                if let Some(b) = end.as_bin(strand.vm()) {
                    b.pin().to_vec()
                } else if let Some(s) = end.as_str(strand.vm()) {
                    s.pin().as_bytes().to_vec()
                } else {
                    return Err(Error::type_error(strand, "end: expected `Str` or `Bin`"));
                }
            }
        };
        this.create(strand, Concat::new(end), &mut out);
        with_instance!(strand, this.cast(&out).unwrap(), |inst| {
            let mut borrow = inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &items);
        });
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Io)
            .get("readable", |this, strand, out| {
                let this = this.borrow(strand)?;
                Output::set(strand, out, this.state != ConcatState::Closed);
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size_value(strand, size)?;
                let mut borrow = this.borrow_mut(strand)?;
                let open = borrow.state != ConcatState::Closed;
                check_caps(strand, "stream", open, IoCaps::READ, IoCaps::READ)?;
                Concat::fill(&mut borrow, strand, size).await?;
                let n = size.min(borrow.pending.len());
                Output::set(strand, out, &borrow.pending[..n]);
                borrow.pending.drain(..n);
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.state = ConcatState::Closed;
                borrow.pending = Vec::new();
                Mut::slot_mut::<0>(&mut borrow).store(Value::NIL);
                Ok(())
            })
    }
}

/// The kind of memory a cursor reads and writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemTarget {
    Str,
    Bin,
    BinBuf,
    StrBuf,
}

/// `io.Cursor`, a seekable `io.Io` over a window of in-memory data.
///
/// Slot 0 holds the target until the cursor is closed. The target is read and written live,
/// so the cursor sees changes made to a buffer by other means. The cursor answers no
/// [`ByteIo`] for it, since GC data can't leave the VM: `Io` serves it through the
/// cursor's own methods, as it does `io.Concat`. The bytes a `StrBuf` cursor holds back
/// therefore live here, not in a handle.
pub(crate) struct Cursor {
    target: MemTarget,
    /// The window's absolute start.
    start: u64,
    /// The window's absolute end, when it is bounded.
    end: Option<u64>,
    /// Position within the window.
    pos: u64,
    /// The start of an unfinished character written to a `StrBuf`, held back until it is
    /// complete. `pos` does not count it.
    held: Vec<u8>,
    open: bool,
}

impl Cursor {
    /// Capabilities of the target, open or not.
    fn target_caps(&self) -> IoCaps {
        match self.target {
            MemTarget::Str | MemTarget::Bin => IoCaps::READ | IoCaps::SEEK,
            MemTarget::BinBuf | MemTarget::StrBuf => IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK,
        }
    }

    /// Capabilities while open; a closed cursor has none.
    fn caps(&self) -> IoCaps {
        if self.open {
            self.target_caps()
        } else {
            IoCaps::empty()
        }
    }

    /// Raises unless the cursor is open and has `need`.
    fn check<'v, 's>(&self, strand: &mut Strand<'v, 's>, need: IoCaps) -> Result<'v, 's, ()> {
        check_caps(strand, "stream", self.open, self.target_caps(), need)
    }

    /// The absolute position, which may lie past the end of the target.
    fn abs(&self) -> u64 {
        self.start.saturating_add(self.pos)
    }

    /// Bytes left before the end of a bounded window.
    fn room(&self) -> usize {
        match self.end {
            Some(end) => usize::try_from(end.saturating_sub(self.abs())).unwrap_or(usize::MAX),
            None => usize::MAX,
        }
    }

    /// Reads up to `size` bytes as a `Bin`, stopping at the end of the window or the
    /// target.
    fn read<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        target: &Value<'v>,
        size: usize,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let at = self.abs();
        let room = self.room();
        let n = with_bytes(strand, target, self.target, |strand, bytes| {
            let at = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
            let n = size.min(room).min(bytes.len() - at);
            Output::set(strand, out, &bytes[at..at + n]);
            n
        })?;
        self.pos += n as u64;
        Ok(())
    }

    /// Writes `data` at the position, cut short at the end of a bounded window, and returns
    /// how much of it was taken.
    fn write<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        target: &Value<'v>,
        data: &[u8],
    ) -> Result<'v, 's, usize> {
        let at = usize::try_from(self.abs()).map_err(|_| Error::overflow(strand))?;
        let room = self.room();
        match self.target {
            MemTarget::BinBuf => {
                let n = data.len().min(room);
                if n > 0 {
                    let buf = target
                        .downcast_native(strand, strand.builtin_types().binbuf)
                        .expect("target is a `BinBuf`");
                    with_pin!(strand, buf, |buf| {
                        let mut buf = buf.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
                        buf.overwrite(strand, at, &data[..n]);
                        Ok(())
                    })?;
                }
                self.pos += n as u64;
                Ok(n)
            }
            MemTarget::StrBuf => self.write_str(strand, target, at, room, data),
            MemTarget::Str | MemTarget::Bin => unreachable!("an immutable target is not writable"),
        }
    }

    /// Writes the complete characters of the held bytes and `data` to a `StrBuf`, and holds
    /// back an unfinished final character. Invalid UTF-8, or a write that would split a
    /// character of the contents, raises and changes nothing.
    fn write_str<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        target: &Value<'v>,
        at: usize,
        room: usize,
        data: &[u8],
    ) -> Result<'v, 's, usize> {
        let mut bytes = self.held.clone();
        bytes.extend_from_slice(data);
        bytes.truncate(room);
        let Ok(text) = utf8::complete_prefix(&bytes) else {
            return Err(Error::data(strand, "invalid UTF-8 data"));
        };
        if !text.is_empty() {
            let buf = target
                .downcast_native(strand, strand.builtin_types().strbuf)
                .expect("target is a `StrBuf`");
            with_pin!(strand, buf, |buf| {
                let mut buf = buf.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
                if !buf.overwrite(strand, at, text) {
                    return Err(Error::value(strand, "write would split a character"));
                }
                Ok(())
            })?;
        }
        let taken = bytes.len().saturating_sub(self.held.len());
        self.pos += text.len() as u64;
        self.held = bytes[text.len()..].to_vec();
        Ok(taken)
    }

    /// Moves the position, returning the new one.
    fn seek<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        target: &Value<'v>,
        pos: SeekFrom,
    ) -> Result<'v, 's, u64> {
        // Reporting the position moves nothing, so it needn't wait for a held character.
        if !self.held.is_empty() && pos != SeekFrom::Current(0) {
            return Err(Error::value(
                strand,
                "cannot seek while an unfinished character is held",
            ));
        }
        let (base, offset) = match pos {
            SeekFrom::Start(n) => (0, i128::from(n)),
            SeekFrom::Current(n) => (self.pos, i128::from(n)),
            SeekFrom::End(n) => {
                let len = match self.end {
                    Some(end) => end - self.start,
                    None => {
                        let start = self.start;
                        with_bytes(strand, target, self.target, |_, bytes| {
                            (bytes.len() as u64).saturating_sub(start)
                        })?
                    }
                };
                (len, i128::from(n))
            }
        };
        let pos = u64::try_from(i128::from(base) + offset)
            .map_err(|_| Error::value(strand, "seek position must be non-negative"))?;
        self.pos = pos;
        Ok(pos)
    }
}

/// Calls `f` with the target's current bytes.
fn with_bytes<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    kind: MemTarget,
    f: impl FnOnce(&mut Strand<'v, 's>, &[u8]) -> R,
) -> Result<'v, 's, R> {
    match kind {
        MemTarget::Str => {
            let s = target.as_str(strand.vm()).expect("target is a `Str`").pin();
            Ok(f(strand, s.as_bytes()))
        }
        MemTarget::Bin => {
            let b = target.as_bin(strand.vm()).expect("target is a `Bin`").pin();
            Ok(f(strand, &b))
        }
        MemTarget::BinBuf => {
            let buf = target
                .downcast_native(strand, strand.builtin_types().binbuf)
                .expect("target is a `BinBuf`");
            with_pin!(strand, buf, |buf| {
                let buf = buf.borrow().ok_or_else(|| Error::concurrency(strand))?;
                Ok(f(strand, buf.as_slice()))
            })
        }
        MemTarget::StrBuf => {
            let buf = target
                .downcast_native(strand, strand.builtin_types().strbuf)
                .expect("target is a `StrBuf`");
            with_pin!(strand, buf, |buf| {
                let buf = buf.borrow().ok_or_else(|| Error::concurrency(strand))?;
                Ok(f(strand, buf.as_str().as_bytes()))
            })
        }
    }
}

/// Parses a window: a `Range` of non-negative `Int`s with step 1, or everything.
fn range_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    range: Option<Slot<'v, '_>>,
) -> Result<'v, 's, (u64, Option<u64>)> {
    let Some(range) = range.filter(|range| !range.is_nil()) else {
        return Ok((0, None));
    };
    let Some(range) = range.downcast_ref(strand.builtin_types().range) else {
        return Err(Error::type_error(strand, "expected `Range` for `range`"));
    };
    with_pin!(strand, range, |range| {
        let (start, end, step) = range.get().parts();
        if !step.is_nil() && step.to_i64(strand).ok() != Some(1) {
            return Err(Error::value(strand, "range step must be 1"));
        }
        let start = bound(strand, start)?.unwrap_or(0);
        let end = bound(strand, end)?;
        if end.is_some_and(|end| end < start) {
            return Err(Error::value(strand, "range must not run backward"));
        }
        Ok((start, end))
    })
}

/// Parses a range bound, `None` when it is open.
fn bound<'v, 's>(strand: &mut Strand<'v, 's>, bound: &Value<'v>) -> Result<'v, 's, Option<u64>> {
    if bound.is_nil() {
        return Ok(None);
    }
    let bound = bound
        .to_i64(strand)
        .map_err(|_| Error::value(strand, "range bounds must be integers"))?;
    u64::try_from(bound)
        .map(Some)
        .map_err(|_| Error::value(strand, "range bounds must be non-negative"))
}

impl<'v> Object<'v> for Cursor {
    const NAME: &'v str = "Cursor";
    const MODULE: &'v str = "io";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([target], [range]) = unpack!(strand, args, 1, 1)?;
        let (start, end) = range_arg(strand, range)?;
        let vm = strand.vm();
        let kind = if target.as_str(vm).is_some() {
            MemTarget::Str
        } else if target.as_bin(vm).is_some() {
            MemTarget::Bin
        } else if target
            .downcast_native(vm, vm.builtin_types().binbuf)
            .is_some()
        {
            MemTarget::BinBuf
        } else if target
            .downcast_native(vm, vm.builtin_types().strbuf)
            .is_some()
        {
            MemTarget::StrBuf
        } else {
            return Err(Error::type_error(
                strand,
                "expected `Str`, `Bin`, `BinBuf` or `StrBuf` for `target`",
            ));
        };
        let cursor = Cursor {
            target: kind,
            start,
            end,
            pos: 0,
            held: Vec::new(),
            open: true,
        };
        this.create(strand, cursor, &mut out);
        with_instance!(strand, this.cast(&out).unwrap(), |inst| {
            let mut borrow = inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &target);
        });
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let start_sym = builder.sym("start");
        let end_sym = builder.sym("end");
        builder
            .supertype(TypeObject::Io)
            .get("readable", |this, strand, out| {
                let caps = this.borrow(strand)?.caps();
                Output::set(strand, out, caps.contains(IoCaps::READ));
                Ok(())
            })
            .get("writable", |this, strand, out| {
                let caps = this.borrow(strand)?.caps();
                Output::set(strand, out, caps.contains(IoCaps::WRITE));
                Ok(())
            })
            .get("seekable", |this, strand, out| {
                let caps = this.borrow(strand)?.caps();
                Output::set(strand, out, caps.contains(IoCaps::SEEK));
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size_value(strand, size)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::READ)?;
                with_slots!(strand, |mut target| {
                    Output::set(strand, &mut target, Mut::slot::<0>(&borrow));
                    borrow.read(strand, &target, size, out)
                })
            })
            .method("write", async move |this, strand, args, out| {
                let pieces = pieces(strand, args)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::WRITE)?;
                let data = pieces
                    .iter()
                    .map(|piece| &**piece)
                    .collect::<Vec<_>>()
                    .concat();
                let n = with_slots!(strand, |mut target| {
                    Output::set(strand, &mut target, Mut::slot::<0>(&borrow));
                    borrow.write(strand, &target, &data)
                })?;
                Output::set(strand, out, n);
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let pos = seek_arg(strand, offset, start, end, start_sym, end_sym)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let pos = with_slots!(strand, |mut target| {
                    Output::set(strand, &mut target, Mut::slot::<0>(&borrow));
                    borrow.seek(strand, &target, pos)
                })?;
                Output::set(strand, out, pos);
                Ok(())
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                this.borrow(strand)?.check(strand, IoCaps::empty())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                Mut::slot_mut::<0>(&mut borrow).store(Value::NIL);
                let unfinished = !borrow.held.is_empty();
                borrow.open = false;
                borrow.held = Vec::new();
                if unfinished {
                    return Err(Error::data(
                        strand,
                        "stream closed with an unfinished character",
                    ));
                }
                Ok(())
            })
    }
}

/// A window of a seekable stream, with a position of its own.
///
/// The span assumes nothing else moves the stream: it seeks only when its own position
/// moves, or to bring a stream that started outside the window into it.
pub(crate) struct Span<B> {
    io: B,
    /// The window's absolute start.
    start: u64,
    /// The window's absolute end, when it is bounded.
    end: Option<u64>,
    /// Position within the window.
    pos: u64,
    /// Whether the stream sits at the position.
    aligned: bool,
}

impl<B: AsyncSeek + Unpin> Span<B> {
    /// A span over `io`, whose absolute position is `at`. The span starts there, clamped to
    /// the window.
    fn new(io: B, start: u64, end: Option<u64>, at: u64) -> Self {
        let clamped = at.max(start).min(end.unwrap_or(u64::MAX));
        Self {
            io,
            start,
            end,
            pos: clamped - start,
            aligned: clamped == at,
        }
    }

    /// Bytes left before the end of a bounded window.
    fn room(&self) -> usize {
        match self.end {
            Some(end) => {
                let at = self.start.saturating_add(self.pos);
                usize::try_from(end.saturating_sub(at)).unwrap_or(usize::MAX)
            }
            None => usize::MAX,
        }
    }

    /// Seeks the stream to the position if it isn't there, then runs `op` and advances past
    /// what it moved.
    fn poll_op(
        &mut self,
        cx: &mut Context<'_>,
        op: impl FnOnce(Pin<&mut B>, &mut Context<'_>) -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        if !self.aligned {
            let at = self
                .start
                .checked_add(self.pos)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            ready!(Pin::new(&mut self.io).poll_seek(cx, SeekFrom::Start(at)))?;
            self.aligned = true;
        }
        let res = ready!(op(Pin::new(&mut self.io), cx));
        // A failed operation may have moved the stream by any amount.
        self.aligned = res.is_ok();
        if let Ok(n) = res {
            self.pos += n as u64;
        }
        Poll::Ready(res)
    }
}

impl<B: AsyncRead + AsyncSeek + Unpin> AsyncRead for Span<B> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = buf.len().min(this.room());
        if n == 0 {
            return Poll::Ready(Ok(0));
        }
        this.poll_op(cx, |io, cx| io.poll_read(cx, &mut buf[..n]))
    }
}

/// Cuts writes short at the end of a bounded window. Closing only flushes, since the stream
/// is the target's.
impl<B: AsyncWrite + AsyncSeek + Unpin> AsyncWrite for Span<B> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = buf.len().min(this.room());
        if n == 0 {
            return Poll::Ready(Ok(0));
        }
        this.poll_op(cx, |io, cx| io.poll_write(cx, &buf[..n]))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut room = this.room();
        let bufs = bufs
            .iter()
            .map_while(|buf| {
                let n = buf.len().min(room);
                room -= n;
                (n > 0).then(|| IoSlice::new(&buf[..n]))
            })
            .collect::<Vec<_>>();
        if bufs.is_empty() {
            return Poll::Ready(Ok(0));
        }
        this.poll_op(cx, |io, cx| io.poll_write_vectored(cx, &bufs))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }
}

/// Seeks within the window. Only an end seek on an open window touches the stream, to
/// measure it.
impl<B: AsyncSeek + Unpin> AsyncSeek for Span<B> {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let pos = match pos {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(this.pos) + i128::from(n),
            SeekFrom::End(n) => {
                let len = match this.end {
                    Some(end) => end - this.start,
                    None => {
                        let end = ready!(Pin::new(&mut this.io).poll_seek(cx, SeekFrom::End(0)))?;
                        this.aligned = false;
                        end.saturating_sub(this.start)
                    }
                };
                i128::from(len) + i128::from(n)
            }
        };
        let pos = u64::try_from(pos).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek position must be non-negative",
            )
        })?;
        if pos != this.pos {
            this.aligned = false;
        }
        this.pos = pos;
        Poll::Ready(Ok(pos))
    }
}

/// `io.Window`, a window of a seekable stream.
///
/// The window wraps the handle it took from its target, so it holds no reference to the
/// target itself.
pub(crate) struct Window {
    /// The span, until the window is closed.
    io: Option<Shared<Span<Prefixed>>>,
    caps: IoCaps,
}

impl Window {
    /// Capabilities while open; a closed window has none.
    fn caps(&self) -> IoCaps {
        open_caps(&self.io, self.caps)
    }

    /// Raises unless the window is open and has `need`.
    fn check<'v, 's>(&self, strand: &mut Strand<'v, 's>, need: IoCaps) -> Result<'v, 's, ()> {
        check_caps(strand, "stream", self.io.is_some(), self.caps, need)
    }

    /// Holds the span for a method.
    async fn lock<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Locked<'_, Span<Prefixed>>> {
        let io = self.io.as_mut().expect("stream is closed");
        io.lock().await.into_do_io(strand)
    }
}

impl<'v> IntoByteIo<'v> for Window {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(Window::caps(self))
    }

    /// Lends the span, so the handle shares the window's position.
    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        let io = self.io.as_ref().expect("stream is closed").clone();
        Ok(ByteHandle::with_caps(io, self.caps))
    }
}

impl<'v> Object<'v> for Window {
    const NAME: &'v str = "Window";
    const MODULE: &'v str = "io";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([target], [range]) = unpack!(strand, args, 1, 1)?;
        let (start, end) = range_arg(strand, range)?;
        let handle = crate::io::get(strand, &target, |strand, caps| {
            if caps.contains(IoCaps::SEEK) {
                Ok(())
            } else {
                Err(Error::value(strand, "target must be seekable"))
            }
        })
        .map_err(|err| {
            if err.kind() == ErrorKind::Unsupported {
                Error::type_error(strand, "expected `io.Io` for `target`")
            } else {
                err
            }
        })?;
        let (mut io, caps) = handle.into_prefixed();
        let at = io.stream_position().await.into_do_io(strand)?;
        let window = Window {
            io: Some(Shared::new(Span::new(io, start, end, at))),
            caps,
        };
        this.create(strand, window, &mut out);
        Ok(())
    }

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            borrow.check(request, IoCaps::empty())?;
            request.provide::<ByteIo>(&mut *borrow)?;
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let start_sym = builder.sym("start");
        let end_sym = builder.sym("end");
        builder
            .supertype(TypeObject::Io)
            .get("readable", |this, strand, out| {
                let caps = this.borrow(strand)?.caps();
                Output::set(strand, out, caps.contains(IoCaps::READ));
                Ok(())
            })
            .get("writable", |this, strand, out| {
                let caps = this.borrow(strand)?.caps();
                Output::set(strand, out, caps.contains(IoCaps::WRITE));
                Ok(())
            })
            .get("seekable", |this, strand, out| {
                let caps = this.borrow(strand)?.caps();
                Output::set(strand, out, caps.contains(IoCaps::SEEK));
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size_value(strand, size)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::READ)?;
                let mut io = borrow.lock(strand).await?;
                let mut buf = BinEmbryo::new();
                let chunk = size.min(BYTE_STREAM_CHUNK_SIZE);
                read_spare(&mut *io, strand, &mut buf, chunk)
                    .await
                    .into_do_io(strand)?;
                buf.finish(strand, out);
                Ok(())
            })
            .method("write", async move |this, strand, args, out| {
                let pieces = pieces(strand, args)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::WRITE)?;
                let mut io = borrow.lock(strand).await?;
                let (slices, _) = slices(&pieces);
                let n = io.write_vectored(&slices).await.into_do_io(strand)?;
                Output::set(strand, out, n);
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let pos = seek_arg(strand, offset, start, end, start_sym, end_sym)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let mut io = borrow.lock(strand).await?;
                let pos = io.seek(pos).await.into_do_io(strand)?;
                Output::set(strand, out, pos);
                Ok(())
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::empty())?;
                let mut io = borrow.lock(strand).await?;
                io.flush().await.into_do_io(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                close_shared(&mut borrow.io).await.into_do_io(strand)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{IoSlice, SeekFrom};

    use futures::{
        executor::block_on,
        io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _, Cursor},
    };

    use super::Span;
    use crate::{
        io::{self, ByteHandle},
        method,
        object::array::Array,
        strand::Strand,
        sym::{self, Sym},
        test_support::with_vm,
        value::{Output, Slot, Value},
    };

    fn bin<'v>(strand: &mut Strand<'v, '_>, value: &Value<'v>) -> Vec<u8> {
        value.as_bin(strand.vm()).expect("`Bin`").pin().to_vec()
    }

    #[test]
    fn reads_prefix_then_handle() {
        with_vm(async |strand, [mut stream, mut out]| {
            let handle =
                ByteHandle::read(Cursor::new(b"cdef".to_vec())).with_prefix(b"ab".to_vec());
            Output::set(strand, Slot::reborrow(&mut stream), handle);
            let read = Sym::well_known(sym::READ);
            let mut got = Vec::new();
            loop {
                method!(strand, &stream, read, 3_i64, Slot::reborrow(&mut out)).unwrap();
                let chunk = bin(strand, &out);
                if chunk.is_empty() {
                    break;
                }
                assert!(chunk.len() <= 3);
                got.extend(chunk);
            }
            assert_eq!(got, b"abcdef");
            stream
                .get(
                    strand,
                    Sym::well_known(sym::READABLE),
                    Slot::reborrow(&mut out),
                )
                .unwrap();
            assert_eq!(out.as_bool(strand), Some(true));
            stream
                .get(
                    strand,
                    Sym::well_known(sym::WRITABLE),
                    Slot::reborrow(&mut out),
                )
                .unwrap();
            assert_eq!(out.as_bool(strand), Some(false));
        });
    }

    #[test]
    fn chunks_are_bounded() {
        with_vm(async |strand, [mut stream, mut iter, mut item]| {
            let handle = ByteHandle::read(Cursor::new(b"hello".to_vec()));
            Output::set(strand, Slot::reborrow(&mut stream), handle);
            method!(
                strand,
                &stream,
                Sym::well_known(sym::CHUNKS),
                2_i64,
                Slot::reborrow(&mut iter)
            )
            .unwrap();
            let mut got = Vec::new();
            while iter.next(strand, Slot::reborrow(&mut item)).await.unwrap() {
                let chunk = bin(strand, &item);
                assert!(!chunk.is_empty() && chunk.len() <= 2);
                got.extend(chunk);
            }
            assert_eq!(got, b"hello");
        });
    }

    #[test]
    fn writes_seeks_and_closes() {
        with_vm(async |strand, [mut stream, mut out]| {
            let handle = ByteHandle::read_write_seek(Cursor::new(Vec::new()));
            Output::set(strand, Slot::reborrow(&mut stream), handle);
            method!(
                strand,
                &stream,
                Sym::well_known(sym::WRITE),
                "xyz",
                Slot::reborrow(&mut out)
            )
            .unwrap();
            assert_eq!(out.as_int(strand), Some(3));
            let start = Sym::well_known(sym::START);
            method!(
                strand,
                &stream,
                Sym::well_known(sym::SEEK),
                start: 0_i64,
                Slot::reborrow(&mut out)
            )
            .unwrap();
            assert_eq!(out.as_int(strand), Some(0));
            let read = Sym::well_known(sym::READ);
            method!(strand, &stream, read, 8_i64, Slot::reborrow(&mut out)).unwrap();
            assert_eq!(bin(strand, &out), b"xyz");

            method!(
                strand,
                &stream,
                Sym::well_known(sym::CLOSE),
                Slot::reborrow(&mut out)
            )
            .unwrap();
            stream
                .get(
                    strand,
                    Sym::well_known(sym::READABLE),
                    Slot::reborrow(&mut out),
                )
                .unwrap();
            assert_eq!(out.as_bool(strand), Some(false));
            let res = method!(strand, &stream, read, 1_i64, Slot::reborrow(&mut out));
            assert!(res.is_err());
        });
    }

    #[test]
    fn lent_handle_shares_position() {
        with_vm(async |strand, [mut stream, mut out]| {
            let handle = ByteHandle::read(Cursor::new(b"cd".to_vec())).with_prefix(b"ab".to_vec());
            Output::set(strand, Slot::reborrow(&mut stream), handle);
            let lent = io::get(strand, &stream, |_, _| Ok(())).unwrap();
            let mut reader = lent.into_read().unwrap();
            let mut byte = [0];
            assert_eq!(reader.read(&mut byte).await.unwrap(), 1);
            assert_eq!(byte, *b"a");
            drop(reader);
            method!(
                strand,
                &stream,
                Sym::well_known(sym::READ),
                8_i64,
                Slot::reborrow(&mut out)
            )
            .unwrap();
            assert_eq!(bin(strand, &out), b"b");
        });
    }

    #[test]
    fn concat_handle_reads_items() {
        with_vm(async |strand, [mut items]| {
            let inner = vec![
                Value::from_input(strand, "ab"),
                Value::from_input(strand, 7_i64),
            ];
            let array = strand.builtin_types().array;
            array.create(strand, Array { inner }, Slot::reborrow(&mut items));
            let handle = io::concat(strand, &items, b"\n").unwrap();
            let mut reader = handle.into_read().unwrap();
            let mut got = Vec::new();
            reader.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, b"ab\n7\n");
        });
    }

    #[test]
    fn data_errors_cross_io_bridge() {
        use crate::error::{Error, ErrorKind};

        with_vm(async |strand, []| {
            let err = io::error(
                strand,
                std::io::Error::new(std::io::ErrorKind::InvalidData, "bad"),
            );
            assert_eq!(err.kind(), ErrorKind::Data);
            let err = io::error(
                strand,
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad"),
            );
            assert_eq!(err.kind(), ErrorKind::Value);

            let err = Error::data(strand, "bad");
            let bridged = io::from_do(strand, err);
            assert_eq!(bridged.kind(), std::io::ErrorKind::InvalidData);
            let back = io::error(strand, bridged);
            assert_eq!(back.kind(), ErrorKind::Data);
            assert_eq!(back.value_message().as_deref(), Some("bad"));
        });
    }

    #[test]
    fn decode_errors_describe_the_input() {
        use crate::error::{Error, ErrorKind};

        with_vm(async |strand, []| {
            let err = io::decode_error(strand, std::io::Error::other("bad header"));
            assert_eq!(err.kind(), ErrorKind::Data);
            let err = io::decode_error(
                strand,
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short"),
            );
            assert_eq!(err.kind(), ErrorKind::Data);
            let err = io::decode_error(
                strand,
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad"),
            );
            assert_eq!(err.kind(), ErrorKind::Value);

            // A Do stream's own error keeps its class.
            let err = Error::runtime(strand, "source failed");
            let bridged = io::from_do(strand, err);
            assert_eq!(bridged.kind(), std::io::ErrorKind::Other);
            let back = io::decode_error(strand, bridged);
            assert_eq!(back.kind(), ErrorKind::Runtime);
        });
    }

    #[test]
    fn bounded_span_cuts_reads_and_writes() {
        block_on(async {
            let mut io = Cursor::new(b"0123456789".to_vec());
            let mut span = Span::new(&mut io, 2, Some(6), 0);
            let mut buf = [0; 8];
            assert_eq!(span.read(&mut buf).await.unwrap(), 4);
            assert_eq!(&buf[..4], b"2345");
            assert_eq!(span.read(&mut buf).await.unwrap(), 0);
            assert_eq!(span.seek(SeekFrom::End(-1)).await.unwrap(), 3);
            assert_eq!(span.seek(SeekFrom::Start(1)).await.unwrap(), 1);
            let bufs = [IoSlice::new(b"ab"), IoSlice::new(b"cd")];
            assert_eq!(span.write_vectored(&bufs).await.unwrap(), 3);
            assert_eq!(span.write(b"e").await.unwrap(), 0);
            assert!(span.seek(SeekFrom::Current(-5)).await.is_err());
            assert_eq!(io.get_ref(), b"012abc6789");
        });
    }

    #[test]
    fn open_span_seeks_from_stream_end() {
        block_on(async {
            let mut io = Cursor::new(b"0123456789".to_vec());
            let mut span = Span::new(&mut io, 7, None, 0);
            assert_eq!(span.seek(SeekFrom::End(0)).await.unwrap(), 3);
            span.write_all(b"abc").await.unwrap();
            span.seek(SeekFrom::Start(0)).await.unwrap();
            let mut got = Vec::new();
            span.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, b"789abc");
        });
    }

    /// A span inside the window starts where the stream is and seeks only when it moves.
    #[test]
    fn span_inside_window_seeks_only_when_moved() {
        block_on(async {
            let mut io = Cursor::new(b"0123456789".to_vec());
            io.set_position(4);
            let mut span = Span::new(&mut io, 2, Some(8), 4);
            assert_eq!(span.seek(SeekFrom::Current(0)).await.unwrap(), 2);
            let mut buf = [0; 2];
            span.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"45");
            span.write_all(b"ab").await.unwrap();
            assert_eq!(span.seek(SeekFrom::Current(-3)).await.unwrap(), 3);
            span.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"5a");
            assert_eq!(io.get_ref(), b"012345ab89");
        });
    }

    /// A stream outside the window is brought to its nearer edge.
    #[test]
    fn span_clamps_into_window() {
        block_on(async {
            let mut io = Cursor::new(b"0123456789".to_vec());
            io.set_position(9);
            let mut span = Span::new(&mut io, 2, Some(6), 9);
            let mut buf = [0; 2];
            assert_eq!(span.seek(SeekFrom::Current(0)).await.unwrap(), 4);
            assert_eq!(span.read(&mut buf).await.unwrap(), 0);
            let mut span = Span::new(&mut io, 2, Some(6), 0);
            span.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"23");
        });
    }
}
