//! The handle end of a Do stream's byte handle.
//!
//! A [`Handle`] sends each read, write and seek as a [`Request`] to a server strand
//! that calls the stream's methods, and the servers' ends are [`Lanes`]. The handle
//! buffers reads and writes. Closing the handle closes its lanes and waits for the
//! servers to stop, but leaves the stream open.
//!
//! A seekable stream has one position, so its requests share one lane. Otherwise reads
//! and writes are independent, and a stream that does both has a lane for each.

use std::{
    future::Future,
    io::{self, SeekFrom},
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::{
    channel::{mpsc, oneshot},
    io::{AsyncRead, AsyncSeek, AsyncWrite},
};

use super::{ByteHandle, IoCaps};

/// The least a read requests from the stream.
const READ_SIZE: usize = 8192;

/// The most writes buffer before they go to the stream.
const WRITE_SIZE: usize = 8192;

pub(crate) type Reply<T> = oneshot::Sender<io::Result<T>>;
type Pending<T> = Option<oneshot::Receiver<io::Result<T>>>;
type Lane = mpsc::UnboundedSender<Request>;

/// A request to a server, with the channel for its result.
pub(crate) enum Request {
    Read(usize, Reply<Vec<u8>>),
    Write(Vec<u8>, Reply<usize>),
    Seek(SeekFrom, Reply<u64>),
}

/// The servers' ends of a handle.
pub(crate) struct Lanes {
    /// One receiver per server.
    pub(crate) servers: Vec<mpsc::UnboundedReceiver<Request>>,
    /// Where the owner reports that the servers stopped.
    pub(crate) done: Reply<()>,
}

/// Creates a handle with `caps`, and the lanes its servers serve.
///
/// # Panics
///
/// Panics if `caps` neither reads nor writes.
pub(crate) fn handle(caps: IoCaps) -> (ByteHandle, Lanes) {
    let read = caps.contains(IoCaps::READ);
    let write = caps.contains(IoCaps::WRITE);
    let seek = caps.contains(IoCaps::SEEK);
    let (reads, writes, servers) = if seek || !(read && write) {
        let (tx, rx) = mpsc::unbounded();
        (read.then(|| tx.clone()), write.then_some(tx), vec![rx])
    } else {
        let (read_tx, read_rx) = mpsc::unbounded();
        let (write_tx, write_rx) = mpsc::unbounded();
        (Some(read_tx), Some(write_tx), vec![read_rx, write_rx])
    };
    let (done_tx, done_rx) = oneshot::channel();
    let io = Handle {
        reads,
        writes,
        seekable: seek,
        read_buf: Vec::new(),
        read_pos: 0,
        read: None,
        write_buf: Vec::new(),
        write: None,
        seek: None,
        done: Some(done_rx),
    };
    let lanes = Lanes {
        servers,
        done: done_tx,
    };
    (ByteHandle::with_caps(io, caps), lanes)
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "stream closed")
}

fn send<T>(
    lane: Option<&Lane>,
    request: impl FnOnce(Reply<T>) -> Request,
) -> io::Result<oneshot::Receiver<io::Result<T>>> {
    let lane = lane.ok_or_else(closed)?;
    let (tx, rx) = oneshot::channel();
    lane.unbounded_send(request(tx)).map_err(|_| closed())?;
    Ok(rx)
}

fn poll_reply<T>(pending: &mut Pending<T>, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
    let rx = pending.as_mut().expect("no request pending");
    let res = ready!(Pin::new(rx).poll(cx));
    *pending = None;
    Poll::Ready(res.unwrap_or_else(|_| Err(closed())))
}

/// The consumer's end: buffers reads and writes, and sends requests to the servers.
struct Handle {
    reads: Option<Lane>,
    writes: Option<Lane>,
    seekable: bool,
    read_buf: Vec<u8>,
    read_pos: usize,
    read: Pending<Vec<u8>>,
    write_buf: Vec<u8>,
    write: Pending<usize>,
    seek: Pending<u64>,
    done: Pending<()>,
}

impl Handle {
    fn unread(&self) -> usize {
        self.read_buf.len() - self.read_pos
    }

    /// Refills the empty read buffer, returning whether the stream had more.
    fn poll_fill(&mut self, cx: &mut Context<'_>, len: usize) -> Poll<io::Result<bool>> {
        if self.read.is_none() {
            let size = len.max(READ_SIZE);
            self.read = Some(send(self.reads.as_ref(), |tx| Request::Read(size, tx))?);
        }
        let data = ready!(poll_reply(&mut self.read, cx))?;
        let more = !data.is_empty();
        self.read_buf = data;
        self.read_pos = 0;
        Poll::Ready(Ok(more))
    }

    /// Finishes a read the consumer stopped waiting for, keeping its bytes as unread.
    fn poll_settle_read(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.read.is_some() {
            ready!(self.poll_fill(cx, 0))?;
        }
        Poll::Ready(Ok(()))
    }

    /// Writes out the write buffer, repeating short writes.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.write_buf.is_empty() {
            if self.write.is_none() {
                let data = self.write_buf.clone();
                self.write = Some(send(self.writes.as_ref(), |tx| Request::Write(data, tx))?);
            }
            let written = ready!(poll_reply(&mut self.write, cx))?;
            self.write_buf.drain(..written);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Handle {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.unread() == 0 {
            // Buffered writes come before the position a read starts from.
            if this.seekable {
                ready!(this.poll_drain(cx))?;
            }
            if !ready!(this.poll_fill(cx, buf.len()))? {
                return Poll::Ready(Ok(0));
            }
        }
        let n = this.unread().min(buf.len());
        buf[..n].copy_from_slice(&this.read_buf[this.read_pos..this.read_pos + n]);
        this.read_pos += n;
        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for Handle {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.writes.is_none() {
            return Poll::Ready(Err(closed()));
        }
        // The stream's position is past unread bytes, so move it back to write at the
        // position the consumer sees.
        if this.seekable {
            ready!(this.poll_settle_read(cx))?;
            if this.unread() > 0 {
                ready!(Pin::new(&mut *this).poll_seek(cx, SeekFrom::Current(0)))?;
            }
        }
        if this.write_buf.len() >= WRITE_SIZE {
            ready!(this.poll_drain(cx))?;
        }
        let n = buf.len().min(WRITE_SIZE - this.write_buf.len());
        this.write_buf.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_drain(cx)
    }

    /// Writes out the buffer, closes the lanes, and waits for the servers to stop.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        this.reads = None;
        this.writes = None;
        let Some(rx) = &mut this.done else {
            return Poll::Ready(Ok(()));
        };
        let res = ready!(Pin::new(rx).poll(cx));
        this.done = None;
        // The owner stops without reporting only when the VM shuts down.
        Poll::Ready(res.unwrap_or(Ok(())))
    }
}

impl AsyncSeek for Handle {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        if this.seek.is_none() {
            ready!(this.poll_settle_read(cx))?;
            ready!(this.poll_drain(cx))?;
            let pos = match pos {
                SeekFrom::Current(n) => SeekFrom::Current(
                    i64::try_from(this.unread())
                        .ok()
                        .and_then(|unread| n.checked_sub(unread))
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
                ),
                pos => pos,
            };
            let lane = this.reads.as_ref().or(this.writes.as_ref());
            this.seek = Some(send(lane, |tx| Request::Seek(pos, tx))?);
        }
        let res = ready!(poll_reply(&mut this.seek, cx));
        if res.is_ok() {
            this.read_buf.clear();
            this.read_pos = 0;
        }
        Poll::Ready(res)
    }
}
