use std::{fmt, future, ops::ControlFlow, rc::Rc, task::Poll, task::Waker};

use crate::{
    arg::Args,
    error::{Error, ErrorPair, Result, ResultExt},
    gc::{Collect, arena::Visit},
    method,
    strand::{InterruptToken, Strand, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
};

use super::{
    iter,
    protocol::{Protocol, Recv},
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
    pub(crate) stream_input: Value<'v>,
    pub(crate) stream_output: Value<'v>,
}

impl<'v> Handle<'v> {
    pub(crate) fn new(inner: Rc<StrandInner<'v>>, interrupt: InterruptToken<'v>) -> Self {
        Self {
            inner: Some(inner),
            interrupt,
            result: None,
            wakers: Vec::new(),
            stream_input: Value::NIL,
            stream_output: Value::NIL,
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
        // Scan result value
        if let Some(Completion::Ok(v)) = &self.result {
            v.accept(visit)?;
        }
        if let Some(Completion::Err((v, _))) = &self.result {
            v.accept(visit)?;
        }

        self.stream_input.accept(visit)?;
        self.stream_output.accept(visit)?;

        // Scan the strand's stack (start_callable, frame chain, input/output)
        if let Some(ref inner) = self.inner {
            unsafe { inner.scan_stack(visit)? };
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        // Cancel the strand
        self.interrupt.cancel();
        // Drop our reference to StrandInner (the Future's Rc clone keeps it alive during unwind)
        self.inner = None;
    }
}

impl<'v> Drop for Handle<'v> {
    fn drop(&mut self) {
        self.clear()
    }
}

impl<'v> Protocol<'v> for Handle<'v> {
    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        let borrow = this.borrow(strand).ok();
        let is_iterable = borrow
            .as_ref()
            .is_some_and(|borrow| !borrow.stream_output.is_nil());
        let is_sinkable = borrow
            .as_ref()
            .is_some_and(|borrow| !borrow.stream_input.is_nil());
        (is_iterable && supertype.eq(strand, &strand.singletons().iterable))
            || (is_sinkable && supertype.eq(strand, &strand.singletons().sinkable))
            || supertype.eq(strand, &strand.singletons().strand)
            || supertype.eq(strand, TypeObject::Value)
    }

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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let is_stream = !this.borrow(strand)?.stream_output.is_nil();
        write!(
            w,
            "<std.strand.{}>",
            if is_stream { "Stream" } else { "Strand" }
        )
        .into_do(strand)
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
                // Close stream channels first to prevent deadlock when the strand
                // is blocked on I/O from the handle side.
                let (stream_input, stream_output) = {
                    let borrow = this.borrow(strand)?;
                    (borrow.stream_input.dup(), borrow.stream_output.dup())
                };
                let close = Sym::well_known(sym::CLOSE);
                if !stream_input.is_nil() {
                    strand
                        .with_slots(async move |strand, [mut tmp]| {
                            let _ = method!(strand, &stream_input, close, &mut tmp).await;
                        })
                        .await;
                }
                if !stream_output.is_nil() {
                    strand
                        .with_slots(async move |strand, [mut tmp]| {
                            let _ = method!(strand, &stream_output, close, &mut tmp).await;
                        })
                        .await;
                }
                // Suspend until result is available. Uses borrow_mut just-in-time
                // and drops it before the await point so the GC can clear the
                // JoinHandle if needed (e.g. during cycle collection).
                future::poll_fn(|cx| {
                    let mut borrow = this.borrow_mut(strand)?;
                    if borrow.result.is_some() {
                        return Poll::Ready(Ok(()));
                    }
                    borrow.wakers.push(cx.waker().clone());
                    Poll::Pending
                })
                .await?;
                // Result is now available
                let borrow = this.borrow(strand)?;
                match borrow.result.as_ref().unwrap() {
                    Completion::Ok(v) => {
                        Output::set(strand, out, v);
                        Ok(())
                    }
                    Completion::Err(pair) => Err(Error::from_pair_ref(strand, pair)),
                }
            }
            sym::CANCEL => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                borrow.interrupt.cancel();
                Ok(())
            }
            sym::WAIT => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                future::poll_fn(|cx| {
                    let mut borrow = this.borrow_mut(strand)?;
                    if borrow.result.is_some() {
                        return Poll::Ready(Ok(()));
                    }
                    borrow.wakers.push(cx.waker().clone());
                    Poll::Pending
                })
                .await
            }
            sym::DONE => Err(Error::type_error(strand, "`done` is a field, not a method")),
            sym::ITER => {
                let is_stream = {
                    let borrow = this.borrow(strand)?;
                    !borrow.stream_input.is_nil() && !borrow.stream_output.is_nil()
                };
                if !is_stream {
                    return Err(Error::field(strand, method));
                }
                iter::iterable_mcall(strand, &this, method, args, out).await
            }
            sym::SINK => {
                let is_stream = {
                    let borrow = this.borrow(strand)?;
                    !borrow.stream_input.is_nil() && !borrow.stream_output.is_nil()
                };
                if !is_stream {
                    return Err(Error::field(strand, method));
                }
                iter::sinkable_mcall(strand, &this, method, args, out).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        if borrow.stream_output.is_nil() {
            return Err(Error::type_error(strand, "strand is not a stream"));
        }
        Output::set(strand, out, &borrow.stream_output);
        Ok(())
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        if borrow.stream_input.is_nil() {
            return Err(Error::type_error(strand, "strand is not a stream"));
        }
        Output::set(strand, out, &borrow.stream_input);
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let is_stream = {
            let borrow = this.borrow(strand)?;
            !borrow.stream_input.is_nil() && !borrow.stream_output.is_nil()
        };
        match field.tag() {
            sym::JOIN | sym::CANCEL | sym::WAIT => {
                super::BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::DONE => {
                let input = this.borrow(strand)?.result.is_some();
                Output::set(strand, out, input);
                Ok(())
            }
            sym::ITER if is_stream => iter::iterable_get(strand, &this, field, out),
            sym::SINK if is_stream => iter::sinkable_get(strand, &this, field, out),
            _ => Err(Error::field(strand, field)),
        }
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        use crate::error::ResultExt;
        write!(w, "<type std.strand>").into_do(strand)
    }
}
