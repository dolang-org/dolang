//! `io.Io`, the abstract type of byte streams.
//!
//! `Io` is a `Sink` of its bytes, so it answers the `Sink` methods as `Sink` does.
//!
//! Native providers inherit it as a supertype and define its members themselves. Members a
//! type does not define fall through to `Io`: the capabilities report `false`, and the
//! stream methods raise as unsupported.
//!
//! A stream implements the primitives, `read_some` and `write_some`, which are best-effort.
//! `Io` provides the complete operations in terms of them: `read` calls `read_some` until it
//! has the size asked for or reaches the end, and `write` calls `write_some` until every
//! piece is written. A stream overrides them where it can do better. The helpers
//! (`read_all`, `chunks`, `(put)`, ...) call the stream's own methods through the delegator,
//! so they keep no state and need no buffer.
//!
//! A Do subclass's instance provides [`ByteIo`] by lending [`DoIo`]. Getting its handle
//! starts an owner strand that runs a server strand for each of the handle's
//! [lanes](crate::io::bridge::Lanes). A server calls the stream's `read_some`, `write_some`
//! and `seek` for the handle's requests, which map one-to-one onto `AsyncRead::poll_read`
//! and `AsyncWrite::poll_write`, and its `flush` when the handle flushes or closes. Once every lane is closed, the owner reports to the handle
//! that the servers stopped. The stream stays open.
//!
//! The owner holds the [`Root`] of its own strand handle, which keeps it alive until the
//! handle's end of the lanes shuts down. The handle's holder is the consumer, whatever
//! strand got it, so the owner runs under an interrupt of its own: closing the handle
//! stops the servers once they are idle, and dropping it interrupts them, even in the
//! middle of a stream method. An exception from a stream method reaches the handle as an [`std::io::Error`]
//! made by [`from_do`](crate::io::from_do).
//!
//! A forkable stream's `fork` is served like the other methods. The handle of the stream
//! it returns has its own owner, so it outlives this stream's handle.
//!
//! [`DoIo`] also tells a consumer called from Do that it would bridge, so `io.Window` and
//! `io.Buffered` call the stream's methods directly instead, through the helpers here, and
//! its exceptions reach them unchanged.

use std::{
    cell::Cell,
    io::{self as std_io, SeekFrom},
    ops::ControlFlow,
    pin::pin,
    rc::Rc,
};

use futures::{
    StreamExt as _,
    channel::mpsc,
    future::{Either, join_all, select},
};

use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    io::{
        ByteHandle, ByteIo, IntoByteIo, IoCaps, IoErrorExt as _,
        bridge::{self, Lanes, Reply},
        from_do,
    },
    method,
    object::{
        BoundMethod,
        interface::Request,
        iter::{Surface, classify, iter_get, iter_mcall, sink_get, sink_mcall, spread_iter},
        protocol::{
            Inspect, Protocol, Recv, Spread, SpreadContext, abstract_call, abstract_init,
            is_special_mcall, members, recv_special_mcall,
        },
    },
    strand::{InterruptMask, Strand},
    sym::{self, Sym},
    unpack,
    value::{
        BinEmbryo, Input, Root, Slot, TypeObject, Value, fmt::Format, private::Sealed,
        with_u8_slice,
    },
    vm::{AllocExt, Vm},
    with_slots,
};

/// Bytes `read_all` requests per call, and the default `chunks` size.
const CHUNK: usize = 65536;

pub(crate) struct Io;

/// The iterator `chunks` returns. `done` is a `Cell`, so the iterator is immutable.
pub(crate) struct Chunks<'v> {
    stream: Value<'v>,
    size: usize,
    done: Cell<bool>,
}

unsafe impl Collect for Io {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Io {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Sink).unwrap_or(false)
            || strand
                .singletons()
                .sinkable
                .eq(strand, supertype)
                .unwrap_or(false)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type io.Io>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            is_final: false,
            members: members![
                Getter(sym::READABLE),
                Getter(sym::WRITABLE),
                Getter(sym::SEEKABLE),
                Getter(sym::FORKABLE),
                Method(sym::READ_SOME),
                Method(sym::WRITE_SOME),
                Method(sym::READ),
                Method(sym::WRITE),
                Method(sym::SEEK),
                Method(sym::FORK),
                Method(sym::CLOSE),
                Method(sym::FLUSH),
                Method(sym::READ_ALL),
                Method(sym::READ_ALL_STR),
                Method(sym::CHUNKS),
                Method(sym::IO),
                Method(sym::SINK_METHOD),
                Method(sym::PUT_METHOD),
                Method(sym::SINK),
                Method(sym::PUT),
                Method(sym::PREMAP),
                Method(sym::PREFILTER),
                Method(sym::PRECHOMP),
                Method(sym::PRECRIMP),
            ],
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
            ],
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::INIT_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            // A capability the subclass does not declare is absent.
            sym::READABLE | sym::WRITABLE | sym::SEEKABLE | sym::FORKABLE
                if this.delegator().is_some() =>
            {
                strand.store(out, false);
                Ok(())
            }
            _ => match this.delegator() {
                Some(stream) => sink_get(strand, stream, field, out),
                None => sink_get(strand, &this, field, out),
            },
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            return abstract_init(strand, &this, args);
        }
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, &this, method, args, out).await;
        }
        // `Sink` methods, as `Sink` answers them.
        if classify(method.tag()) == Some(Surface::Sinkable) {
            return match this.delegator() {
                Some(stream) => sink_mcall(strand, stream, method, args, out).await,
                None => sink_mcall(strand, &this, method, args, out).await,
            };
        }

        if let Some(delegator) = this.delegator() {
            args.prepend_self(strand, delegator);
        }
        helper_mcall(strand, method, args, out).await
    }

    /// A Do subclass's instance provides [`ByteIo`] by calling its own methods.
    fn op_provide<'a, 's>(
        this: Recv<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        match this.delegator() {
            Some(stream) => request.provide::<ByteIo>(&mut DoIo { stream, caps: None }),
            None => Ok(()),
        }
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let Some(stream) = this.delegator() else {
            return Err(Error::type_error(strand, "sink protocol not supported"));
        };
        strand.store(out, stream);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let Some(stream) = this.delegator() else {
            return Err(Error::type_error(strand, "sink protocol not supported"));
        };
        put(strand, stream, &item).await
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "io.Io"))
    }
}

/// Calls an `Io` helper on the stream that `args` start with. A stream method the stream
/// does not define is unsupported, and any other method raises as a missing field.
async fn helper_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    method: Sym<'v, 'a>,
    mut args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match method.tag() {
        sym::READ_ALL => {
            unpack!(strand, args => stream);
            let mut buf = BinEmbryo::new();
            read_to_end(strand, &stream, &mut buf).await?;
            buf.finish(strand, out);
            Ok(())
        }
        sym::READ_ALL_STR => {
            unpack!(strand, args => stream);
            let mut buf = BinEmbryo::new();
            read_to_end(strand, &stream, &mut buf).await?;
            if str::from_utf8(buf.as_slice()).is_err() {
                return Err(Error::data(strand, "invalid UTF-8 data"));
            }
            // SAFETY: validated above.
            unsafe { buf.finish_str_unchecked(strand, out) };
            Ok(())
        }
        sym::READ => {
            unpack!(strand, args => stream, size);
            let size = size
                .to_i64(strand)
                .ok()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| Error::type_error(strand, "size must be a non-negative integer"))?;
            let mut buf = BinEmbryo::new();
            read_exact(strand, &stream, size, &mut buf).await?;
            buf.finish(strand, out);
            Ok(())
        }
        sym::WRITE => {
            let stream = match args.next() {
                Some(Arg::Pos(stream)) => stream,
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
                None => return Err(Error::missing_positional(strand, 0)),
            };
            let total = write_pieces(strand, &stream, args).await?;
            strand.store(out, total);
            Ok(())
        }
        sym::CHUNKS => {
            unpack!(strand, args => stream, ?size);
            let size = match size {
                Some(size) => size
                    .to_i64(strand)
                    .ok()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| Error::type_error(strand, "size must be a positive integer"))?,
                None => CHUNK,
            };
            let embryo = strand.builtin_types().io_chunks.embryo(strand);
            // SAFETY: the iterator is allocated; the stream is copied straight into it
            let stream = unsafe { stream.dup() };
            embryo.finish_imm(
                strand,
                Chunks {
                    stream,
                    size,
                    done: Cell::new(false),
                },
                out,
            );
            Ok(())
        }
        // A stream is its own sink and its own byte view.
        sym::SINK_METHOD | sym::IO => {
            unpack!(strand, args => stream);
            strand.store(out, &stream);
            Ok(())
        }
        sym::PUT_METHOD => {
            unpack!(strand, args => stream, value);
            put(strand, &stream, &value).await
        }
        // Nothing is held back unless the stream overrides it.
        sym::FLUSH => {
            unpack!(strand, args => _stream);
            Ok(())
        }
        // The stream methods are the subclass's to define.
        sym::READ_SOME | sym::WRITE_SOME | sym::SEEK | sym::FORK | sym::CLOSE => {
            Err(Error::unsupported(strand, "abstract stream method"))
        }
        _ => Err(Error::field(strand, method)),
    }
}

/// Answers `Io`'s inherited members for a native receiver that forwards to `Io` by hand:
/// the `Sink` methods and the helpers. Any other method raises as [`helper_mcall`] does.
pub(crate) async fn recv_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    mut args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if classify(method.tag()) == Some(Surface::Sinkable) {
        return sink_mcall(strand, rcvr, method, args, out).await;
    }
    args.prepend_self(strand, rcvr);
    helper_mcall(strand, method, args, out).await
}

/// Binds `Io`'s inherited methods for a native receiver that forwards to `Io` by hand.
pub(crate) fn recv_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::READ
        | sym::WRITE
        | sym::FLUSH
        | sym::READ_ALL
        | sym::READ_ALL_STR
        | sym::CHUNKS
        | sym::IO => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => sink_get(strand, rcvr, field, out),
    }
}

/// Provides [`ByteIo`] for a native receiver with `caps` through the bridge, whose strands
/// call its own `read_some`, `write_some` and `seek`, as for a Do subclass.
pub(crate) fn provide_bridge<'v, 's>(
    request: &mut Request<'v, 's, '_>,
    stream: &Value<'v>,
    caps: IoCaps,
) -> Result<'v, 's, ()> {
    request.provide::<ByteIo>(&mut DoIo {
        stream,
        caps: Some(caps),
    })
}

/// Appends the rest of `stream` to `buf`, calling its `read_some` until it returns `b""`.
async fn read_to_end<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    buf: &mut BinEmbryo<'v>,
) -> Result<'v, 's, ()> {
    let read_some = Sym::well_known(sym::READ_SOME);
    with_slots!(strand, |mut chunk| {
        while read_chunk(strand, stream, read_some, CHUNK, Slot::reborrow(&mut chunk)).await? {
            let bytes = chunk
                .as_bin(strand.vm())
                .expect("read_chunk checks for `Bin`");
            let bytes = bytes.pin();
            buf.extend(strand, &bytes);
        }
        Ok(())
    })
}

/// Appends `size` bytes of `stream` to `buf`, calling its `read_some` until it has them or
/// reaches the end.
async fn read_exact<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    size: usize,
    buf: &mut BinEmbryo<'v>,
) -> Result<'v, 's, ()> {
    let read_some = Sym::well_known(sym::READ_SOME);
    with_slots!(strand, |mut chunk| {
        let mut left = size;
        while left > 0 {
            let want = left.min(CHUNK);
            if !read_chunk(strand, stream, read_some, want, Slot::reborrow(&mut chunk)).await? {
                break;
            }
            let bytes = chunk
                .as_bin(strand.vm())
                .expect("read_chunk checks for `Bin`");
            let bytes = bytes.pin();
            if bytes.len() > want {
                return Err(Error::runtime(strand, "read_some: read more than `size`"));
            }
            left -= bytes.len();
            buf.extend(strand, &bytes);
        }
        Ok(())
    })
}

/// Calls `stream.<method> size`, a `read` or `read_some`, into `out`, returning whether it
/// read anything.
async fn read_chunk<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    method: Sym<'v, '_>,
    size: usize,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    method!(strand, stream, method, size as i64, &mut out)?;
    match out.as_bin(strand.vm()) {
        Some(bytes) => Ok(!bytes.is_empty()),
        None => {
            let message = format!("{}: expected `Bin`", method.as_str(strand.vm()));
            Err(Error::type_error(strand, message))
        }
    }
}

/// Calls `stream.<method> size`, a `read` or `read_some`, into `out`, and checks that it
/// returned a `Bin` of at most `size` bytes.
pub(crate) async fn read_bounded<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    method: Sym<'v, '_>,
    size: usize,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    read_chunk(strand, stream, method, size, Slot::reborrow(&mut out)).await?;
    let len = out
        .as_bin(strand.vm())
        .expect("read_chunk checks for `Bin`")
        .len();
    if len > size {
        let message = format!("{}: read more than `size`", method.as_str(strand.vm()));
        return Err(Error::runtime(strand, message));
    }
    Ok(())
}

/// Writes the pieces in `args` with `stream`'s `write_some`, one piece or the rest of one
/// at a time, and returns the total written.
///
/// A `write_some` that writes nothing ends the write: after some progress, the total so far
/// is returned, since a bounded output is full; before any, it raises as
/// [`std::io::ErrorKind::WriteZero`].
async fn write_pieces<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    mut args: Args<'v, '_>,
) -> Result<'v, 's, usize> {
    for arg in args.reborrow() {
        match arg {
            Arg::Pos(piece) => {
                piece_len(strand, &piece)?;
            }
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        }
    }
    let write_some = Sym::well_known(sym::WRITE_SOME);
    with_slots!(strand, |mut rest, mut count| {
        let mut total = 0;
        for arg in args {
            let Arg::Pos(piece) = arg else { continue };
            let len = piece_len(strand, &piece)?;
            let mut offset = 0;
            while offset < len {
                if offset == 0 {
                    method!(strand, stream, write_some, &piece, &mut count)?;
                } else {
                    with_u8_slice!(strand, piece, |bytes| {
                        strand.store(&mut rest, &bytes[offset..])
                    });
                    method!(strand, stream, write_some, &rest, &mut count)?;
                }
                let n = written(strand, write_some, &count, len - offset)?;
                if n == 0 {
                    if total == 0 {
                        return Err(
                            std_io::Error::from(std_io::ErrorKind::WriteZero).into_do_io(strand)
                        );
                    }
                    return Ok(total);
                }
                offset += n;
                total += n;
            }
        }
        Ok(total)
    })
}

/// The length of a `write` piece, which must be a `Str` or `Bin`.
fn piece_len<'v, 's>(strand: &mut Strand<'v, 's>, piece: &Value<'v>) -> Result<'v, 's, usize> {
    with_u8_slice!(strand, piece, |bytes| bytes.len())
        .ok_or_else(|| Error::type_error(strand, "expected `Str` or `Bin`"))
}

/// Checks the count a `write_some` or `write` of `len` bytes returned: no more than `len`.
/// Zero means a bounded output is full.
pub(crate) fn written<'v, 's>(
    strand: &mut Strand<'v, 's>,
    method: Sym<'v, '_>,
    count: &Value<'v>,
    len: usize,
) -> Result<'v, 's, usize> {
    count
        .to_i64(strand)
        .ok()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n <= len)
        .ok_or_else(|| {
            let message = format!(
                "{}: expected a count of bytes written",
                method.as_str(strand.vm())
            );
            Error::runtime(strand, message)
        })
}

/// Writes `value` with one call of the stream's `write`, as `io.Buffered.(put)` does: a
/// `Str` or `Bin` as its bytes, anything else as its string form. A short count there is
/// the end of a bounded output, so it is not retried.
async fn put<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    let write = Sym::well_known(sym::WRITE);
    with_slots!(strand, |mut data, mut count| {
        if let Some(len) = with_u8_slice!(strand, value, |bytes| bytes.len()) {
            if len > 0 {
                method!(strand, stream, write, value, &mut count)?;
            }
            return Ok(());
        }
        let text = value.to_string(strand)?;
        if !text.is_empty() {
            strand.store(&mut data, text.as_str());
            method!(strand, stream, write, &data, &mut count)?;
        }
        Ok(())
    })
}

unsafe impl<'v> crate::gc::ImmState for Chunks<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Chunks<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.stream.accept(visit)
    }
}

pub(crate) type ChunksObj<'v> = crate::gc::Imm<Chunks<'v>>;

impl<'v> Protocol<'v> for ChunksObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<io.Chunks>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let chunks = this.get();
        if chunks.done.get() {
            return Ok(false);
        }
        let read = Sym::well_known(sym::READ);
        let more = read_chunk(strand, &chunks.stream, read, chunks.size, out).await?;
        chunks.done.set(!more);
        Ok(more)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter_mcall(strand, &this, method, args, out).await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
    }
}

/// Lends [`IntoByteIo`] for a Do stream, borrowing it for the query.
struct DoIo<'a, 'v> {
    stream: &'a Value<'v>,
    caps: Option<IoCaps>,
}

impl<'v> IntoByteIo<'v> for DoIo<'_, 'v> {
    fn caps<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        if let Some(caps) = self.caps {
            return Ok(caps);
        }
        let mut caps = IoCaps::empty();
        for (field, cap) in [
            (sym::READABLE, IoCaps::READ),
            (sym::WRITABLE, IoCaps::WRITE),
            (sym::SEEKABLE, IoCaps::SEEK),
            (sym::FORKABLE, IoCaps::FORK),
        ] {
            let set = with_slots!(strand, |mut value| {
                self.stream
                    .get(strand, Sym::well_known(field), &mut value)?;
                value.to_bool(strand)
            })?;
            caps.set(cap, set);
        }
        // Only a seekable stream forks.
        if !caps.contains(IoCaps::SEEK) {
            caps.remove(IoCaps::FORK);
        }
        self.caps = Some(caps);
        Ok(caps)
    }

    /// Starts the owner strand, which serves the handle's lanes.
    fn served_by_methods(&self, _: Sealed) -> bool {
        true
    }

    fn get<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        let caps = self.caps(strand)?;
        if !caps.intersects(IoCaps::READ | IoCaps::WRITE) {
            return Err(Error::value(strand, "stream must be readable or writable"));
        }
        let (handle, lanes) = bridge::handle(caps);
        // The owner keeps its own strand handle, which the spawn writes only after creating
        // it.
        let keep = Rc::new(Cell::new(None::<Root<'v>>));
        let mut root = Root::new(strand);
        strand.spawn_background(self.stream, None, &mut root, {
            let keep = keep.clone();
            async move |strand, stream, _out| {
                let _root = keep.take();
                own(strand, stream, lanes).await;
                Ok(())
            }
        })?;
        keep.set(Some(root));
        Ok(handle)
    }
}

/// Serves `lanes` until they close, then reports that the servers stopped. If the handle
/// is dropped first, interrupts the servers.
async fn own<'v>(strand: &mut Strand<'v, '_>, stream: &Value<'v>, lanes: Lanes) {
    let Lanes {
        servers,
        done,
        dropped,
    } = lanes;
    let interrupt = strand.interrupt_token().nested(InterruptMask::empty());
    let servers = servers
        .into_iter()
        .map(|lane| {
            strand.spawn_scoped(Some(interrupt.clone()), async move |strand| {
                serve(strand, stream, lane).await;
                Ok(())
            })
        })
        .collect::<Vec<_>>();
    let servers = pin!(join_all(servers));
    if let Either::Left((_, servers)) = select(dropped, servers).await {
        interrupt.cancel();
        servers.await;
    }
    let _ = done.send(Ok(()));
}

/// Serves the requests on `lane` until it closes or the strand is interrupted.
async fn serve<'v>(
    strand: &mut Strand<'v, '_>,
    stream: &Value<'v>,
    mut lane: mpsc::UnboundedReceiver<bridge::Request>,
) {
    while let Ok(Some(request)) = strand
        .interrupt_guard(async |_| Ok(lane.next().await))
        .await
    {
        match request {
            bridge::Request::Read(size, tx) => {
                let res = serve_read(strand, stream, size).await;
                reply(strand, tx, res);
            }
            bridge::Request::Write(data, tx) => {
                let res = serve_write(strand, stream, &data).await;
                reply(strand, tx, res);
            }
            bridge::Request::Seek(pos, tx) => {
                let res = seek(strand, stream, pos).await;
                reply(strand, tx, res);
            }
            bridge::Request::Fork(offset, tx) => {
                let res = serve_fork(strand, stream, offset).await;
                reply(strand, tx, res);
            }
            bridge::Request::Flush(tx) => {
                let res = serve_flush(strand, stream).await;
                reply(strand, tx, res);
            }
        }
    }
}

/// Sends `res` to the handle, an exception as an [`std::io::Error`].
fn reply<'v, 's, T>(strand: &mut Strand<'v, 's>, tx: Reply<T>, res: Result<'v, 's, T>) {
    let res = res.map_err(|err| from_do(strand, err));
    let _ = tx.send(res);
}

async fn serve_read<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    size: usize,
) -> Result<'v, 's, Vec<u8>> {
    with_slots!(strand, |mut chunk| {
        let read_some = Sym::well_known(sym::READ_SOME);
        read_bounded(strand, stream, read_some, size, Slot::reborrow(&mut chunk)).await?;
        let bytes = chunk
            .as_bin(strand.vm())
            .expect("read_chunk checks for `Bin`");
        Ok(bytes.pin().to_vec())
    })
}

/// Calls the stream's `flush`.
async fn serve_flush<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |mut out| {
        method!(strand, stream, Sym::well_known(sym::FLUSH), &mut out)
    })
}

/// Calls the stream's `fork` and gets the handle of the stream it returns.
async fn serve_fork<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    offset: u64,
) -> Result<'v, 's, ByteHandle> {
    let offset = i64::try_from(offset).map_err(|_| Error::overflow(strand))?;
    with_slots!(strand, |mut fork| {
        method!(
            strand,
            stream,
            Sym::well_known(sym::FORK),
            offset,
            &mut fork
        )?;
        crate::io::get(strand, &fork, |strand, caps| {
            if caps.intersects(IoCaps::READ | IoCaps::WRITE) {
                Ok(())
            } else {
                Err(Error::value(strand, "fork must be readable or writable"))
            }
        })
        .map_err(|err| {
            if err.kind() == crate::error::ErrorKind::Unsupported {
                Error::type_error(strand, "fork: expected `io.Io`")
            } else {
                err
            }
        })
    })
}

async fn serve_write<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    data: &[u8],
) -> Result<'v, 's, usize> {
    with_slots!(strand, |mut bin, mut count| {
        strand.store(&mut bin, data);
        let write_some = Sym::well_known(sym::WRITE_SOME);
        method!(strand, stream, write_some, &bin, &mut count)?;
        written(strand, write_some, &count, data.len())
    })
}

/// Calls `stream.seek` for `pos` and returns the position it reports.
pub(crate) async fn seek<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    pos: SeekFrom,
) -> Result<'v, 's, u64> {
    let method = Sym::well_known(sym::SEEK);
    with_slots!(strand, |mut out| {
        match pos {
            SeekFrom::Start(n) => {
                let n = i64::try_from(n).map_err(|_| Error::overflow(strand))?;
                let start = Sym::well_known(sym::START);
                method!(strand, stream, method, start: n, &mut out)?;
            }
            SeekFrom::Current(n) => method!(strand, stream, method, n, &mut out)?,
            SeekFrom::End(n) => {
                let end = Sym::well_known(sym::END);
                method!(strand, stream, method, end: n, &mut out)?;
            }
        }
        out.to_i64(strand)
            .ok()
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| Error::type_error(strand, "seek: expected a position"))
    })
}
