use std::{future, ops::ControlFlow, rc::Rc, task::Poll, task::Waker};

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, ErrorPair, Result},
    gc::{Collect, arena::Visit},
    method,
    strand::{InterruptToken, Strand, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
};

use super::{
    iter,
    protocol::{GcObj, Protocol, Recv},
};

/// Result stored in a JoinHandle after a background strand completes.
pub(crate) enum Completion<'v> {
    Ok(Value<'v>),
    Err(ErrorPair<'v>),
}

/// GC-managed handle for a background strand.
pub(crate) struct Handle<'v> {
    pub(crate) inner: Option<Rc<StrandInner<'v>>>,
    pub(crate) interrupt: InterruptToken<'v>,
    pub(crate) result: Option<Completion<'v>>,
    pub(crate) wakers: Vec<Waker>,
}

impl<'v> Handle<'v> {
    pub(crate) fn new(inner: Rc<StrandInner<'v>>, interrupt: InterruptToken<'v>) -> Self {
        Self {
            inner: Some(inner),
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
        if let Some(Completion::Err((v, _))) = &self.result {
            v.accept(visit)?;
        }

        // Scan the strand's stack (start_callable, frame chain, input/output)
        if let Some(ref inner) = self.inner {
            unsafe { inner.scan_stack(visit)? };
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        // Drop our reference to StrandInner (the Future's Rc clone keeps it alive during unwind)
        if self.inner.take().is_some() {
            // Cancel the strand
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
        Output::set(strand, out, &strand.singletons().strand)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<strand.Strand>")
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
            sym::DONE => Err(Error::type_error(strand, "`done` is a field, not a method")),
            _ => Err(Error::field(strand, method)),
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
            _ => Err(Error::field(strand, field)),
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
            sym::DONE => Err(Error::type_error(strand, "`done` is a field, not a method")),
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

// ── Strand Class ────────────────────────────────────────────────

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
        crate::fmt!(strand, w, "<type strand.Strand>")
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
