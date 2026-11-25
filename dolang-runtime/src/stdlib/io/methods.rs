//! `io.Buffered`'s reads and writes over either kind of source, and the calls `io.Window`
//! and `io.Buffered` make on a stream served through its `Io` methods.
//!
//! Such a stream is called on the strand that called the window or buffer, so its
//! exceptions propagate unchanged. A read returns the `Bin` the stream returned, unless
//! the buffer has to keep it.

use std::io::{IoSlice, SeekFrom};

use dolang_util::{shared::Locked, utf8};
use futures::io::{AsyncBufReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

use super::{
    Buf, BufIo, CAPACITY, SpanPos, cut, invalid_utf8, one_piece, pieces, read_spare, write_zero,
};
use crate::{
    BYTE_STREAM_CHUNK_SIZE,
    arg::Args,
    error::{Error, Result},
    io::IoResultExt as _,
    method,
    object::io::{read_bounded, seek, written},
    strand::Strand,
    sym::{self, Sym},
    value::{BinEmbryo, Slot, Value},
    vm::AllocExt,
    with_slots,
};

/// A buffer's source for the length of one method.
pub(super) enum Source<'x, 'v> {
    /// The locked buffer over a native handle.
    Native(Locked<'x, BufIo>),
    /// The buffer over a stream served through its methods.
    Methods {
        buf: &'x mut Buf,
        target: &'x Value<'v>,
        seekable: bool,
    },
}

impl<'v> Source<'_, 'v> {
    fn buf(&mut self) -> &mut Buf {
        match self {
            Source::Native(io) => &mut io.buf,
            Source::Methods { buf, .. } => buf,
        }
    }

    /// Refills the read-ahead once it is drained, writing out pending writes first. It stays
    /// empty at the end of the stream.
    async fn fill<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        match self {
            Source::Native(io) => {
                let res = (**io).fill_buf().await.map(|_| ());
                res.into_do_io(strand)
            }
            Source::Methods { buf, target, .. } => {
                if buf.unread() > 0 {
                    return Ok(());
                }
                flush_buf(strand, buf, target).await?;
                with_slots!(strand, |mut chunk| {
                    let read_some = Sym::well_known(sym::READ_SOME);
                    read_bounded(
                        strand,
                        target,
                        read_some,
                        CAPACITY,
                        Slot::reborrow(&mut chunk),
                    )
                    .await?;
                    let bytes = chunk
                        .as_bin(strand.vm())
                        .expect("read_bounded checks for `Bin`");
                    buf.rbuf.clear();
                    buf.rbuf.extend_from_slice(&bytes.pin());
                    buf.rpos = 0;
                    Ok(())
                })
            }
        }
    }

    /// Reads once from the source, past the drained read-ahead, appending at most `n` bytes
    /// to `out`, and returns the count.
    async fn read_past<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: &mut BinEmbryo<'v>,
        n: usize,
    ) -> Result<'v, 's, usize> {
        match self {
            Source::Native(io) => read_spare(&mut **io, strand, out, n)
                .await
                .into_do_io(strand),
            Source::Methods { buf, target, .. } => {
                flush_buf(strand, buf, target).await?;
                with_slots!(strand, |mut chunk| {
                    let read_some = Sym::well_known(sym::READ_SOME);
                    read_bounded(strand, target, read_some, n, Slot::reborrow(&mut chunk)).await?;
                    let bytes = chunk
                        .as_bin(strand.vm())
                        .expect("read_bounded checks for `Bin`");
                    let bytes = bytes.pin();
                    out.extend(strand, &bytes);
                    Ok(bytes.len())
                })
            }
        }
    }

    /// Appends `n` bytes to `out`, or the rest of the stream when `n` is `None`, stopping
    /// short only at the end of the stream.
    async fn read_into<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: &mut BinEmbryo<'v>,
        n: Option<usize>,
    ) -> Result<'v, 's, ()> {
        let start = out.len();
        loop {
            let want = match n {
                Some(n) => n - (out.len() - start),
                None => usize::MAX,
            };
            if want == 0 {
                return Ok(());
            }
            if self.buf().unread() == 0 && want >= CAPACITY {
                let chunk = want.min(BYTE_STREAM_CHUNK_SIZE);
                if self.read_past(strand, out, chunk).await? == 0 {
                    return Ok(());
                }
            } else {
                self.fill(strand).await?;
                let buf = self.buf();
                let avail = buf.available();
                if avail.is_empty() {
                    return Ok(());
                }
                let take = avail.len().min(want);
                out.extend(strand, &avail[..take]);
                buf.consume(take);
            }
        }
    }

    /// Appends one line to `out`, terminator included. Nothing is appended at the end of
    /// the stream.
    async fn read_line_into<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: &mut BinEmbryo<'v>,
    ) -> Result<'v, 's, ()> {
        loop {
            self.fill(strand).await?;
            let buf = self.buf();
            let avail = buf.available();
            if avail.is_empty() {
                return Ok(());
            }
            let (take, done) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (avail.len(), false),
            };
            out.extend(strand, &avail[..take]);
            buf.consume(take);
            if done {
                return Ok(());
            }
        }
    }

    /// Runs `read` into `buf` under an interrupt guard, so an interrupted read fails instead
    /// of being dropped with its caller, and pushes back what it read if it fails.
    async fn guarded<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        buf: &mut BinEmbryo<'v>,
        read: impl AsyncFnOnce(&mut Self, &mut Strand<'v, 's>, &mut BinEmbryo<'v>) -> Result<'v, 's, ()>,
    ) -> Result<'v, 's, ()> {
        let res = strand
            .interrupt_guard(async |strand| read(self, strand, buf).await)
            .await;
        if res.is_err() {
            self.buf().push_back(buf.as_slice());
        }
        res
    }

    /// Reads `size` bytes, or the rest of the stream, as a `Bin`.
    pub(super) async fn read<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        size: Option<usize>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = BinEmbryo::new();
        self.guarded(strand, &mut buf, async |this, strand, buf| {
            this.read_into(strand, buf, size).await
        })
        .await?;
        buf.finish(strand, out);
        Ok(())
    }

    /// Reads at most `size` bytes as a `Bin`, reading the source at most once.
    pub(super) async fn read_some<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        size: usize,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let chunk = size.min(BYTE_STREAM_CHUNK_SIZE);
        match self {
            Source::Native(io) => {
                let mut buf = BinEmbryo::new();
                read_spare(&mut **io, strand, &mut buf, chunk)
                    .await
                    .into_do_io(strand)?;
                buf.finish(strand, out);
                Ok(())
            }
            // A read at least a buffer long skips the drained buffer, and its `Bin` is the
            // stream's own.
            Source::Methods { buf, target, .. } if buf.unread() == 0 && chunk >= CAPACITY => {
                flush_buf(strand, buf, target).await?;
                let read_some = Sym::well_known(sym::READ_SOME);
                read_bounded(strand, target, read_some, chunk, out).await
            }
            Source::Methods { .. } => {
                self.fill(strand).await?;
                let buf = self.buf();
                let n = chunk.min(buf.unread());
                strand.store(out, &buf.available()[..n]);
                buf.consume(n);
                Ok(())
            }
        }
    }

    /// Reads a line as a `Str`, or `""` at the end of the stream.
    pub(super) async fn read_line<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = BinEmbryo::new();
        self.guarded(strand, &mut buf, async |this, strand, buf| {
            this.read_line_into(strand, buf).await
        })
        .await?;
        if str::from_utf8(buf.as_slice()).is_err() {
            self.buf().push_back(buf.as_slice());
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
    pub(super) async fn read_str<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        size: Option<usize>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = BinEmbryo::new();
        self.guarded(strand, &mut buf, async |this, strand, buf| {
            this.read_into(strand, buf, size).await
        })
        .await?;
        loop {
            let eof = size.is_none_or(|size| buf.len() < size);
            let valid = match utf8::complete_prefix(buf.as_slice()) {
                Ok(text) if text.len() == buf.len() => break,
                Ok(text) if !eof => text.len(),
                _ => {
                    self.buf().push_back(buf.as_slice());
                    return Err(invalid_utf8(strand));
                }
            };
            if valid > 0 {
                self.buf().push_back(&buf.as_slice()[valid..]);
                buf.truncate(valid);
                break;
            }
            // The first character is incomplete, so read on until it is whole.
            let before = buf.len();
            self.guarded(strand, &mut buf, async |this, strand, buf| {
                this.read_into(strand, buf, Some(1)).await
            })
            .await?;
            if buf.len() == before {
                self.buf().push_back(buf.as_slice());
                return Err(invalid_utf8(strand));
            }
        }
        // SAFETY: validated above.
        unsafe { buf.finish_str_unchecked(strand, out) };
        Ok(())
    }

    /// Writes all of `pieces`, through the buffer unless they are at least a buffer long.
    pub(super) async fn write_all<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        pieces: &[&[u8]],
    ) -> Result<'v, 's, ()> {
        match self {
            Source::Native(io) => {
                let mut slices = pieces
                    .iter()
                    .map(|piece| IoSlice::new(piece))
                    .collect::<Vec<_>>();
                let res = io.write_all_vectored(&mut slices).await;
                res.into_do_io(strand)
            }
            Source::Methods {
                buf,
                target,
                seekable,
            } => {
                begin_write(strand, buf, target, *seekable).await?;
                let len = pieces.iter().map(|piece| piece.len()).sum::<usize>();
                if buf.wbuf.len() + len > CAPACITY {
                    flush_buf(strand, buf, target).await?;
                }
                if len >= CAPACITY {
                    for piece in pieces {
                        write_through(strand, target, piece).await?;
                    }
                } else {
                    for piece in pieces {
                        buf.wbuf.extend_from_slice(piece);
                    }
                }
                Ok(())
            }
        }
    }

    /// Writes out pending writes. The stream's own methods have no flush to call.
    pub(super) async fn flush<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        match self {
            Source::Native(io) => io.flush().await.into_do_io(strand),
            Source::Methods { buf, target, .. } => flush_buf(strand, buf, target).await,
        }
    }

    /// The logical position, keeping both buffers.
    pub(super) async fn tell<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, u64> {
        match self {
            Source::Native(io) => io.stream_position().await.into_do_io(strand),
            Source::Methods { buf, target, .. } => {
                let pos = seek(strand, target, SeekFrom::Current(0)).await?;
                buf.position(pos).into_do_io(strand)
            }
        }
    }

    /// Moves the logical position, writing out pending writes and dropping read-ahead.
    pub(super) async fn seek<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        pos: SeekFrom,
    ) -> Result<'v, 's, u64> {
        match self {
            Source::Native(io) => io.seek(pos).await.into_do_io(strand),
            Source::Methods { .. } if pos == SeekFrom::Current(0) => self.tell(strand).await,
            Source::Methods { buf, target, .. } => {
                flush_buf(strand, buf, target).await?;
                let pos = match pos {
                    SeekFrom::Current(n) => SeekFrom::Current(buf.current(n).into_do_io(strand)?),
                    pos => pos,
                };
                let pos = seek(strand, target, pos).await?;
                buf.drop_read_ahead();
                Ok(pos)
            }
        }
    }
}

/// Writes out `buf`'s pending writes with one `target.write`. A short count means a
/// bounded output is full, so the rest can't be written.
pub(super) async fn flush_buf<'v, 's>(
    strand: &mut Strand<'v, 's>,
    buf: &mut Buf,
    target: &Value<'v>,
) -> Result<'v, 's, ()> {
    if buf.wbuf.is_empty() {
        return Ok(());
    }
    with_slots!(strand, |mut data, mut count| {
        strand.store(&mut data, buf.wbuf.as_slice());
        let write = Sym::well_known(sym::WRITE);
        method!(strand, target, write, &data, &mut count)?;
        let n = written(strand, write, &count, buf.wbuf.len())?;
        buf.wbuf.drain(..n);
        if buf.wbuf.is_empty() {
            Ok(())
        } else {
            Err(write_zero(strand))
        }
    })
}

/// Writes all of `data` with one `target.write`.
async fn write_through<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    data: &[u8],
) -> Result<'v, 's, ()> {
    if data.is_empty() {
        return Ok(());
    }
    with_slots!(strand, |mut bin, mut count| {
        strand.store(&mut bin, data);
        let write = Sym::well_known(sym::WRITE);
        method!(strand, target, write, &bin, &mut count)?;
        if written(strand, write, &count, data.len())? < data.len() {
            return Err(write_zero(strand));
        }
        Ok(())
    })
}

/// Prepares to write, moving a seekable stream back over unread bytes.
async fn begin_write<'v, 's>(
    strand: &mut Strand<'v, 's>,
    buf: &mut Buf,
    target: &Value<'v>,
    seekable: bool,
) -> Result<'v, 's, ()> {
    if !seekable {
        return Ok(());
    }
    if buf.unread() > 0 {
        let back = i64::try_from(buf.unread()).map_err(|_| Error::overflow(strand))?;
        seek(strand, target, SeekFrom::Current(-back)).await?;
    }
    buf.drop_read_ahead();
    Ok(())
}

/// Seeks `target` to `at`'s position if it isn't there.
async fn realign<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    at: &mut SpanPos,
) -> Result<'v, 's, ()> {
    if let Some(abs) = at.realign().into_do_io(strand)? {
        seek(strand, target, SeekFrom::Start(abs)).await?;
        at.aligned = true;
    }
    Ok(())
}

/// Calls `target.<method> size`, a `read` or `read_some`, for a window at `at`, with the
/// size cut at the window's end, and stores the `Bin` it returns in `out`.
pub(super) async fn window_read<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    at: &mut SpanPos,
    method: Sym<'v, '_>,
    size: usize,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let n = size.min(at.room());
    if n == 0 {
        strand.store(out, &[] as &[u8]);
        return Ok(());
    }
    realign(strand, target, at).await?;
    let res = read_bounded(strand, target, method, n, Slot::reborrow(&mut out)).await;
    let moved = res
        .is_ok()
        .then(|| out.as_bin(strand.vm()).map_or(0, |bytes| bytes.len()));
    at.advance(moved);
    res
}

/// Calls `target.<method>`, a `write` with `args` as its pieces or a `write_some` with
/// one, for a window at `at`, and stores the count in `out`.
///
/// Pieces that fit pass through as they are. Otherwise the bytes that fit before the
/// window's end are written as one `Bin`. Nothing fitting fails a `write` as a full
/// bounded output does.
pub(super) async fn window_write<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    at: &mut SpanPos,
    method: Sym<'v, 'a>,
    mut args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let some = method.tag() == sym::WRITE_SOME;
    let (len, data) = {
        let pieces = if some {
            vec![one_piece(strand, args.reborrow())?]
        } else {
            pieces(strand, args.reborrow())?
        };
        let len = pieces.iter().map(|piece| piece.len()).sum::<usize>();
        let room = at.room();
        if len <= room {
            (len, None)
        } else {
            let data = cut(&pieces, room).flatten().copied().collect::<Vec<_>>();
            (data.len(), Some(data))
        }
    };
    if len == 0 {
        if data.is_some() && !some {
            return Err(write_zero(strand));
        }
        strand.store(out, 0);
        return Ok(());
    }
    realign(strand, target, at).await?;
    let res = match data {
        None => {
            target
                .op_mcall(strand, method, args, Slot::reborrow(&mut out))
                .await
        }
        Some(data) => with_slots!(strand, |mut bin| {
            strand.store(&mut bin, data.as_slice());
            method!(strand, target, method, &bin, &mut out)
        }),
    };
    let res = match res {
        Ok(()) => written(strand, method, &out, len),
        Err(err) => Err(err),
    };
    at.advance(res.as_ref().ok().copied());
    strand.store(out, res?);
    Ok(())
}

/// Moves a window at `at` over `target`, returning the new position. Only an end seek on
/// an open window calls the stream, to measure it.
pub(super) async fn window_seek<'v, 's>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    at: &mut SpanPos,
    pos: SeekFrom,
) -> Result<'v, 's, u64> {
    let end = if at.needs_end(pos) {
        Some(seek(strand, target, SeekFrom::End(0)).await?)
    } else {
        None
    };
    at.seek(pos, end).into_do_io(strand)
}
