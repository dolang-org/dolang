//! The buffer of a buffered `fs.File`.
//!
//! A file has one position, so its buffer holds read-ahead or pending writes, never both: a
//! write first moves the file back over unread bytes, and a read or seek first writes out
//! pending writes. Reads and writes at least a buffer long skip the buffer once it is
//! drained or flushed.
//!
//! The buffer sits in a [`Shared`](dolang::runtime::io::Shared) with its own clone of the
//! file, so the handles a file lends read and write through the same buffer.

use std::{
    future::poll_fn,
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll, ready},
};

use dolang::runtime::{BYTE_STREAM_CHUNK_SIZE, Strand, value::BinEmbryo};
use dolang_vfs::file::File as VfsFile;
use futures::io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};
use tokio::io::{
    AsyncRead as TokioRead, AsyncSeek as TokioSeek, AsyncWrite as TokioWrite, ReadBuf,
};

use crate::fs::read_into_spare;

/// Buffer capacity, and the size from which reads and writes skip the buffer.
pub(crate) const CAPACITY: usize = 8 * 1024;

/// A buffer over a clone of an open file.
pub(crate) struct FileBuf {
    file: VfsFile,
    /// Read-ahead unread from `pos`, or pending writes when `writing`.
    buf: Vec<u8>,
    pos: usize,
    writing: bool,
    /// A seek on `file` was started and has not completed.
    seeking: bool,
}

/// Reads up to `want` bytes from `file` straight into `out`'s spare capacity.
pub(crate) async fn read_chunk<'v>(
    file: &mut VfsFile,
    strand: &mut Strand<'v, '_>,
    out: &mut BinEmbryo<'v>,
    want: usize,
) -> io::Result<usize> {
    let chunk = want.min(BYTE_STREAM_CHUNK_SIZE);
    if out.spare_capacity_mut().len() < chunk {
        out.reserve(strand, chunk);
    }
    let spare = out.spare_capacity_mut();
    let end = chunk.min(spare.len());
    let read = read_into_spare(file, &mut spare[..end]).await?;
    // SAFETY: `read` bytes at the front of the spare capacity are initialized.
    unsafe { out.advance(read) };
    Ok(read)
}

impl FileBuf {
    pub(crate) fn new(file: VfsFile) -> Self {
        Self {
            file,
            buf: Vec::new(),
            pos: 0,
            writing: false,
            seeking: false,
        }
    }

    pub(crate) fn unread(&self) -> usize {
        if self.writing {
            0
        } else {
            self.buf.len() - self.pos
        }
    }

    pub(crate) fn pending(&self) -> usize {
        if self.writing { self.buf.len() } else { 0 }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.writing = false;
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
        if self.pos == self.buf.len() {
            self.clear();
        }
    }

    /// Returns bytes taken from the file to the front of the read-ahead, so a failed read
    /// loses nothing.
    pub(crate) fn push_back(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        debug_assert!(!self.writing, "read-ahead while writing");
        if bytes.len() <= self.pos {
            self.pos -= bytes.len();
            self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        } else {
            let mut buf = Vec::with_capacity(bytes.len() + self.unread());
            buf.extend_from_slice(bytes);
            buf.extend_from_slice(&self.buf[self.pos..]);
            self.buf = buf;
            self.pos = 0;
        }
    }

    /// Seeks the file, finishing a seek an earlier poll started instead of starting another.
    fn poll_seek_file(&mut self, cx: &mut Context<'_>, pos: SeekFrom) -> Poll<io::Result<u64>> {
        if !self.seeking {
            Pin::new(&mut self.file).start_seek(pos)?;
            self.seeking = true;
        }
        let res = ready!(Pin::new(&mut self.file).poll_complete(cx));
        self.seeking = false;
        Poll::Ready(res)
    }

    fn poll_flush_buf(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.writing {
            return Poll::Ready(Ok(()));
        }
        while !self.buf.is_empty() {
            let n = ready!(Pin::new(&mut self.file).poll_write(cx, &self.buf))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.buf.drain(..n);
        }
        self.clear();
        Poll::Ready(Ok(()))
    }

    /// Moves the file back over unread read-ahead and drops it.
    fn poll_unread_back(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let unread = self.unread();
        if unread > 0 {
            let back = -i64::try_from(unread).map_err(io::Error::other)?;
            ready!(self.poll_seek_file(cx, SeekFrom::Current(back)))?;
        }
        if !self.writing {
            self.clear();
        }
        Poll::Ready(Ok(()))
    }

    /// Writes out pending writes.
    pub(crate) async fn flush_buf(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush_buf(cx)).await
    }

    /// Moves the file back over unread read-ahead and drops it, so the file sits at the
    /// logical position.
    pub(crate) async fn unread_back(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_unread_back(cx)).await
    }

    /// Leaves the file at the logical position, with nothing buffered.
    pub(crate) async fn settle(&mut self) -> io::Result<()> {
        self.flush_buf().await?;
        self.unread_back().await
    }

    /// Writes out pending writes, then flushes the file.
    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        poll_fn(|cx| Pin::new(&mut *self).poll_flush(cx)).await
    }

    /// Writes all of `data`.
    pub(crate) async fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = poll_fn(|cx| Pin::new(&mut *self).poll_write(cx, data)).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            data = &data[n..];
        }
        Ok(())
    }

    pub(crate) async fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        poll_fn(|cx| Pin::new(&mut *self).poll_seek(cx, pos)).await
    }

    /// The logical position: the file's, less unread bytes, plus pending writes.
    pub(crate) async fn tell(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0)).await
    }

    /// Appends `n` bytes to `out`, or the rest of the file when `n` is `None`, stopping
    /// short only at the end of the file.
    pub(crate) async fn read_into<'v>(
        &mut self,
        strand: &mut Strand<'v, '_>,
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
                self.flush_buf().await?;
                if read_chunk(&mut self.file, strand, out, want).await? == 0 {
                    return Ok(());
                }
            } else {
                let avail = poll_fn(|cx| self.poll_fill(cx)).await?;
                if avail == 0 {
                    return Ok(());
                }
                let take = avail.min(want);
                out.extend(strand, &self.buf[self.pos..self.pos + take]);
                self.consume(take);
            }
        }
    }

    /// Appends one line to `out`, terminator included. Nothing is appended at the end of
    /// the file.
    pub(crate) async fn read_line_into<'v>(
        &mut self,
        strand: &mut Strand<'v, '_>,
        out: &mut BinEmbryo<'v>,
    ) -> io::Result<()> {
        loop {
            if poll_fn(|cx| self.poll_fill(cx)).await? == 0 {
                return Ok(());
            }
            let avail = &self.buf[self.pos..];
            let (take, done) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (avail.len(), false),
            };
            out.extend(strand, &avail[..take]);
            self.consume(take);
            if done {
                return Ok(());
            }
        }
    }

    /// Refills the read-ahead only once it is drained, returning how much is unread. Zero
    /// means end of file.
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        if self.unread() == 0 {
            ready!(self.poll_flush_buf(cx))?;
            self.buf.resize(CAPACITY, 0);
            let mut read = ReadBuf::new(&mut self.buf);
            match Pin::new(&mut self.file).poll_read(cx, &mut read) {
                Poll::Ready(Ok(())) => {
                    let n = read.filled().len();
                    self.buf.truncate(n);
                }
                res => {
                    self.buf.clear();
                    return res.map_ok(|()| 0);
                }
            }
        }
        Poll::Ready(Ok(self.unread()))
    }
}

impl AsyncRead for FileBuf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.unread() == 0 && buf.len() >= CAPACITY {
            ready!(this.poll_flush_buf(cx))?;
            let mut read = ReadBuf::new(buf);
            ready!(Pin::new(&mut this.file).poll_read(cx, &mut read))?;
            return Poll::Ready(Ok(read.filled().len()));
        }
        let avail = ready!(this.poll_fill(cx))?;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&this.buf[this.pos..this.pos + n]);
        this.consume(n);
        Poll::Ready(Ok(n))
    }
}

impl AsyncBufRead for FileBuf {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        ready!(this.poll_fill(cx))?;
        Poll::Ready(Ok(&this.buf[this.pos..]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.get_mut().consume(amt);
    }
}

impl AsyncWrite for FileBuf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // Writing at the logical position means leaving any read-ahead.
        ready!(this.poll_unread_back(cx))?;
        if this.buf.len() + data.len() > CAPACITY {
            ready!(this.poll_flush_buf(cx))?;
        }
        if data.len() >= CAPACITY {
            return Pin::new(&mut this.file).poll_write(cx, data);
        }
        this.buf.extend_from_slice(data);
        this.writing = true;
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_flush_buf(cx))?;
        Pin::new(&mut this.file).poll_flush(cx)
    }

    /// Only flushes: closing the file is up to its owner.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

/// `SeekFrom::Current(0)` reports the logical position, keeping the buffer.
impl AsyncSeek for FileBuf {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        if pos == SeekFrom::Current(0) {
            let pos = ready!(this.poll_seek_file(cx, pos))?;
            let pos = (pos + this.pending() as u64)
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
        let pos = ready!(this.poll_seek_file(cx, pos))?;
        this.clear();
        Poll::Ready(Ok(pos))
    }
}
