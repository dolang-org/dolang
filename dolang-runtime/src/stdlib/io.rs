//! The `io` module, `io.Buffered`, a buffer over a [`ByteHandle`], [`Stream`], the
//! anonymous `io.Io` a handle becomes as an [`Input`], and `io.Concat`, an iterable read as
//! bytes.
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

use super::cursor::Cursor;
use crate::{
    BYTE_STREAM_CHUNK_SIZE,
    arg::{Arg, Args},
    error::{Error, ErrorKind, Result},
    io::{
        ByteHandle, ByteIo, IntoByteIo, Io, IoCaps, IoErrorExt as _, IoResultExt as _, Prefixed,
        Shared,
    },
    object::{
        interface::Request,
        native::{Instance, Mut, Object, Type, TypeBuilder},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        BinEmbryo, Input, InputBy, Output, Slot, TypeObject, Value,
        private::Sealed,
        view::{PinBin, PinStr},
    },
    vm::{Alloc, Builder, StateTag, Vm},
};

/// Buffer capacity, and the size from which reads and writes skip the buffer.
const CAPACITY: usize = 8 * 1024;

/// A buffer over an owned byte handle.
struct BufIo {
    io: Io,
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
        self.io.caps().contains(IoCaps::SEEK)
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
pub(super) async fn read_spare<'v>(
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
        let ByteHandle { io, prefix } = handle;
        Self {
            caps: io.caps(),
            io: Some(Shared::new(BufIo {
                io,
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
pub(super) fn check_caps<'v, 's>(
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
pub(super) fn open_caps<T>(io: &Option<Shared<T>>, caps: IoCaps) -> IoCaps {
    if io.is_some() { caps } else { IoCaps::empty() }
}

/// Closes a shared stream for every handle, then closes the stream itself. Closing a closed
/// stream does nothing.
pub(super) async fn close_shared<T: AsyncWrite + Unpin>(
    io: &mut Option<Shared<T>>,
) -> io::Result<()> {
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
    Error::runtime(strand, "invalid UTF-8 data")
}

/// Parses a byte count.
pub(super) fn size_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    size: Slot<'v, '_>,
) -> Result<'v, 's, usize> {
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
pub(super) enum Piece<'v, 'a> {
    Str(PinStr<'v, 'a>),
    Bin(PinBin<'v, 'a>),
}

impl Deref for Piece<'_, '_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Piece::Str(s) => s.as_bytes(),
            Piece::Bin(b) => b,
        }
    }
}

/// Pins the arguments of a `write`: any number of `Str` or `Bin` pieces.
pub(super) fn pieces<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
) -> Result<'v, 's, Vec<Piece<'v, 'a>>> {
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
pub(super) fn slices<'a>(pieces: &'a [Piece<'_, '_>]) -> (Vec<IoSlice<'a>>, usize) {
    let slices = pieces
        .iter()
        .map(|piece| IoSlice::new(piece))
        .collect::<Vec<_>>();
    let len = pieces.iter().map(|piece| piece.len()).sum();
    (slices, len)
}

/// Parses `seek`'s arguments: exactly one of a relative `offset`, `start:` or `end:`.
pub(super) fn seek_arg<'v, 's>(
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
    let buffered = builder.register_type::<Buffered>();
    let stream = builder.register_type::<Stream>();
    builder.register_state::<StreamType>(stream);
    let concat = builder.register_type::<Concat>();
    builder.register_state::<ConcatType>(concat);
    let cursor = builder.register_type::<Cursor>();
    let io = builder.singletons().io.dup();
    builder
        .module("io")
        .value("Io", &io)
        .value("Buffered", buffered)
        .value("Concat", concat)
        .value("Cursor", cursor)
        .commit();
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
    io: Option<Shared<Prefixed<Io>>>,
    caps: IoCaps,
}

/// The registered [`Stream`] type, for the [`Input`] impl of [`ByteHandle`].
struct StreamType;

impl StateTag for StreamType {
    type State<'v> = Type<'v, Stream>;
}

impl Stream {
    fn new(handle: ByteHandle) -> Self {
        let ByteHandle { io, prefix } = handle;
        Self {
            caps: io.caps(),
            io: Some(Shared::new(Prefixed::new(io, prefix))),
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
    ) -> Result<'v, 's, Locked<'_, Prefixed<Io>>> {
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
    #[allow(private_interfaces)]
    fn input_take<'a>(&'a mut self, vm: &'a Vm<'v>, _: Sealed) -> InputBy<'v, 'a> {
        let handle = mem::replace(self, ByteHandle::read(futures::io::empty()));
        let ty = *vm.state::<StreamType>();
        InputBy::Value(ty.create_raw(vm, Stream::new(handle), ()), None)
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
        strand
            .with_slots(async move |strand, [mut item]| {
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
            .await
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
        this.cast(&out).unwrap().enter_sync(strand, |strand, inst| {
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

#[cfg(test)]
mod tests {
    use futures::io::{AsyncReadExt as _, Cursor};

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
                method!(strand, &stream, read, Slot::reborrow(&mut out), 3_i64)
                    .await
                    .unwrap();
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
                Slot::reborrow(&mut iter),
                2_i64
            )
            .await
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
                Slot::reborrow(&mut out),
                "xyz"
            )
            .await
            .unwrap();
            assert_eq!(out.as_int(strand), Some(3));
            let start = Sym::well_known(sym::START);
            method!(
                strand,
                &stream,
                Sym::well_known(sym::SEEK),
                Slot::reborrow(&mut out),
                start: 0_i64
            )
            .await
            .unwrap();
            assert_eq!(out.as_int(strand), Some(0));
            let read = Sym::well_known(sym::READ);
            method!(strand, &stream, read, Slot::reborrow(&mut out), 8_i64)
                .await
                .unwrap();
            assert_eq!(bin(strand, &out), b"xyz");

            method!(
                strand,
                &stream,
                Sym::well_known(sym::CLOSE),
                Slot::reborrow(&mut out)
            )
            .await
            .unwrap();
            stream
                .get(
                    strand,
                    Sym::well_known(sym::READABLE),
                    Slot::reborrow(&mut out),
                )
                .unwrap();
            assert_eq!(out.as_bool(strand), Some(false));
            let res = method!(strand, &stream, read, Slot::reborrow(&mut out), 1_i64).await;
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
                Slot::reborrow(&mut out),
                8_i64
            )
            .await
            .unwrap();
            assert_eq!(bin(strand, &out), b"b");
        });
    }

    #[test]
    fn concat_handle_reads_items() {
        with_vm(async |strand, [mut items]| {
            let vm = strand.vm();
            let inner = vec![Value::from_input(vm, "ab"), Value::from_input(vm, 7_i64)];
            let array = vm.builtin_types().array;
            array.create(strand, Array { inner }, Slot::reborrow(&mut items));
            let handle = io::concat(strand, &items, b"\n").unwrap();
            let mut reader = handle.into_read().unwrap();
            let mut got = Vec::new();
            reader.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, b"ab\n7\n");
        });
    }
}
