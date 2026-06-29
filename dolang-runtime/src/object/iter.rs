use std::{fmt, ops::ControlFlow};

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result, ResultExt},
    gc::{Annex, Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{Inspect, Protocol, Recv},
        tuple,
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Input, Output, Slot, TypeObject, Value},
    vm::Vm,
};

fn iter_members<'v, 'a>() -> Vec<Sym<'v, 'a>> {
    vec![
        Sym::well_known(sym::ALL),
        Sym::well_known(sym::ANY),
        Sym::well_known(sym::COUNT),
        Sym::well_known(sym::FOLD),
        Sym::well_known(sym::ITER),
        Sym::well_known(sym::NEXT),
        Sym::well_known(sym::MAP),
        Sym::well_known(sym::FILTER),
        Sym::well_known(sym::CHAIN),
        Sym::well_known(sym::ZIP),
        Sym::well_known(sym::TAKE),
        Sym::well_known(sym::SKIP),
        Sym::well_known(sym::ENUMERATE),
        Sym::well_known(sym::FIND),
        Sym::well_known(sym::MIN),
        Sym::well_known(sym::MAX),
    ]
}

fn iterable_members<'v, 'a>() -> Vec<Sym<'v, 'a>> {
    vec![Sym::well_known(sym::ITER)]
}

fn sink_members<'v, 'a>() -> Vec<Sym<'v, 'a>> {
    vec![
        Sym::well_known(sym::SINK),
        Sym::well_known(sym::PUT),
        Sym::well_known(sym::MAP),
        Sym::well_known(sym::FILTER),
    ]
}

fn sinkable_members<'v, 'a>() -> Vec<Sym<'v, 'a>> {
    vec![Sym::well_known(sym::SINK)]
}

pub(crate) fn iter_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::ALL
        | sym::ANY
        | sym::COUNT
        | sym::FOLD
        | sym::ITER
        | sym::NEXT
        | sym::MAP
        | sym::FILTER
        | sym::CHAIN
        | sym::ZIP
        | sym::TAKE
        | sym::SKIP
        | sym::ENUMERATE
        | sym::FIND
        | sym::MIN
        | sym::MAX => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => Err(Error::field(strand, field)),
    }
}

async fn iter_extrema<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    default: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
    is_min: bool,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut iter, mut item]| {
            obj.iter(strand, &mut iter).await?;
            if !iter.next(strand, &mut out).await? {
                if let Some(mut default) = default {
                    out.store(default.take());
                    return Ok(());
                }
                return Err(Error::iter_stop(strand));
            }
            while iter.next(strand, &mut item).await? {
                let replace = if is_min {
                    item.op_lt(strand, &out)?.to_bool(strand)
                } else {
                    out.op_lt(strand, &item)?.to_bool(strand)
                };
                if replace {
                    Slot::swap(Slot::reborrow(&mut out), &mut item);
                }
                strand.check_trap_gc()?;
            }
            Ok(())
        })
        .await
}

async fn iter_all_any<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    pred: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
    want_all: bool,
) -> Result<'v, 's, ()> {
    let has_pred = pred.is_some();
    strand
        .with_slots(
            async move |strand, [mut iter, mut item, mut pred_fn, mut pred_out]| {
                obj.iter(strand, &mut iter).await?;
                if let Some(mut pred) = pred {
                    pred_fn.store(pred.take());
                }
                while iter.next(strand, &mut item).await? {
                    let passed = if has_pred {
                        call!(strand, &pred_fn, &mut pred_out, &item).await?;
                        pred_out.to_bool(strand)
                    } else {
                        item.to_bool(strand)
                    };
                    if want_all && !passed {
                        out.store(Value::FALSE);
                        return Ok(());
                    }
                    if !want_all && passed {
                        out.store(Value::TRUE);
                        return Ok(());
                    }
                    strand.check_trap_gc()?;
                }
                out.store(Value::from_bool(want_all));
                Ok(())
            },
        )
        .await
}

async fn iter_count<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut iter, mut item]| {
            obj.iter(strand, &mut iter).await?;
            let mut count = 0usize;
            while iter.next(strand, &mut item).await? {
                count += 1;
                if count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap_gc()?;
                }
            }
            let value = i64::try_from(count).map_err(|_| Error::overflow(strand))?;
            out.store(Value::from_i64(strand, value));
            Ok(())
        })
        .await
}

async fn iter_fold<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut init: Slot<'v, 'a>,
    mut func: Slot<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(
            async move |strand, [mut iter, mut acc, mut next_acc, mut item, mut func_slot]| {
                obj.iter(strand, &mut iter).await?;
                acc.store(init.take());
                func_slot.store(func.take());
                while iter.next(strand, &mut item).await? {
                    call!(strand, &func_slot, &mut next_acc, &acc, &item).await?;
                    Slot::swap(Slot::reborrow(&mut acc), &mut next_acc);
                    strand.check_trap_gc()?;
                }
                out.store(acc.take());
                Ok(())
            },
        )
        .await
}

fn nonnegative_count<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, usize> {
    let count = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, "expected int"))?;
    if count < 0 {
        return Err(Error::value(strand, "expected non-negative int"));
    }
    usize::try_from(count).map_err(|_| Error::overflow(strand))
}

async fn iter_find<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut pred: Slot<'v, 'a>,
    default: Option<Slot<'v, 'a>>,
    or_else: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if default.is_some() && or_else.is_some() {
        return Err(Error::unexpected_key(strand, Sym::well_known(sym::ELSE)));
    }
    strand
        .with_slots(
            async move |strand, [mut iter, mut item, mut pred_fn, mut pred_out]| {
                obj.iter(strand, &mut iter).await?;
                pred_fn.store(pred.take());
                while iter.next(strand, &mut item).await? {
                    call!(strand, &pred_fn, &mut pred_out, &item).await?;
                    if pred_out.to_bool(strand) {
                        out.store(item.take());
                        return Ok(());
                    }
                    strand.check_trap_gc()?;
                }
                if let Some(mut default) = default {
                    out.store(default.take());
                    return Ok(());
                }
                if let Some(or_else) = or_else {
                    return call!(strand, or_else, out).await;
                }
                Err(Error::runtime(strand, "find: no matching item"))
            },
        )
        .await
}

pub(crate) async fn iter_next<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    iter: &Value<'v>,
    default: Option<Slot<'v, 'a>>,
    or_else: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if default.is_some() && or_else.is_some() {
        return Err(Error::unexpected_key(strand, Sym::well_known(sym::ELSE)));
    }
    if iter.next(strand, &mut out).await? {
        return Ok(());
    }
    if let Some(mut default) = default {
        out.store(default.take());
        return Ok(());
    }
    if let Some(or_else) = or_else {
        return call!(strand, or_else, out).await;
    }
    Err(Error::iter_stop(strand))
}

async fn collect_chain_sources<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
) -> Result<'v, 's, Vec<Value<'v>>> {
    strand
        .with_slots(async move |strand, [mut tmp]| {
            let mut sources = Vec::new();
            for arg in args {
                let slot = match arg {
                    Arg::Pos(slot) => slot,
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                };
                slot.iter(strand, &mut tmp).await?;
                sources.push(tmp.take());
            }
            Ok(sources)
        })
        .await
}

async fn collect_zip_sources<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
) -> Result<'v, 's, Vec<Value<'v>>> {
    strand
        .with_slots(async move |strand, [mut tmp]| {
            let mut sources = Vec::new();
            for arg in args {
                let slot = match arg {
                    Arg::Pos(slot) => slot,
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                };
                slot.iter(strand, &mut tmp).await?;
                sources.push(tmp.take());
            }
            Ok(sources)
        })
        .await
}

pub(crate) struct Iter;
pub(crate) struct Iterable;
pub(crate) struct Sinkable;

unsafe impl Collect for Iter {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for Iterable {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for Sinkable {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

pub(crate) struct Sink;

unsafe impl Collect for Sink {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

pub(crate) async fn iter_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if method.tag() == sym::ITER {
        iterable_mcall(strand, rcvr, method, args, out).await
    } else {
        let delegator = Value::from_input(strand, rcvr);
        strand
            .vm()
            .singletons()
            .input_iter
            .op_dcall(strand, &delegator, method, args, out)
            .await
    }
}

impl<'v> Protocol<'v> for Iterable {
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
        write!(w, "<type Iterable>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: iterable_members(),
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iterable_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::ITER => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                obj.iter(strand, out).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_dcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        args.prepend_self(delegator.dup());
        Iterable::op_mcall(this, strand, method, args, out).await
    }
}

impl<'v> Protocol<'v> for Sinkable {
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
        write!(w, "<type Sinkable>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: sinkable_members(),
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            sinkable_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::SINK => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                obj.sink(strand, out).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_dcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        args.prepend_self(delegator.dup());
        Sinkable::op_mcall(this, strand, method, args, out).await
    }
}

pub(crate) fn sink_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::SINK | sym::PUT | sym::MAP | sym::FILTER => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => Err(Error::field(strand, field)),
    }
}

pub(crate) async fn sink_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if method.tag() == sym::SINK {
        sinkable_mcall(strand, rcvr, method, args, out).await
    } else {
        let delegator = Value::from_input(strand, rcvr);
        strand
            .vm()
            .singletons()
            .output_iter
            .op_dcall(strand, &delegator, method, args, out)
            .await
    }
}

pub(crate) fn iterable_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::ITER => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => Err(Error::field(strand, field)),
    }
}

pub(crate) async fn iterable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let delegator = Value::from_input(strand, rcvr);
    strand
        .vm()
        .singletons()
        .iterable
        .op_dcall(strand, &delegator, method, args, out)
        .await
}

pub(crate) fn sinkable_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::SINK => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => Err(Error::field(strand, field)),
    }
}

pub(crate) async fn sinkable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let delegator = Value::from_input(strand, rcvr);
    strand
        .vm()
        .singletons()
        .sinkable
        .op_dcall(strand, &delegator, method, args, out)
        .await
}

pub(crate) struct Chain<'v> {
    sources: Vec<Value<'v>>,
    index: usize,
}

pub(crate) struct Zip<'v> {
    sources: Vec<Value<'v>>,
}

unsafe impl<'v> Collect for Chain<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for source in self.sources.iter() {
            source.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.sources.clear();
        self.index = 0;
    }
}

unsafe impl<'v> Collect for Zip<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for source in self.sources.iter() {
            source.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.sources.clear();
    }
}

impl<'v> Protocol<'v> for Chain<'v> {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Chain>").into_do(strand)
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        loop {
            let source = {
                let borrow = this.borrow(strand)?;
                match borrow.sources.get(borrow.index) {
                    Some(source) => source.dup(),
                    None => return Ok(false),
                }
            };
            if source.next(strand, Slot::reborrow(&mut out)).await? {
                return Ok(true);
            }
            this.borrow_mut(strand)?.index += 1;
        }
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
}

impl<'v> Protocol<'v> for Zip<'v> {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Zip>").into_do(strand)
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let sources = this
            .borrow(strand)?
            .sources
            .iter()
            .map(Value::dup)
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return Ok(false);
        }
        let mut items = Vec::with_capacity(sources.len());
        for source in sources {
            let mut item = Value::NIL;
            if !source.next(strand, Slot::new(&mut item)).await? {
                return Ok(false);
            }
            items.push(item);
        }
        out.store(Value::from_object(tuple::tuple(strand, items)));
        Ok(true)
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
}

pub(crate) struct Take<'v> {
    source: Value<'v>,
    remaining: usize,
}

pub(crate) struct Skip<'v> {
    source: Value<'v>,
    remaining: usize,
}

pub(crate) struct Enumerate<'v> {
    source: Value<'v>,
    index: i64,
}

unsafe impl<'v> Collect for Take<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.source.clear();
        self.remaining = 0;
    }
}

unsafe impl<'v> Collect for Skip<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.source.clear();
        self.remaining = 0;
    }
}

unsafe impl<'v> Collect for Enumerate<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.source.clear();
        self.index = 0;
    }
}

impl<'v> Protocol<'v> for Take<'v> {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Take>").into_do(strand)
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
        let source = {
            let borrow = this.borrow(strand)?;
            if borrow.remaining == 0 {
                return Ok(false);
            }
            borrow.source.dup()
        };
        if !source.next(strand, out).await? {
            return Ok(false);
        }
        this.borrow_mut(strand)?.remaining -= 1;
        Ok(true)
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
}

impl<'v> Protocol<'v> for Skip<'v> {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Skip>").into_do(strand)
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
        strand
            .with_slots(async move |strand, [mut item]| {
                loop {
                    let (source, skipping) = {
                        let mut borrow = this.borrow_mut(strand)?;
                        let skipping = borrow.remaining > 0;
                        if skipping {
                            borrow.remaining -= 1;
                        }
                        (borrow.source.dup(), skipping)
                    };
                    if !skipping {
                        return source.next(strand, out).await;
                    }
                    if !source.next(strand, &mut item).await? {
                        return Ok(false);
                    }
                    strand.check_trap_gc()?;
                }
            })
            .await
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
}

impl<'v> Protocol<'v> for Enumerate<'v> {
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Enumerate>").into_do(strand)
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        strand
            .with_slots(async move |strand, [mut item]| {
                let (source, index) = {
                    let borrow = this.borrow(strand)?;
                    (borrow.source.dup(), borrow.index)
                };
                if !source.next(strand, &mut item).await? {
                    return Ok(false);
                }
                let next_index = index
                    .checked_add(1)
                    .ok_or_else(|| Error::overflow(strand))?;
                out.store(Value::from_object(tuple::tuple(
                    strand,
                    vec![Value::from_i64(strand, index), item.take()],
                )));
                this.borrow_mut(strand)?.index = next_index;
                Ok(true)
            })
            .await
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
}

impl<'v> Protocol<'v> for Iter {
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
        supertype: &crate::value::Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, TypeObject::Value)
            || strand.singletons().iterable.eq(strand, supertype)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type Iter>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: iter_members(),
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iter_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let default = Sym::well_known(sym::DEFAULT);
        let else_key = Sym::well_known(sym::ELSE);
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::NEXT => {
                let ([obj], [default, or_else]) =
                    unpack!(strand, args, 1, 0, default = None, else_key = None)?;
                iter_next(strand, &obj, default, or_else, out).await
            }
            sym::ALL => {
                let ([obj], [pred]) = unpack!(strand, args, 1, 1)?;
                iter_all_any(strand, &obj, pred, out, true).await
            }
            sym::ANY => {
                let ([obj], [pred]) = unpack!(strand, args, 1, 1)?;
                iter_all_any(strand, &obj, pred, out, false).await
            }
            sym::COUNT => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                iter_count(strand, &obj, out).await
            }
            sym::FOLD => {
                let ([obj, init, func], []) = unpack!(strand, args, 3, 0)?;
                iter_fold(strand, &obj, init, func, out).await
            }
            sym::MAP => {
                let ([obj, func], []) = unpack!(strand, args, 2, 0)?;
                create_map(strand, &obj, func, true, false, out).await
            }
            sym::FILTER => {
                let ([obj, pred], []) = unpack!(strand, args, 2, 0)?;
                create_filter(strand, &obj, pred, true, false, out).await
            }
            sym::CHAIN => create_chain_from_args(strand, args, out).await,
            sym::ZIP => create_zip_from_args(strand, args, out).await,
            sym::TAKE => {
                let ([obj, count], []) = unpack!(strand, args, 2, 0)?;
                let count = nonnegative_count(strand, &count)?;
                create_take(strand, obj.dup(), count, out);
                Ok(())
            }
            sym::SKIP => {
                let ([obj, count], []) = unpack!(strand, args, 2, 0)?;
                let count = nonnegative_count(strand, &count)?;
                create_skip(strand, obj.dup(), count, out);
                Ok(())
            }
            sym::ENUMERATE => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                create_enumerate(strand, obj.dup(), out);
                Ok(())
            }
            sym::FIND => {
                let ([obj, pred], [default, or_else]) =
                    unpack!(strand, args, 2, 0, default = None, else_key = None)?;
                iter_find(strand, &obj, pred, default, or_else, out).await
            }
            sym::MIN => {
                let ([obj], [default]) = unpack!(strand, args, 1, 0, default = None)?;
                iter_extrema(strand, &obj, default, out, true).await
            }
            sym::MAX => {
                let ([obj], [default]) = unpack!(strand, args, 1, 0, default = None)?;
                iter_extrema(strand, &obj, default, out, false).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_dcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        args.prepend_self(delegator.dup());
        Iter::op_mcall(this, strand, method, args, out).await
    }
}

impl<'v> Protocol<'v> for Sink {
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
        supertype: &crate::value::Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, TypeObject::Value)
            || strand.singletons().sinkable.eq(strand, supertype)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type Sink>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: sink_members(),
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            sink_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::PUT => {
                let ([obj, value], []) = unpack!(strand, args, 2, 0)?;
                obj.put(strand, value).await
            }
            sym::MAP => {
                let ([obj, func], []) = unpack!(strand, args, 2, 0)?;
                create_map(strand, &obj, func, false, true, out).await
            }
            sym::FILTER => {
                let ([obj, pred], []) = unpack!(strand, args, 2, 0)?;
                create_filter(strand, &obj, pred, false, true, out).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_dcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        delegator: &'a Value<'v>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        args.prepend_self(delegator.dup());
        Sink::op_mcall(this, strand, method, args, out).await
    }
}

pub(crate) struct Map<'v> {
    func: Value<'v>,
    obj: Value<'v>,
}

pub(crate) struct MapAnnex {
    has_input: bool,
    has_output: bool,
}

impl Annex for MapAnnex {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self) {}
}

unsafe impl<'v> Collect for Map<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = MapAnnex;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.func.accept(visit)?;
        self.obj.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.func.clear();
        self.obj.clear();
    }
}

impl<'v> Protocol<'v> for Map<'v> {
    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(
            strand,
            out,
            if this.annex().has_input {
                &strand.singletons().map_iter
            } else {
                &strand.singletons().output_iter
            },
        );
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Map>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !this.annex().has_input {
            return Err(Error::not_supported(strand));
        }
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if !this.annex().has_input {
            return Err(Error::not_supported(strand));
        }
        strand
            .with_slots(async move |strand, [mut input, mut func, mut item]| {
                let borrow = this.borrow(strand)?;
                input.store(borrow.obj.dup());
                func.store(borrow.func.dup());
                drop(borrow);
                if input.next(strand, &mut item).await? {
                    call!(strand, &func, out, &item).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .await
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !this.annex().has_output {
            return Err(Error::not_supported(strand));
        }
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !this.annex().has_output {
            return Err(Error::not_supported(strand));
        }
        strand
            .with_slots(async move |strand, [mut output, mut func]| {
                let borrow = this.borrow(strand)?;
                func.store(borrow.func.dup());
                output.store(borrow.obj.dup());
                drop(borrow);
                call!(strand, &func, &mut output, value).await?;
                this.borrow(strand)?.obj.put(strand, output).await
            })
            .await
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::PUT && this.annex().has_output {
            sink_get(strand, &this, field, out)
        } else {
            if field.tag() == sym::INIT_METHOD {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            } else {
                iter_get(strand, &this, field, out)
            }
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::PUT if this.annex().has_output => {
                sink_mcall(strand, &this, method, args, out).await
            }
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

pub(crate) struct Filter<'v> {
    pred: Value<'v>,
    obj: Value<'v>,
}

pub(crate) struct FilterAnnex {
    has_input: bool,
    has_output: bool,
}

impl Annex for FilterAnnex {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self) {}
}

unsafe impl<'v> Collect for Filter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = FilterAnnex;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.pred.accept(visit)?;
        self.obj.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.pred.clear();
        self.obj.clear();
    }
}

impl<'v> Protocol<'v> for Filter<'v> {
    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(
            strand,
            out,
            if this.annex().has_input {
                &strand.singletons().filter_iter
            } else {
                &strand.singletons().output_iter
            },
        );
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<std.iter.Filter>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !this.annex().has_input {
            return Err(Error::not_supported(strand));
        }
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        if !this.annex().has_input {
            return Err(Error::not_supported(strand));
        }
        strand
            .with_slots(async move |strand, [mut input, mut pred, mut res]| {
                let borrow = this.borrow(strand)?;
                pred.store(borrow.pred.dup());
                input.store(borrow.obj.dup());
                drop(borrow);
                loop {
                    if !input.next(strand, &mut out).await? {
                        return Ok(false);
                    }
                    call!(strand, &pred, &mut res, &out).await?;
                    if res.to_bool(strand) {
                        return Ok(true);
                    }
                }
            })
            .await
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !this.annex().has_output {
            return Err(Error::not_supported(strand));
        }
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !this.annex().has_output {
            return Err(Error::not_supported(strand));
        }
        strand
            .with_slots(async move |strand, [mut output, mut pred]| {
                let borrow = this.borrow(strand)?;
                pred.store(borrow.pred.dup());
                output.store(borrow.obj.dup());
                drop(borrow);
                call!(strand, &pred, &mut output, &value).await?;
                if output.to_bool(strand) {
                    this.borrow(strand)?.obj.put(strand, value).await
                } else {
                    Ok(())
                }
            })
            .await
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::PUT && this.annex().has_output {
            sink_get(strand, &this, field, out)
        } else {
            if field.tag() == sym::INIT_METHOD {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            } else {
                iter_get(strand, &this, field, out)
            }
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::PUT if this.annex().has_output => {
                sink_mcall(strand, &this, method, args, out).await
            }
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

pub(crate) struct MapType;

unsafe impl Collect for MapType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for MapType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.iter.Map>").into_do(strand)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([func, obj], []) = unpack!(strand, args, 2, 0)?;
        create_map(strand, &obj, func, true, false, out).await
    }
}

pub(crate) struct FilterType;

unsafe impl Collect for FilterType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for FilterType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.iter.Filter>").into_do(strand)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([pred, obj], []) = unpack!(strand, args, 2, 0)?;
        create_filter(strand, &obj, pred, true, false, out).await
    }
}

pub(crate) async fn create_map<'v, 's>(
    strand: &mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut func: Slot<'v, '_>,
    has_input: bool,
    has_output: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    strand.builtin_types().map_iter.create_with_annex(
        strand,
        Map {
            func: func.take(),
            obj: obj.dup(),
        },
        MapAnnex {
            has_input,
            has_output,
        },
        out,
    );
    Ok(())
}

pub(crate) async fn create_filter<'v, 's>(
    strand: &mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut pred: Slot<'v, '_>,
    has_input: bool,
    has_output: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    strand.builtin_types().filter_iter.create_with_annex(
        strand,
        Filter {
            pred: pred.take(),
            obj: obj.dup(),
        },
        FilterAnnex {
            has_input,
            has_output,
        },
        out,
    );
    Ok(())
}

pub(crate) fn create_chain<'v>(
    strand: &mut Strand<'v, '_>,
    sources: Vec<Value<'v>>,
    out: impl Output<'v>,
) {
    strand
        .builtin_types()
        .chain_iter
        .create(strand, Chain { sources, index: 0 }, out);
}

pub(crate) fn create_zip<'v>(
    strand: &mut Strand<'v, '_>,
    sources: Vec<Value<'v>>,
    out: impl Output<'v>,
) {
    strand
        .builtin_types()
        .zip_iter
        .create(strand, Zip { sources }, out);
}

pub(crate) fn create_take<'v>(
    strand: &mut Strand<'v, '_>,
    source: Value<'v>,
    remaining: usize,
    out: impl Output<'v>,
) {
    strand
        .builtin_types()
        .take_iter
        .create(strand, Take { source, remaining }, out);
}

pub(crate) fn create_skip<'v>(
    strand: &mut Strand<'v, '_>,
    source: Value<'v>,
    remaining: usize,
    out: impl Output<'v>,
) {
    strand
        .builtin_types()
        .skip_iter
        .create(strand, Skip { source, remaining }, out);
}

pub(crate) fn create_enumerate<'v>(
    strand: &mut Strand<'v, '_>,
    source: Value<'v>,
    out: impl Output<'v>,
) {
    strand
        .builtin_types()
        .enumerate_iter
        .create(strand, Enumerate { source, index: 0 }, out);
}

pub(crate) async fn create_chain_from_args<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let sources = collect_chain_sources(strand, args).await?;
    create_chain(strand, sources, out);
    Ok(())
}

pub(crate) async fn create_zip_from_args<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let sources = collect_zip_sources(strand, args).await?;
    create_zip(strand, sources, out);
    Ok(())
}

/// Null iterator/sink that yields no items and discards all items.
pub(crate) struct Null;

unsafe impl Collect for Null {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Null {
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
        write!(w, "<std.NullIter>").into_do(strand)
    }

    // Iter protocol: never yields any items
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Ok(false)
    }

    // Sink protocol: discards everything
    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Discard the value
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::PUT {
            sink_get(strand, &this, field, out)
        } else {
            if field.tag() == sym::INIT_METHOD {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            } else {
                iter_get(strand, &this, field, out)
            }
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::PUT => sink_mcall(strand, &this, method, args, out).await,
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}
