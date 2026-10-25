//! The `io` module, and `io.Buffered`, a buffer over a [`ByteHandle`].
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
    io::{self, SeekFrom},
    mem::MaybeUninit,
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
    error::{Error, ErrorKind, Result},
    io::{ByteHandle, ByteIo, IntoByteIo, Io, IoCaps, IoErrorExt as _, IoResultExt as _, Shared},
    object::{
        interface::Request,
        native::{Instance, Object, Type, TypeBuilder},
    },
    strand::Strand,
    unpack,
    value::{BinEmbryo, Output, Slot, TypeObject},
    vm::{Alloc, Builder},
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
                out.reserve(alloc, chunk);
                let spare = &mut out.spare_capacity_mut()[..chunk];
                spare.fill(MaybeUninit::new(0));
                // SAFETY: every byte of `spare` was just initialized.
                let spare = unsafe { &mut *(spare as *mut [MaybeUninit<u8>] as *mut [u8]) };
                let read = self.read(spare).await?;
                if read == 0 {
                    return Ok(());
                }
                // SAFETY: `read` bytes at the front of the spare capacity are initialized.
                unsafe { out.advance(read) };
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
        if self.io.is_none() {
            return Err(Error::state_error(strand, "buffer is closed"));
        }
        let missing = need - self.caps;
        if missing.contains(IoCaps::READ) {
            Err(Error::state_error(strand, "buffer is not readable"))
        } else if missing.contains(IoCaps::WRITE) {
            Err(Error::state_error(strand, "buffer is not writable"))
        } else if missing.contains(IoCaps::SEEK) {
            Err(Error::state_error(strand, "buffer is not seekable"))
        } else {
            Ok(())
        }
    }

    /// Capabilities while open; a closed buffer has none.
    fn open_caps(&self) -> IoCaps {
        if self.io.is_some() {
            self.caps
        } else {
            IoCaps::empty()
        }
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
        let Some(mut shared) = self.io.take() else {
            return Ok(());
        };
        match shared.close().await {
            Some(mut io) => io.close().await,
            None => Ok(()),
        }
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

/// Parses an optional byte count.
fn size_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    size: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<usize>> {
    size.map(|size| {
        size.to_i64(strand)
            .ok()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Error::type_error(strand, "size must be a non-negative integer"))
    })
    .transpose()
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
    let io = builder.singletons().io.dup();
    builder
        .module("io")
        .value("Io", &io)
        .value("Buffered", buffered)
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
        args: crate::arg::Args<'v, 'a>,
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
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::WRITE)?;
                let mut io = borrow.lock(strand).await?;
                let res = if let Some(b) = data.as_bin(strand.vm()) {
                    let b = b.pin();
                    io.write_all(&b).await.map(|()| b.len())
                } else if let Some(s) = data.as_str(strand.vm()) {
                    let s = s.pin();
                    io.write_all(s.as_bytes()).await.map(|()| s.len())
                } else {
                    return Err(Error::type_error(strand, "expected `Str` or `Bin`"));
                };
                let written = res.into_do_io(strand)?;
                Output::set(strand, out, written);
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
                let pos = match (offset, start, end) {
                    (Some(offset), None, None) => {
                        SeekFrom::Current(offset.to_i64(strand).map_err(|_| {
                            Error::type_error(strand, "seek offset must be an integer")
                        })?)
                    }
                    (None, Some(start), None) => {
                        let start = start
                            .to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "start must be an integer"))?;
                        SeekFrom::Start(u64::try_from(start).map_err(|_| {
                            Error::value(strand, "start offset must be non-negative")
                        })?)
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
                };
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let res = borrow.lock(strand).await?.seek(pos).await;
                let pos = res.into_do_io(strand)?;
                Output::set(strand, out, pos);
                Ok(())
            })
    }
}
