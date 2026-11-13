use std::{
    cell::RefCell,
    future,
    hash::{DefaultHasher, Hash},
    mem,
    ops::ControlFlow,
    ptr::NonNull,
    rc::Rc,
    task::Poll,
    task::Waker,
};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, ErrorPair, Result, UnwindEntry},
    gc::{Collect, arena::Visit},
    method,
    strand::{CellState, InterruptToken, RawInfo, Strand, StrandCell, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
    vm::Vm,
};

use super::{
    backtrace, iter,
    protocol::{GcObj, Protocol, Recv, instance_get_fallback, instance_mcall_fallback},
    tuple,
};

/// Result stored in a JoinHandle after a background strand completes.
pub(crate) enum Completion<'v> {
    Ok(Value<'v>),
    Err(ErrorPair<'v>),
}

/// GC-managed handle for a background strand (`strand.Task`).
pub(crate) struct Handle<'v> {
    pub(crate) cell: Rc<StrandCell>,
    /// Whether `accept` scans the strand's stack; cleared by `clear`.
    traced: bool,
    pub(crate) interrupt: InterruptToken<'v>,
    pub(crate) result: Option<Completion<'v>>,
    pub(crate) wakers: Vec<Waker>,
}

impl<'v> Handle<'v> {
    pub(crate) fn new(cell: Rc<StrandCell>, interrupt: InterruptToken<'v>) -> Self {
        Self {
            cell,
            traced: true,
            interrupt,
            result: None,
            wakers: Vec::new(),
        }
    }

    /// Store the result of the background strand and wake any joiner.
    pub(crate) fn complete(&mut self, result: Completion<'v>) {
        self.result = Some(result);
        for waker in self.wakers.drain(..) {
            waker.wake();
        }
    }
}

unsafe impl<'v> Collect for Handle<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    const STRAND: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        if let Some(Completion::Ok(v)) = &self.result {
            v.accept(visit)?;
        }
        if let Some(Completion::Err((v, backtrace))) = &self.result {
            v.accept(visit)?;
            for entry in backtrace {
                entry.accept(visit)?;
            }
        }

        // Scan the strand's stack (start value, frame chain, input/output).
        //
        // Until first poll the cell is unattached, so the queued `start` value
        // goes untraced.  Under refcounting that is conservative and therefore
        // sound; a tracing collector would have to root it until first poll.
        if self.traced
            && let Some(inner) = self.cell.get()
        {
            // Safety: the cell pointer is cleared before the strand goes away,
            // and collection never runs concurrently with the strand
            unsafe { inner.as_ref().scan_stack(visit)? };
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        // Stop tracing the strand and cancel it; it unwinds on its own.
        if mem::take(&mut self.traced) {
            self.interrupt.cancel();
        }
    }
}

impl<'v> Drop for Handle<'v> {
    fn drop(&mut self) {
        self.clear()
    }
}

async fn join_handle<'v, 's>(
    handle: &GcObj<'v, Handle<'v>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    // Borrow only while polling so the handle remains collectable across suspension.
    future::poll_fn(|cx| {
        let mut borrow = handle
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?;
        if borrow.result.is_some() {
            return Poll::Ready(Ok(()));
        }
        borrow.wakers.push(cx.waker().clone());
        Poll::Pending
    })
    .await?;
    let borrow = handle.borrow().ok_or_else(|| Error::concurrency(strand))?;
    match borrow.result.as_ref().unwrap() {
        Completion::Ok(v) => {
            Output::set(strand, out, v);
            Ok(())
        }
        Completion::Err(pair) => Err(Error::from_pair_ref(strand, pair)),
    }
}

async fn wait_handle<'v, 's>(
    handle: &GcObj<'v, Handle<'v>>,
    strand: &mut Strand<'v, 's>,
) -> Result<'v, 's, ()> {
    future::poll_fn(|cx| {
        let mut borrow = handle
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?;
        if borrow.result.is_some() {
            return Poll::Ready(Ok(()));
        }
        borrow.wakers.push(cx.waker().clone());
        Poll::Pending
    })
    .await
}

impl<'v> Protocol<'v> for Handle<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().task)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let id = this.borrow(strand)?.cell.id();
        crate::fmt!(strand, w, "<strand.Task {id}>")
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let cell = this.borrow(strand)?.cell.clone();
        cell_eq(strand, &cell, other)
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        cell_hash(&this.borrow(strand)?.cell, hasher);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::JOIN => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                join_handle(&this.to_strong(), strand, out).await
            }
            sym::CANCEL => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                borrow.interrupt.cancel();
                Ok(())
            }
            sym::WAIT => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                wait_handle(&this.to_strong(), strand).await
            }
            sym::DONE | sym::ID => Err(field_not_method(strand, method)),
            tag if is_observe_method(tag) => {
                let cell = this.borrow(strand)?.cell.clone();
                observe_mcall(strand, &cell, method, args, out)
            }
            _ => instance_mcall_fallback(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::JOIN | sym::CANCEL | sym::WAIT => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::DONE => {
                let done = this.borrow(strand)?.result.is_some();
                Output::set(strand, out, done);
                Ok(())
            }
            sym::ID => {
                let id = this.borrow(strand)?.cell.id();
                Output::set(strand, out, id);
                Ok(())
            }
            tag if is_observe_method(tag) => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }
}

/// A background strand together with its caller-facing stream endpoints.
///
/// `input`/`output` are already [`StreamIter`]/[`StreamSink`]-wrapped (built
/// alongside the channels themselves, at construction time) rather than the
/// raw channel values, so that anything extracted from a `Stream` via
/// `op_iter`/`op_sink` keeps [`Handle`] GC-reachable even once the `Stream`
/// itself is no longer referenced (e.g. a `for` loop that discards the
/// original expression once it has extracted an iterator from it). A bare
/// channel value holds no reference back to the strand, so handing one out
/// directly used to let the strand get collected and canceled mid-iteration.
pub(crate) struct Stream<'v> {
    pub(crate) handle: GcObj<'v, Handle<'v>>,
    pub(crate) input: Value<'v>,
    pub(crate) output: Value<'v>,
}

unsafe impl<'v> Collect for Stream<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.handle.accept(visit)?;
        self.input.accept(visit)?;
        self.output.accept(visit)
    }

    fn clear(&mut self) {
        self.input = Value::NIL;
        self.output = Value::NIL;
    }
}

/// Wraps the channel `Value` returned by `Stream::iter()`, additionally
/// keeping the background strand's [`Handle`] GC-reachable for as long as
/// the wrapper itself is reachable. See [`Stream`] for why this exists.
pub(crate) struct StreamIter<'v> {
    value: Value<'v>,
    root: GcObj<'v, Handle<'v>>,
}

impl<'v> StreamIter<'v> {
    pub(crate) fn new(value: Value<'v>, root: GcObj<'v, Handle<'v>>) -> Self {
        Self { value, root }
    }
}

unsafe impl<'v> Collect for StreamIter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.value.accept(visit)?;
        self.root.accept(visit)
    }

    fn clear(&mut self) {
        self.value.clear();
    }
}

impl<'v> Protocol<'v> for StreamIter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<strand.Stream.iter>")
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
        let value = this.borrow(strand)?.value.dup();
        value.next(strand, out).await
    }

    // `close` isn't part of the `Iter` surface (`iter::classify`), but the
    // channel underneath it has one, and `Stream::JOIN` (below) relies on
    // being able to call it on whatever it gets back from `iter`/`sink` to
    // release the channel promptly rather than waiting on the GC.
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::CLOSE {
            let value = this.borrow(strand)?.value.dup();
            value.op_get(strand, field, out)
        } else {
            iter::iter_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::CLOSE {
            let value = this.borrow(strand)?.value.dup();
            value.op_mcall(strand, method, args, out).await
        } else {
            iter::iter_mcall(strand, &this, method, args, out).await
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: super::protocol::SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this.clone(), sink).await
    }
}

/// Wraps the channel `Value` returned by `Stream::sink()`. See [`StreamIter`]
/// for why this exists rather than handing back the raw channel `Value`
/// directly.
pub(crate) struct StreamSink<'v> {
    value: Value<'v>,
    root: GcObj<'v, Handle<'v>>,
}

impl<'v> StreamSink<'v> {
    pub(crate) fn new(value: Value<'v>, root: GcObj<'v, Handle<'v>>) -> Self {
        Self { value, root }
    }
}

unsafe impl<'v> Collect for StreamSink<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.value.accept(visit)?;
        self.root.accept(visit)
    }

    fn clear(&mut self) {
        self.value.clear();
    }
}

impl<'v> Protocol<'v> for StreamSink<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().output_iter);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<strand.Stream.sink>")
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let value = this.borrow(strand)?.value.dup();
        value.put(strand, item).await
    }

    // See the matching comment on `StreamIter::op_get`.
    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::CLOSE {
            let value = this.borrow(strand)?.value.dup();
            value.op_get(strand, field, out)
        } else {
            iter::sink_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::CLOSE {
            let value = this.borrow(strand)?.value.dup();
            value.op_mcall(strand, method, args, out).await
        } else {
            iter::sink_mcall(strand, &this, method, args, out).await
        }
    }
}

impl<'v> Protocol<'v> for Stream<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().stream)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<strand.Stream>")
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let cell = stream_cell(strand, &this)?;
        cell_eq(strand, &cell, other)
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        cell_hash(&*stream_cell(strand, &this)?, hasher);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::JOIN => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let (input, output, handle) = {
                    let borrow = this.borrow(strand)?;
                    (
                        borrow.input.dup(),
                        borrow.output.dup(),
                        borrow.handle.clone(),
                    )
                };
                let close = Sym::well_known(sym::CLOSE);
                strand
                    .with_slots(async move |strand, [mut tmp]| {
                        let _ = method!(strand, &input, close, &mut tmp).await;
                        let _ = method!(strand, &output, close, &mut tmp).await;
                    })
                    .await;
                join_handle(&handle, strand, out).await
            }
            sym::CANCEL => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let handle = this.borrow(strand)?.handle.clone();
                let borrow = handle.borrow().ok_or_else(|| Error::concurrency(strand))?;
                borrow.interrupt.cancel();
                Ok(())
            }
            sym::WAIT => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let handle = this.borrow(strand)?.handle.clone();
                wait_handle(&handle, strand).await
            }
            sym::DONE | sym::ID => Err(field_not_method(strand, method)),
            tag if is_observe_method(tag) => {
                let cell = stream_cell(strand, &this)?;
                observe_mcall(strand, &cell, method, args, out)
            }
            tag => match iter::classify(tag) {
                Some(iter::Surface::Iterable) => {
                    iter::iterable_mcall(strand, &this, method, args, out).await
                }
                Some(iter::Surface::Sinkable) => {
                    iter::sinkable_mcall(strand, &this, method, args, out).await
                }
                None => Err(Error::field(strand, method)),
            },
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let output = this.borrow(strand)?.output.dup();
        Output::set(strand, out, &output);
        Ok(())
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let input = this.borrow(strand)?.input.dup();
        Output::set(strand, out, &input);
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::JOIN | sym::CANCEL | sym::WAIT => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::DONE => {
                let handle = this.borrow(strand)?.handle.clone();
                let done = handle
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .result
                    .is_some();
                Output::set(strand, out, done);
                Ok(())
            }
            sym::ID => {
                let id = stream_cell(strand, &this)?.id();
                Output::set(strand, out, id);
                Ok(())
            }
            tag if is_observe_method(tag) => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            tag => match iter::classify(tag) {
                Some(iter::Surface::Iterable) => iter::iterable_get(strand, &this, field, out),
                Some(iter::Surface::Sinkable) => iter::sinkable_get(strand, &this, field, out),
                None => Err(Error::field(strand, field)),
            },
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: super::protocol::SpreadContext,
        sink: &'a mut dyn super::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this.clone(), sink).await
    }
}

// ── Task Class ──────────────────────────────────────────────────

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type strand.Task>")
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
            || strand
                .singletons()
                .strand
                .eq(strand, supertype)
                .unwrap_or(false)
    }
}

// ── Stream Class ────────────────────────────────────────────────

pub(crate) struct StreamType;

unsafe impl Collect for StreamType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for StreamType {
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
            || strand
                .singletons()
                .task
                .eq(strand, supertype)
                .unwrap_or(false)
            || strand
                .singletons()
                .strand
                .eq(strand, supertype)
                .unwrap_or(false)
            || strand
                .singletons()
                .iterable
                .eq(strand, supertype)
                .unwrap_or(false)
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
        crate::fmt!(strand, w, "<type strand.Stream>")
    }
}

// ── Strand observation ──────────────────────────────────────────

/// Observation handle to any strand (`strand.Strand`).
///
/// Holds no values and does not keep the strand alive.
pub(crate) struct StrandRef {
    cell: Rc<StrandCell>,
}

impl StrandRef {
    pub(crate) fn new(cell: Rc<StrandCell>) -> Self {
        Self { cell }
    }
}

unsafe impl Collect for StrandRef {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

fn stream_cell<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: &Recv<'v, '_, Stream<'v>>,
) -> Result<'v, 's, Rc<StrandCell>> {
    let handle = this.borrow(strand)?.handle.clone();
    let cell = handle
        .borrow()
        .ok_or_else(|| Error::concurrency(strand))?
        .cell
        .clone();
    Ok(cell)
}

/// The cell behind any strand handle value.
fn cell_of<'v>(vm: &Vm<'v>, value: &Value<'v>) -> Option<Rc<StrandCell>> {
    let types = vm.builtin_types();
    if let Some(r) = value.downcast_ref(types.strand_ref) {
        return Some(r.get().cell.clone());
    }
    let handle = match value.downcast_ref(types.strand_handle) {
        Some(h) => h.to_strong(),
        None => value.downcast_ref(types.stream)?.get().handle.clone(),
    };
    Some(handle.borrow()?.cell.clone())
}

fn cell_eq<'v, 's>(
    strand: &mut Strand<'v, 's>,
    cell: &StrandCell,
    other: &Value<'v>,
) -> Result<'v, 's, Value<'v>> {
    match cell_of(strand.vm(), other) {
        Some(other) => Ok(Value::from_bool(other.id() == cell.id())),
        None => Err(Error::unsupported(strand, "`==`")),
    }
}

fn cell_hash(cell: &StrandCell, hasher: &mut DefaultHasher) {
    cell.id().hash(hasher)
}

fn is_observe_method(tag: sym::Tag) -> bool {
    matches!(tag, sym::BACKTRACE | sym::CHILDREN | sym::INFO)
}

fn field_not_method<'v, 's>(strand: &mut Strand<'v, 's>, field: Sym<'v, '_>) -> Error<'v, 's> {
    let name = field.as_str(strand.vm()).to_owned();
    Error::type_error(strand, format!("`{name}` is a field, not a method"))
}

fn exited<'v, 's>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::state_error(strand, "strand has exited")
}

/// Observation methods shared by every strand handle type.
///
/// Each walk is synchronous: the strand pointer is used only before any GC
/// object is built.
fn observe_mcall<'v, 's>(
    strand: &mut Strand<'v, 's>,
    cell: &Rc<StrandCell>,
    method: Sym<'v, '_>,
    args: Args<'v, '_>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match method.tag() {
        sym::BACKTRACE => {
            let full_key = Sym::well_known(sym::FULL);
            let ([], [full]) = unpack!(strand, args, 0, 0, full_key = None)?;
            let full = match full {
                Some(full) => full.op_bool(strand)?,
                None => false,
            };
            let entries = match (cell.state(), cell.get()) {
                (CellState::Live, Some(inner)) => unsafe {
                    strand.entries_of(inner.as_ref(), full)
                },
                (CellState::Pending, _) => Vec::new(),
                _ => return Err(exited(strand)),
            };
            backtrace::create(strand, entries, out);
            Ok(())
        }
        sym::CHILDREN => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let cells: Vec<_> = match (cell.state(), cell.get()) {
                (CellState::Live, Some(inner)) => unsafe { inner.as_ref() }
                    .children()
                    .into_iter()
                    .map(StrandInner::cell)
                    .collect(),
                (CellState::Pending, _) => Vec::new(),
                _ => return Err(exited(strand)),
            };
            let ty = strand.builtin_types().strand_ref;
            let values: Vec<_> = cells
                .into_iter()
                .map(|cell| {
                    let mut value = Value::NIL;
                    // Safety: written by the last allocation; see `Slot::new`.
                    let slot = unsafe { Slot::new(NonNull::from(&mut value)) };
                    ty.create(strand, StrandRef::new(cell), slot);
                    value
                })
                .collect();
            tuple::create(strand, values, out);
            Ok(())
        }
        sym::INFO => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let raw = match (cell.state(), cell.get()) {
                (CellState::Live, Some(inner)) => unsafe { strand.observe(inner.as_ref()) },
                (CellState::Pending, _) => RawInfo {
                    cell: cell.clone(),
                    parent: None,
                    entries: Vec::new(),
                },
                _ => return Err(exited(strand)),
            };
            strand
                .builtin_types()
                .strand_info
                .create(strand, StrandInfo::new(raw), out);
            Ok(())
        }
        _ => unreachable!("not an observation method"),
    }
}

impl<'v> Protocol<'v> for StrandRef {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let id = this.get().cell.id();
        crate::fmt!(strand, w, "<strand.Strand {id}>")
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        cell_eq(strand, &this.get().cell, other)
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        cell_hash(&this.get().cell, hasher);
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let cell = this.get().cell.clone();
        match method.tag() {
            sym::CANCEL => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if let (CellState::Live, Some(inner)) = (cell.state(), cell.get()) {
                    // Safety: synchronous use
                    unsafe { inner.as_ref() }.base_interrupt().cancel();
                }
                Ok(())
            }
            sym::DONE | sym::ID => Err(field_not_method(strand, method)),
            tag if is_observe_method(tag) => observe_mcall(strand, &cell, method, args, out),
            _ => instance_mcall_fallback(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::CANCEL | sym::BACKTRACE | sym::CHILDREN | sym::INFO => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::DONE => {
                let done = this.get().cell.state() == CellState::Exited;
                Output::set(strand, out, done);
                Ok(())
            }
            sym::ID => {
                let id = this.get().cell.id();
                Output::set(strand, out, id);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }
}

/// The `strand.Strand` class.
pub(crate) struct StrandType;

unsafe impl Collect for StrandType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for StrandType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type strand.Strand>")
    }
}

// ── StrandInfo ──────────────────────────────────────────────────

enum InfoBacktrace<'v> {
    Entries(Vec<UnwindEntry<'v>>),
    Built(Value<'v>),
}

/// One strand's state at the time of a snapshot (`strand.StrandInfo`).
///
/// Holds the raw walk data; Do projections are built on demand, and the
/// backtrace is cached once built.
pub(crate) struct StrandInfo<'v> {
    cell: Rc<StrandCell>,
    parent: Option<Rc<StrandCell>>,
    backtrace: RefCell<InfoBacktrace<'v>>,
}

impl<'v> StrandInfo<'v> {
    pub(crate) fn new(raw: RawInfo<'v>) -> Self {
        Self {
            cell: raw.cell,
            parent: raw.parent,
            backtrace: RefCell::new(InfoBacktrace::Entries(raw.entries)),
        }
    }
}

unsafe impl<'v> Collect for StrandInfo<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        // Never borrowed across an allocation
        match &*self.backtrace.borrow() {
            InfoBacktrace::Entries(entries) => {
                for entry in entries {
                    entry.accept(visit)?;
                }
            }
            InfoBacktrace::Built(value) => value.accept(visit)?,
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Protocol<'v> for StrandInfo<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().strand_info)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let id = this.get().cell.id();
        crate::fmt!(strand, w, "<strand.StrandInfo {id}>")
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let info = this.get();
        match field.tag() {
            sym::STRAND => {
                let ty = strand.builtin_types().strand_ref;
                ty.create(strand, StrandRef::new(info.cell.clone()), out);
                Ok(())
            }
            sym::PARENT => {
                match &info.parent {
                    Some(parent) => {
                        let ty = strand.builtin_types().strand_ref;
                        ty.create(strand, StrandRef::new(parent.clone()), out);
                    }
                    None => Output::set(strand, out, Value::NIL),
                }
                Ok(())
            }
            sym::BACKTRACE => {
                let mut state = info.backtrace.borrow_mut();
                match &mut *state {
                    InfoBacktrace::Built(value) => Output::set(strand, out, &*value),
                    InfoBacktrace::Entries(entries) => {
                        let entries = mem::take(entries);
                        // `accept` borrows the state, so release it before allocating
                        drop(state);
                        let mut out = out;
                        backtrace::create(strand, entries, &mut out);
                        *info.backtrace.borrow_mut() = InfoBacktrace::Built(out.dup());
                    }
                }
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }
}

/// The `strand.StrandInfo` class.
pub(crate) struct StrandInfoType;

unsafe impl Collect for StrandInfoType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for StrandInfoType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type strand.StrandInfo>")
    }
}
