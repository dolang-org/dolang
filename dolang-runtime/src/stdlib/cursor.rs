//! `io.Cursor`, a seekable `io.Io` over in-memory data or a window of another stream.
//!
//! Slot 0 holds the target until the cursor is closed. Positions are relative to the start
//! of the cursor's window.
//!
//! A memory target, a `Str`, `Bin`, `BinBuf` or `StrBuf`, is read and written live, so the
//! cursor sees changes made to a buffer by other means. The cursor answers no
//! [`ByteIo`](crate::io::ByteIo) for it, since GC data can't leave the VM: `Io` serves it
//! through the cursor's own methods, as it does `io.Concat`. The bytes a `StrBuf` cursor
//! holds back therefore live here, not in a handle.
//!
//! A stream target is read through its handle, in a [`Window`] the cursor shares with the
//! handles it lends.

use std::{
    io::{self, IoSlice, SeekFrom},
    pin::Pin,
    task::{Context, Poll, ready},
};

use dolang_util::{shared::Locked, utf8};
use futures::io::{AsyncRead, AsyncSeek, AsyncSeekExt as _, AsyncWrite, AsyncWriteExt as _};

use super::io::{
    check_caps, close_shared, open_caps, pieces, read_spare, seek_arg, size_value, slices,
};
use crate::{
    BYTE_STREAM_CHUNK_SIZE,
    arg::Args,
    error::{Error, ErrorKind, Result},
    io::{ByteHandle, ByteIo, IntoByteIo, Io, IoCaps, IoResultExt as _, Prefixed, Shared},
    object::{
        interface::Request,
        native::{Instance, Mut, Object, Type, TypeBuilder},
    },
    strand::Strand,
    unpack,
    value::{BinEmbryo, Output, Slot, TypeObject, Value},
};

/// The kind of data a cursor reads and writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Str,
    Bin,
    BinBuf,
    StrBuf,
}

/// A cursor over a memory target.
struct Memory {
    target: Target,
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

impl Memory {
    /// Capabilities of the target, open or not.
    fn target_caps(&self) -> IoCaps {
        match self.target {
            Target::Str | Target::Bin => IoCaps::READ | IoCaps::SEEK,
            Target::BinBuf | Target::StrBuf => IoCaps::READ | IoCaps::WRITE | IoCaps::SEEK,
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
            Target::BinBuf => {
                let n = data.len().min(room);
                if n > 0 {
                    let buf = target
                        .downcast_native(strand, strand.builtin_types().binbuf)
                        .expect("target is a `BinBuf`");
                    let mut buf = buf.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
                    buf.overwrite(strand, at, &data[..n]);
                }
                self.pos += n as u64;
                Ok(n)
            }
            Target::StrBuf => self.write_str(strand, target, at, room, data),
            Target::Str | Target::Bin => unreachable!("an immutable target is not writable"),
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
            return Err(Error::value(strand, "invalid UTF-8 data"));
        };
        if !text.is_empty() {
            let buf = target
                .downcast_native(strand, strand.builtin_types().strbuf)
                .expect("target is a `StrBuf`");
            let mut buf = buf.borrow_mut().ok_or_else(|| Error::concurrency(strand))?;
            if !buf.overwrite(strand, at, text) {
                return Err(Error::value(strand, "write would split a character"));
            }
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

/// A seekable `io.Io` over a window of in-memory data or another stream.
pub(crate) struct Cursor {
    kind: Kind,
}

enum Kind {
    Memory(Memory),
    Stream {
        /// The window, until the cursor is closed.
        io: Option<Shared<Window<Prefixed<Io>>>>,
        caps: IoCaps,
    },
}

impl Cursor {
    /// Capabilities while open; a closed cursor has none.
    fn caps(&self) -> IoCaps {
        match &self.kind {
            Kind::Memory(memory) => memory.caps(),
            Kind::Stream { io, caps } => open_caps(io, *caps),
        }
    }

    /// Raises unless the cursor is open and has `need`.
    fn check<'v, 's>(&self, strand: &mut Strand<'v, 's>, need: IoCaps) -> Result<'v, 's, ()> {
        match &self.kind {
            Kind::Memory(memory) => memory.check(strand, need),
            Kind::Stream { io, caps } => check_caps(strand, "stream", io.is_some(), *caps, need),
        }
    }

    /// The memory cursor, after checking that the target is in memory.
    fn memory(&mut self) -> &mut Memory {
        match &mut self.kind {
            Kind::Memory(memory) => memory,
            Kind::Stream { .. } => unreachable!("the target is a stream"),
        }
    }

    /// Holds the window of a stream target for a method, or `None` for a memory target.
    async fn lock<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Option<Locked<'_, Window<Prefixed<Io>>>>> {
        match &mut self.kind {
            Kind::Memory(_) => Ok(None),
            Kind::Stream { io, .. } => {
                let io = io.as_mut().expect("stream is closed");
                io.lock().await.into_do_io(strand).map(Some)
            }
        }
    }

    /// Closes the cursor. A memory cursor drops held bytes and raises if a character was
    /// unfinished; a stream cursor flushes the window.
    async fn close<'v, 's>(
        this: &mut Mut<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        Mut::slot_mut::<0>(this).store(Value::NIL);
        match &mut this.kind {
            Kind::Memory(memory) => {
                let unfinished = !memory.held.is_empty();
                memory.open = false;
                memory.held = Vec::new();
                if unfinished {
                    return Err(Error::value(
                        strand,
                        "stream closed with an unfinished character",
                    ));
                }
                Ok(())
            }
            Kind::Stream { io, .. } => close_shared(io).await.into_do_io(strand),
        }
    }
}

impl<'v> IntoByteIo<'v> for Cursor {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(Cursor::caps(self))
    }

    /// Lends the window, so the handle shares the cursor's position.
    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        let Kind::Stream { io, caps } = &self.kind else {
            unreachable!("only a stream cursor lends a handle");
        };
        let io = io.as_ref().expect("stream is closed").clone();
        Ok(ByteHandle::with_caps(io, *caps))
    }
}

/// A window of a seekable stream, with a position of its own.
///
/// Each read or write first seeks the stream to the window's position, so the stream's own
/// position doesn't matter between operations. Another user of the stream must not move
/// it during one.
pub(crate) struct Window<B> {
    io: B,
    /// The window's absolute start.
    start: u64,
    /// The window's absolute end, when it is bounded.
    end: Option<u64>,
    /// Position within the window.
    pos: u64,
    /// Whether the stream sits at the position, so an operation that was pending doesn't
    /// seek again.
    aligned: bool,
}

impl<B: AsyncSeek + Unpin> Window<B> {
    fn new(io: B, start: u64, end: Option<u64>) -> Self {
        Self {
            io,
            start,
            end,
            pos: 0,
            aligned: false,
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

    /// Seeks the stream to the position, then runs `op` and advances past what it moved.
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
        self.aligned = false;
        if let Ok(n) = res {
            self.pos += n as u64;
        }
        Poll::Ready(res)
    }
}

impl<B: AsyncRead + AsyncSeek + Unpin> AsyncRead for Window<B> {
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
impl<B: AsyncWrite + AsyncSeek + Unpin> AsyncWrite for Window<B> {
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
impl<B: AsyncSeek + Unpin> AsyncSeek for Window<B> {
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
        this.pos = pos;
        Poll::Ready(Ok(pos))
    }
}

/// Calls `f` with the target's current bytes.
fn with_bytes<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    target: &Value<'v>,
    kind: Target,
    f: impl FnOnce(&mut Strand<'v, 's>, &[u8]) -> R,
) -> Result<'v, 's, R> {
    match kind {
        Target::Str => {
            let s = target.as_str(strand.vm()).expect("target is a `Str`").pin();
            Ok(f(strand, s.as_bytes()))
        }
        Target::Bin => {
            let b = target.as_bin(strand.vm()).expect("target is a `Bin`").pin();
            Ok(f(strand, &b))
        }
        Target::BinBuf => {
            let buf = target
                .downcast_native(strand, strand.builtin_types().binbuf)
                .expect("target is a `BinBuf`");
            let buf = buf.borrow().ok_or_else(|| Error::concurrency(strand))?;
            Ok(f(strand, buf.as_slice()))
        }
        Target::StrBuf => {
            let buf = target
                .downcast_native(strand, strand.builtin_types().strbuf)
                .expect("target is a `StrBuf`");
            let buf = buf.borrow().ok_or_else(|| Error::concurrency(strand))?;
            Ok(f(strand, buf.as_str().as_bytes()))
        }
    }
}

/// Parses the window: a `Range` of non-negative `Int`s with step 1, or the whole target.
fn window<'v, 's>(
    strand: &mut Strand<'v, 's>,
    range: Option<Slot<'v, '_>>,
) -> Result<'v, 's, (u64, Option<u64>)> {
    let Some(range) = range.filter(|range| !range.is_nil()) else {
        return Ok((0, None));
    };
    let Some(range) = range.downcast_ref(strand.builtin_types().range) else {
        return Err(Error::type_error(strand, "expected `Range` for `range`"));
    };
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
        let (start, end) = window(strand, range)?;
        let vm = strand.vm();
        // A `BinBuf` is also an `Io`, so memory targets come first.
        let memory = if target.as_str(vm).is_some() {
            Some(Target::Str)
        } else if target.as_bin(vm).is_some() {
            Some(Target::Bin)
        } else if target
            .downcast_native(vm, vm.builtin_types().binbuf)
            .is_some()
        {
            Some(Target::BinBuf)
        } else if target
            .downcast_native(vm, vm.builtin_types().strbuf)
            .is_some()
        {
            Some(Target::StrBuf)
        } else {
            None
        };
        let kind = match memory {
            Some(target) => Kind::Memory(Memory {
                target,
                start,
                end,
                pos: 0,
                held: Vec::new(),
                open: true,
            }),
            None => {
                let need = IoCaps::READ | IoCaps::SEEK;
                let handle = crate::io::get(strand, &target, |strand, caps| {
                    if caps.contains(need) {
                        Ok(())
                    } else {
                        Err(Error::value(strand, "target must be readable and seekable"))
                    }
                })
                .map_err(|err| {
                    if err.kind() == ErrorKind::Unsupported {
                        Error::type_error(
                            strand,
                            "expected `Str`, `Bin`, `BinBuf`, `StrBuf` or `io.Io` for `target`",
                        )
                    } else {
                        err
                    }
                })?;
                let ByteHandle { io, prefix } = handle;
                Kind::Stream {
                    caps: io.caps(),
                    io: Some(Shared::new(Window::new(
                        Prefixed::new(io, prefix),
                        start,
                        end,
                    ))),
                }
            }
        };
        this.create(strand, Cursor { kind }, &mut out);
        this.cast(&out).unwrap().enter_sync(strand, |strand, inst| {
            let mut borrow = inst.borrow_mut_unwrap();
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &target);
        });
        Ok(())
    }

    /// Lends the window of a stream target. A memory target answers nothing, so `Io` serves
    /// it through the cursor's methods.
    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            if let Kind::Stream { .. } = borrow.kind {
                borrow.check(request, IoCaps::empty())?;
                request.provide::<ByteIo>(&mut *borrow)?;
            }
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
                if let Some(mut io) = borrow.lock(strand).await? {
                    let mut buf = BinEmbryo::new();
                    let chunk = size.min(BYTE_STREAM_CHUNK_SIZE);
                    read_spare(&mut *io, strand, &mut buf, chunk)
                        .await
                        .into_do_io(strand)?;
                    buf.finish(strand, out);
                    return Ok(());
                }
                strand.with_slots_sync(|strand, [mut target]| {
                    Output::set(strand, &mut target, Mut::slot::<0>(&borrow));
                    borrow.memory().read(strand, &target, size, out)
                })
            })
            .method("write", async move |this, strand, args, out| {
                let pieces = pieces(strand, args)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::WRITE)?;
                let n = if let Some(mut io) = borrow.lock(strand).await? {
                    let (slices, _) = slices(&pieces);
                    io.write_vectored(&slices).await.into_do_io(strand)?
                } else {
                    let data = pieces
                        .iter()
                        .map(|piece| &**piece)
                        .collect::<Vec<_>>()
                        .concat();
                    strand.with_slots_sync(|strand, [mut target]| {
                        Output::set(strand, &mut target, Mut::slot::<0>(&borrow));
                        borrow.memory().write(strand, &target, &data)
                    })?
                };
                Output::set(strand, out, n);
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let pos = seek_arg(strand, offset, start, end, start_sym, end_sym)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::SEEK)?;
                let pos = if let Some(mut io) = borrow.lock(strand).await? {
                    io.seek(pos).await.into_do_io(strand)?
                } else {
                    strand.with_slots_sync(|strand, [mut target]| {
                        Output::set(strand, &mut target, Mut::slot::<0>(&borrow));
                        borrow.memory().seek(strand, &target, pos)
                    })?
                };
                Output::set(strand, out, pos);
                Ok(())
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check(strand, IoCaps::empty())?;
                if let Some(mut io) = borrow.lock(strand).await? {
                    io.flush().await.into_do_io(strand)?;
                }
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                Cursor::close(&mut borrow, strand).await
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

    use super::Window;

    #[test]
    fn bounded_window_cuts_reads_and_writes() {
        block_on(async {
            let mut io = Cursor::new(b"0123456789".to_vec());
            let mut window = Window::new(&mut io, 2, Some(6));
            let mut buf = [0; 8];
            assert_eq!(window.read(&mut buf).await.unwrap(), 4);
            assert_eq!(&buf[..4], b"2345");
            assert_eq!(window.read(&mut buf).await.unwrap(), 0);
            assert_eq!(window.seek(SeekFrom::End(-1)).await.unwrap(), 3);
            assert_eq!(window.seek(SeekFrom::Start(1)).await.unwrap(), 1);
            let bufs = [IoSlice::new(b"ab"), IoSlice::new(b"cd")];
            assert_eq!(window.write_vectored(&bufs).await.unwrap(), 3);
            assert_eq!(window.write(b"e").await.unwrap(), 0);
            assert!(window.seek(SeekFrom::Current(-5)).await.is_err());
            assert_eq!(io.get_ref(), b"012abc6789");
        });
    }

    #[test]
    fn open_window_seeks_from_stream_end() {
        block_on(async {
            let mut io = Cursor::new(b"0123456789".to_vec());
            let mut window = Window::new(&mut io, 7, None);
            assert_eq!(window.seek(SeekFrom::End(0)).await.unwrap(), 3);
            window.write_all(b"abc").await.unwrap();
            window.seek(SeekFrom::Start(0)).await.unwrap();
            let mut got = Vec::new();
            window.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, b"789abc");
        });
    }
}
