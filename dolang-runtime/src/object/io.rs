//! `io.Io`, the abstract type of byte streams.
//!
//! `Io` is a `Sink` of its bytes, so it answers the `Sink` methods as `Sink` does.
//!
//! Native providers inherit it as a supertype and define its members themselves. Members a
//! type does not define fall through to `Io`: the capabilities report `false`, and the
//! stream methods raise as a missing field would.
//!
//! The helpers (`read_all`, `write_all`, `chunks`, `(put)`, ...) call the stream's own
//! methods through the delegator, so they keep no state and need no buffer.
//!
//! A Do subclass's instance provides [`ByteIo`] by lending [`DoIo`]. Getting its handle
//! starts an owner strand that runs a server strand for each of the handle's
//! [lanes](crate::io::bridge::Lanes). A server calls the stream's `read`, `write` and
//! `seek` for the handle's requests. Once every lane is closed, the owner reports to the
//! handle that the servers stopped. The stream stays open.
//!
//! The owner holds the [`Root`] of its own strand handle, which keeps it alive until the
//! handle's end of the lanes shuts down. It runs under an interrupt nested from the
//! consumer's, so cancelling the consumer, or shutting down the VM, interrupts the
//! servers. An exception from a stream method reaches the handle as an [`std::io::Error`]
//! made by [`from_do`](crate::io::from_do).

use std::{
    cell::{Cell, UnsafeCell},
    io::SeekFrom,
    ops::ControlFlow,
    rc::Rc,
};

use futures::{StreamExt as _, channel::mpsc, future::join_all};

use crate::{
    arg::{Arg, Args, OwnedItem},
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    io::{
        ByteHandle, ByteIo, IntoByteIo, IoCaps,
        bridge::{self, Lanes, Reply},
        from_do,
    },
    method,
    object::{
        BoundMethod,
        interface::Request,
        iter::{Surface, classify, iter_get, iter_mcall, sink_get, sink_mcall, spread_iter},
        protocol::{
            Inspect, Protocol, Recv, Spread, SpreadContext, is_special_mcall, members,
            recv_special_mcall,
        },
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{BinEmbryo, Input, Output, Root, Slot, TypeObject, Value, fmt::Format},
    vm::Vm,
};

/// Bytes `read_all` and `copy_to` request per call, and the default `chunks` size.
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
    const IMMUTABLE: bool = true;
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
        Output::set(strand, out, &strand.singletons().type_obj)
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
                Method(sym::READ),
                Method(sym::WRITE),
                Method(sym::SEEK),
                Method(sym::CLOSE),
                Method(sym::READ_ALL),
                Method(sym::READ_ALL_STR),
                Method(sym::WRITE_ALL),
                Method(sym::COPY_TO),
                Method(sym::CHUNKS),
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
            sym::READABLE | sym::WRITABLE | sym::SEEKABLE if this.delegator().is_some() => {
                Output::set(strand, out, false);
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
            args.prepend_self(delegator.dup());
        }
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            _ => helper_mcall(strand, method, args, out).await,
        }
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
        Output::set(strand, out, stream);
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
}

/// Calls an `Io` helper on the stream that `args` start with. Any other method is the
/// stream's own to define, so it raises as a missing field.
async fn helper_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    method: Sym<'v, 'a>,
    mut args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match method.tag() {
        sym::READ_ALL => {
            let ([stream], []) = unpack!(strand, args, 1, 0)?;
            let mut buf = BinEmbryo::new();
            read_to_end(strand, &stream, &mut buf).await?;
            buf.finish(strand, out);
            Ok(())
        }
        sym::READ_ALL_STR => {
            let ([stream], []) = unpack!(strand, args, 1, 0)?;
            let mut buf = BinEmbryo::new();
            read_to_end(strand, &stream, &mut buf).await?;
            if str::from_utf8(buf.as_slice()).is_err() {
                return Err(Error::runtime(strand, "invalid UTF-8 data"));
            }
            // SAFETY: validated above.
            unsafe { buf.finish_str_unchecked(strand, out) };
            Ok(())
        }
        sym::WRITE_ALL => {
            let stream = match args.next() {
                Some(Arg::Pos(stream)) => stream,
                Some(Arg::Key(key, _)) => return Err(Error::unexpected_key(strand, key)),
                None => return Err(Error::missing_positional(strand, 0)),
            };
            write_all(strand, &stream, args).await
        }
        sym::COPY_TO => {
            let ([stream, dest], []) = unpack!(strand, args, 2, 0)?;
            copy_to(strand, &stream, &dest, out).await
        }
        sym::CHUNKS => {
            let ([stream], [size]) = unpack!(strand, args, 1, 1)?;
            let size = match size {
                Some(size) => size
                    .to_i64(strand)
                    .ok()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| Error::type_error(strand, "size must be a positive integer"))?,
                None => CHUNK,
            };
            let stream = stream.dup();
            strand.builtin_types().io_chunks.create(
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
        sym::SINK_METHOD => {
            let ([stream], []) = unpack!(strand, args, 1, 0)?;
            Output::set(strand, out, &stream);
            Ok(())
        }
        sym::PUT_METHOD => {
            let ([stream, value], []) = unpack!(strand, args, 2, 0)?;
            put(strand, &stream, &value).await
        }
        // The stream methods are the subclass's to define.
        _ => Err(Error::field(strand, method)),
    }
}

/// Answers `Io`'s inherited members for a native receiver that forwards to `Io` by hand:
/// the `Sink` methods and the helpers. Any other method raises as a missing field.
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
    args.prepend_self(Value::from_input(strand.vm(), rcvr));
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
        sym::READ_ALL | sym::READ_ALL_STR | sym::WRITE_ALL | sym::COPY_TO | sym::CHUNKS => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => sink_get(strand, rcvr, field, out),
    }
}

/// Provides [`ByteIo`] for a native receiver with `caps` through the bridge, whose strands
/// call its own `read`, `write` and `seek`, as for a Do subclass.
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

/// Appends the rest of `stream` to `buf`, calling its `read` until it returns `b""`.
async fn read_to_end<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    buf: &mut BinEmbryo<'v>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut chunk]| {
            while read_chunk(strand, stream, CHUNK, Slot::reborrow(&mut chunk)).await? {
                let bytes = chunk
                    .as_bin(strand.vm())
                    .expect("read_chunk checks for `Bin`");
                let bytes = bytes.pin();
                buf.extend(strand, &bytes);
            }
            Ok(())
        })
        .await
}

/// Calls `stream.read size` into `out`, returning whether it read anything.
async fn read_chunk<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    size: usize,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, bool> {
    method!(
        strand,
        stream,
        Sym::well_known(sym::READ),
        &mut out,
        size as i64
    )
    .await?;
    match out.as_bin(strand.vm()) {
        Some(bytes) => Ok(!bytes.is_empty()),
        None => Err(Error::type_error(strand, "read: expected `Bin`")),
    }
}

/// Writes all of the pieces in `args` with `stream`'s `write`.
///
/// The first `write` gets every piece, so a stream that writes them at once keeps them
/// together. It gets copies of the arguments, since a call takes its arguments, and `args`
/// still holds them afterward for what a short write leaves, which is joined into one `Bin`.
async fn write_all<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    mut args: Args<'v, '_>,
) -> Result<'v, 's, ()> {
    let mut items: Vec<OwnedItem<'v>> = Vec::with_capacity(args.len() + 1);
    // Headroom for the receiver.
    items.push((None, UnsafeCell::new(Value::NIL)));
    let mut len = 0;
    for arg in args.reborrow() {
        let piece = match arg {
            Arg::Pos(piece) => piece,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };
        len += piece_len(strand, &piece)?;
        items.push((None, UnsafeCell::new(piece.dup())));
    }
    if len == 0 {
        return Ok(());
    }
    strand
        .with_slots(async move |strand, [mut count, mut rest]| {
            let write = Sym::well_known(sym::WRITE);
            stream
                .method(strand, write, Args::new_owned(&items, 1), &mut count)
                .await?;
            let mut skip = written(strand, &count, len)?;
            if skip == len {
                return Ok(());
            }
            let mut left = BinEmbryo::new_with_capacity(strand, len - skip);
            for arg in args {
                let Arg::Pos(piece) = arg else { continue };
                let Some(bytes) = piece.as_u8_slice_raw(strand.vm()) else {
                    continue;
                };
                if skip >= bytes.len() {
                    skip -= bytes.len();
                    continue;
                }
                left.extend(strand, &bytes[skip..]);
                skip = 0;
            }
            left.finish(strand, &mut rest);
            write_all_value(strand, stream, &rest).await
        })
        .await
}

/// The length of a `write` piece, which must be a `Str` or `Bin`.
fn piece_len<'v, 's>(strand: &mut Strand<'v, 's>, piece: &Value<'v>) -> Result<'v, 's, usize> {
    match piece.as_u8_slice_raw(strand.vm()) {
        Some(bytes) => Ok(bytes.len()),
        None => Err(Error::type_error(strand, "expected `Str` or `Bin`")),
    }
}

/// Writes all of `data` with `stream`'s `write`, passing what a short write leaves as a
/// `Bin`.
async fn write_all_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    data: &Value<'v>,
) -> Result<'v, 's, ()> {
    let len = piece_len(strand, data)?;
    strand
        .with_slots(async move |strand, [mut rest, mut written_count]| {
            let write = Sym::well_known(sym::WRITE);
            let mut offset = 0;
            while offset < len {
                if offset == 0 {
                    method!(strand, stream, write, &mut written_count, data).await?;
                } else {
                    let Some(bytes) = data.as_u8_slice_raw(strand.vm()) else {
                        return Err(Error::type_error(strand, "expected `Str` or `Bin`"));
                    };
                    Output::set(strand, &mut rest, &bytes[offset..]);
                    method!(strand, stream, write, &mut written_count, &rest).await?;
                }
                offset += written(strand, &written_count, len - offset)?;
            }
            Ok(())
        })
        .await
}

/// Checks the count a `write` of `len` bytes returned: at least one, and no more than `len`.
fn written<'v, 's>(
    strand: &mut Strand<'v, 's>,
    count: &Value<'v>,
    len: usize,
) -> Result<'v, 's, usize> {
    count
        .to_i64(strand)
        .ok()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0 && n <= len)
        .ok_or_else(|| Error::runtime(strand, "write: expected a count of bytes written"))
}

/// Writes `value` as `io.Buffered.(put)` does: a `Str` or `Bin` as its bytes, anything
/// else as its string form.
async fn put<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    if value.as_bin(strand.vm()).is_some() || value.as_str(strand.vm()).is_some() {
        return write_all_value(strand, stream, value).await;
    }
    let text = value.to_string(strand)?;
    strand
        .with_slots(async move |strand, [mut data]| {
            Output::set(strand, &mut data, text.as_str());
            write_all_value(strand, stream, &data).await
        })
        .await
}

/// Puts the rest of `stream` into `dest`'s sink, returning the number of bytes copied.
async fn copy_to<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    dest: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let copied = strand
        .with_slots(async move |strand, [mut sink, mut chunk]| {
            dest.sink(strand, &mut sink).await?;
            let mut copied = 0;
            while read_chunk(strand, stream, CHUNK, Slot::reborrow(&mut chunk)).await? {
                if let Some(bytes) = chunk.as_bin(strand.vm()) {
                    copied += bytes.len();
                }
                sink.put(strand, Slot::reborrow(&mut chunk)).await?;
            }
            Ok(copied)
        })
        .await?;
    Output::set(strand, out, copied);
    Ok(())
}

unsafe impl<'v> Collect for Chunks<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.stream.accept(visit)
    }

    fn clear(&mut self) {
        self.stream.clear();
        self.done.set(true);
    }
}

impl<'v> Protocol<'v> for Chunks<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
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
        Output::set(strand, out, &this);
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
        let more = read_chunk(strand, &chunks.stream, chunks.size, out).await?;
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
        ] {
            let set = strand.with_slots_sync(|strand, [mut value]| {
                self.stream
                    .get(strand, Sym::well_known(field), &mut value)?;
                value.to_bool(strand)
            })?;
            caps.set(cap, set);
        }
        self.caps = Some(caps);
        Ok(caps)
    }

    /// Starts the owner strand, which serves the handle's lanes.
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
        let token = strand.interrupt_token();
        strand.spawn_background(self.stream, Some(&token), &mut root, {
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

/// Serves `lanes` until they close, then reports that the servers stopped.
async fn own<'v>(strand: &mut Strand<'v, '_>, stream: &Value<'v>, lanes: Lanes) {
    let Lanes { servers, done } = lanes;
    let servers = servers
        .into_iter()
        .map(|lane| {
            strand.spawn_scoped(None, async move |strand| {
                serve(strand, stream, lane).await;
                Ok(())
            })
        })
        .collect::<Vec<_>>();
    join_all(servers).await;
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
                let res = serve_seek(strand, stream, pos).await;
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
    strand
        .with_slots(async move |strand, [mut chunk]| {
            read_chunk(strand, stream, size, Slot::reborrow(&mut chunk)).await?;
            let bytes = chunk
                .as_bin(strand.vm())
                .expect("read_chunk checks for `Bin`");
            Ok(bytes.pin().to_vec())
        })
        .await
}

async fn serve_write<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    data: &[u8],
) -> Result<'v, 's, usize> {
    strand
        .with_slots(async move |strand, [mut bin, mut count]| {
            Output::set(strand, &mut bin, data);
            method!(
                strand,
                stream,
                Sym::well_known(sym::WRITE),
                &mut count,
                &bin
            )
            .await?;
            written(strand, &count, data.len())
        })
        .await
}

async fn serve_seek<'v, 's>(
    strand: &mut Strand<'v, 's>,
    stream: &Value<'v>,
    pos: SeekFrom,
) -> Result<'v, 's, u64> {
    let method = Sym::well_known(sym::SEEK);
    strand
        .with_slots(async move |strand, [mut out]| {
            match pos {
                SeekFrom::Start(n) => {
                    let n = i64::try_from(n).map_err(|_| Error::overflow(strand))?;
                    let start = Sym::well_known(sym::START);
                    method!(strand, stream, method, &mut out, start: n).await?;
                }
                SeekFrom::Current(n) => method!(strand, stream, method, &mut out, n).await?,
                SeekFrom::End(n) => {
                    let end = Sym::well_known(sym::END);
                    method!(strand, stream, method, &mut out, end: n).await?;
                }
            }
            out.to_i64(strand)
                .ok()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| Error::type_error(strand, "seek: expected a position"))
        })
        .await
}
