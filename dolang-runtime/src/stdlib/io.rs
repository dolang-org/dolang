//! The `io` module, and `io.Buffered`, a buffer over a [`ByteHandle`].
//!
//! A seekable handle that reads and writes holds read-ahead or pending writes, never both:
//! a write first moves the handle back over unread bytes, and a read or seek first flushes.
//! A handle that cannot seek keeps the two buffers apart, since its reads and writes are
//! separate streams. Reads and writes at least a buffer long skip the buffer once it is
//! drained or flushed.

use std::{
    future::poll_fn,
    io::{self, SeekFrom},
    mem::MaybeUninit,
    pin::Pin,
    str,
    task::{Context, Poll, ready},
};

use dolang_util::utf8;
use futures::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::{
    BYTE_STREAM_CHUNK_SIZE,
    error::{Error, ErrorKind, Result},
    io::{ByteHandle, ByteIo, IntoByteIo, Io, IoCaps},
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
pub(crate) struct Buffered {
    /// The handle, until it is closed or taken.
    io: Option<Io>,
    caps: IoCaps,
    /// Read-ahead, unread from `rpos`.
    rbuf: Vec<u8>,
    rpos: usize,
    /// Pending writes.
    wbuf: Vec<u8>,
}

impl Buffered {
    /// Wraps `handle`, serving its prefix first.
    fn new(handle: ByteHandle) -> Self {
        let ByteHandle { io, prefix } = handle;
        Self {
            caps: io.caps(),
            io: Some(io),
            rbuf: prefix,
            rpos: 0,
            wbuf: Vec::new(),
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

    fn io(&mut self) -> &mut Io {
        self.io.as_mut().expect("buffer is closed")
    }

    fn unread(&self) -> usize {
        self.rbuf.len() - self.rpos
    }

    fn seekable(&self) -> bool {
        self.caps.contains(IoCaps::SEEK)
    }

    fn consume(&mut self, n: usize) {
        self.rpos += n;
        if self.rpos == self.rbuf.len() {
            self.rbuf.clear();
            self.rpos = 0;
        }
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

    fn poll_flush_buf(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let io = self.io.as_mut().expect("buffer is closed");
        while !self.wbuf.is_empty() {
            let n = ready!(Pin::new(&mut *io).poll_write(cx, &self.wbuf))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.wbuf.drain(..n);
        }
        Poll::Ready(Ok(()))
    }

    /// Writes out pending writes.
    async fn flush_buf(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush_buf(cx)).await
    }

    /// Prepares to read from the handle.
    ///
    /// Pending writes go first. On a seekable handle they must, so the read starts after
    /// them. On one that cannot seek, a peer may be waiting for them before it answers.
    async fn begin_read(&mut self) -> io::Result<()> {
        self.flush_buf().await
    }

    /// Prepares to write, moving a seekable handle back over unread bytes.
    async fn begin_write(&mut self) -> io::Result<()> {
        if !self.seekable() {
            return Ok(());
        }
        if self.unread() > 0 {
            let back = -i64::try_from(self.unread()).map_err(io::Error::other)?;
            self.io().seek(SeekFrom::Current(back)).await?;
        }
        self.rbuf.clear();
        self.rpos = 0;
        Ok(())
    }

    /// Returns the read-ahead, refilling it first if it is drained. Empty means end of
    /// stream.
    async fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.unread() == 0 {
            self.begin_read().await?;
            self.rbuf.clear();
            self.rpos = 0;
            self.rbuf.resize(CAPACITY, 0);
            let io = self.io.as_mut().expect("buffer is closed");
            match io.read(&mut self.rbuf).await {
                Ok(n) => self.rbuf.truncate(n),
                Err(err) => {
                    self.rbuf.clear();
                    return Err(err);
                }
            }
        }
        Ok(&self.rbuf[self.rpos..])
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
                self.begin_read().await?;
                let chunk = want.min(BYTE_STREAM_CHUNK_SIZE);
                out.reserve(alloc, chunk);
                let spare = &mut out.spare_capacity_mut()[..chunk];
                spare.fill(MaybeUninit::new(0));
                // SAFETY: every byte of `spare` was just initialized.
                let spare = unsafe { &mut *(spare as *mut [MaybeUninit<u8>] as *mut [u8]) };
                let read = self.io().read(spare).await?;
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

    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.begin_write().await?;
        if self.wbuf.len() + data.len() > CAPACITY {
            self.flush_buf().await?;
        }
        if data.len() >= CAPACITY {
            self.io().write_all(data).await
        } else {
            self.wbuf.extend_from_slice(data);
            Ok(())
        }
    }

    async fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.flush_buf().await?;
        let pos = match pos {
            SeekFrom::Current(n) => {
                let adjusted = i64::try_from(self.unread())
                    .ok()
                    .and_then(|unread| n.checked_sub(unread))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                SeekFrom::Current(adjusted)
            }
            pos => pos,
        };
        let pos = self.io().seek(pos).await?;
        self.rbuf.clear();
        self.rpos = 0;
        Ok(pos)
    }

    /// The logical position: the handle's, less unread bytes, plus pending writes.
    async fn tell(&mut self) -> io::Result<u64> {
        let pos = self.io().seek(SeekFrom::Current(0)).await?;
        (pos + self.wbuf.len() as u64)
            .checked_sub(self.unread() as u64)
            .ok_or_else(|| io::Error::other("position is before buffered data"))
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.flush_buf().await?;
        self.io().flush().await
    }

    /// Flushes and closes the handle, dropping it even if flushing fails.
    async fn close(&mut self) -> io::Result<()> {
        if self.io.is_none() {
            return Ok(());
        }
        let flushed = self.flush_buf().await;
        let mut io = self.io.take().expect("buffer is closed");
        self.rbuf = Vec::new();
        self.rpos = 0;
        self.wbuf = Vec::new();
        let closed = io.close().await;
        flushed.and(closed)
    }
}

impl<'v> IntoByteIo<'v> for Buffered {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(self.caps)
    }

    fn poll_flush(
        &mut self,
        _strand: &mut Strand<'v, '_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        ready!(self.poll_flush_buf(cx))?;
        Pin::new(self.io()).poll_flush(cx)
    }

    fn take<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        assert!(self.wbuf.is_empty(), "buffer taken before flushing");
        let io = self.io.take().expect("buffer already taken");
        let prefix = self.rbuf.split_off(self.rpos);
        self.rbuf = Vec::new();
        self.rpos = 0;
        Ok(ByteHandle { io, prefix })
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
        let mut buf = BinEmbryo::new();
        if let Err(err) = self.read_line_into(strand, &mut buf).await {
            self.push_back(buf.as_slice());
            return Err(crate::io::error(strand, err));
        }
        if str::from_utf8(buf.as_slice()).is_err() {
            self.push_back(buf.as_slice());
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
        let mut buf = BinEmbryo::new();
        if let Err(err) = self.read_into(strand, &mut buf, size).await {
            self.push_back(buf.as_slice());
            return Err(crate::io::error(strand, err));
        }
        loop {
            let eof = size.is_none_or(|size| buf.len() < size);
            let valid = match utf8::complete_prefix(buf.as_slice()) {
                Ok(text) if text.len() == buf.len() => break,
                Ok(text) if !eof => text.len(),
                _ => {
                    self.push_back(buf.as_slice());
                    return Err(invalid_utf8(strand));
                }
            };
            if valid > 0 {
                self.push_back(&buf.as_slice()[valid..]);
                buf.truncate(valid);
                break;
            }
            // The first character is incomplete, so read on until it is whole.
            let before = buf.len();
            if let Err(err) = self.read_into(strand, &mut buf, Some(1)).await {
                self.push_back(buf.as_slice());
                return Err(crate::io::error(strand, err));
            }
            if buf.len() == before {
                self.push_back(buf.as_slice());
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
    let res = if let Some(b) = value.as_bin(strand.vm()) {
        let b = b.pin();
        this.write_all(&b).await
    } else if let Some(s) = value.as_str(strand.vm()) {
        let s = s.pin();
        this.write_all(s.as_bytes()).await
    } else {
        let s = value.to_string(strand)?;
        this.write_all(s.as_bytes()).await
    };
    res.map_err(|err| crate::io::error(strand, err))
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
        let handle = crate::io::take(strand, &source, |_, _| Ok(()))
            .await
            .map_err(|err| {
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
                let mut buf = BinEmbryo::new();
                if let Err(err) = borrow.read_into(strand, &mut buf, size).await {
                    borrow.push_back(buf.as_slice());
                    return Err(crate::io::error(strand, err));
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
                let res = if let Some(b) = data.as_bin(strand.vm()) {
                    let b = b.pin();
                    borrow.write_all(&b).await.map(|()| b.len())
                } else if let Some(s) = data.as_str(strand.vm()) {
                    let s = s.pin();
                    borrow.write_all(s.as_bytes()).await.map(|()| s.len())
                } else {
                    return Err(Error::type_error(strand, "expected `Str` or `Bin`"));
                };
                let written = res.map_err(|err| crate::io::error(strand, err))?;
                Output::set(strand, out, written);
                Ok(())
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::empty())?;
                let res = borrow.flush().await;
                res.map_err(|err| crate::io::error(strand, err))
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                let res = borrow.close().await;
                res.map_err(|err| crate::io::error(strand, err))
            })
            .method("tell", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let res = borrow.tell().await;
                let pos = res.map_err(|err| crate::io::error(strand, err))?;
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
                let res = borrow.seek(pos).await;
                let pos = res.map_err(|err| crate::io::error(strand, err))?;
                Output::set(strand, out, pos);
                Ok(())
            })
    }
}
