use std::{cell::UnsafeCell, ops::ControlFlow};

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    bytecode::Variadic,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{
            Delegated, Dispatch, Inspect, Member, Protocol, Recv, Spread, SpreadContext, members,
        },
        tuple,
    },
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{BinEmbryo, Input, Output, Slot, Slots, TypeObject, Value, view::View},
    vm::Vm,
};

/// Methods of [`Iter`] that [`Iterable`] forwards, by materializing an
/// iterator from the receiver with `iter` and re-dispatching onto it.
///
/// `iter` itself heads the list: it is the defining method of the surface.
const ITERABLE_METHODS: &[sym::Tag] = &[
    sym::ITER,
    sym::ALL,
    sym::ANY,
    sym::FOLD,
    sym::MAP,
    sym::FILTER,
    sym::CHOMP,
    sym::CRIMP,
    sym::CHAIN,
    sym::ZIP,
    sym::TAKE,
    sym::SKIP,
    sym::ENUMERATE,
    sym::FIND,
    sym::MIN,
    sym::MAX,
];

/// Methods of [`Iter`] that [`Iterable`] deliberately does *not* forward.
///
/// - `next` is stateful: a container has no iteration position of its own, so
///   forwarding it would mint a fresh iterator per call and a `while` loop
///   over `next` would spin on the first element forever.
/// - `count` is `len` spelled expensively, and `Dict`/`dict_view`/`Record`
///   already define `count` with unrelated semantics.
/// - `kv` describes the pair shape of an iterator; spread behavior belongs to
///   the iterable itself.
const ITER_ONLY_METHODS: &[sym::Tag] = &[sym::NEXT, sym::COUNT, sym::KV];

/// Methods of [`Sink`], all of which [`Sinkable`] forwards by materializing a
/// sink from the receiver with `sink` and re-dispatching onto it.
///
/// `premap`/`prefilter` are named for their direction: unlike the `Iterable`
/// `map`/`filter`, they transform values on the way *into* the sink. The
/// distinct names are what let a value that is both surfaces (an `Array`, say)
/// offer both without one spelling carrying two meanings.
const SINKABLE_METHODS: &[sym::Tag] = &[
    sym::SINK,
    sym::PUT,
    sym::PREMAP,
    sym::PREFILTER,
    sym::PRECHOMP,
    sym::PRECRIMP,
];

/// Which abstract surface a method name belongs to, if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Surface {
    Iterable,
    Sinkable,
}

pub(crate) fn classify(tag: sym::Tag) -> Option<Surface> {
    if ITERABLE_METHODS.contains(&tag) {
        Some(Surface::Iterable)
    } else if SINKABLE_METHODS.contains(&tag) {
        Some(Surface::Sinkable)
    } else {
        None
    }
}

fn iter_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    members![
        Method(sym::ITER),
        Method(sym::ALL),
        Method(sym::ANY),
        Method(sym::FOLD),
        Method(sym::MAP),
        Method(sym::FILTER),
        Method(sym::CHOMP),
        Method(sym::CRIMP),
        Method(sym::CHAIN),
        Method(sym::ZIP),
        Method(sym::TAKE),
        Method(sym::SKIP),
        Method(sym::ENUMERATE),
        Method(sym::FIND),
        Method(sym::MIN),
        Method(sym::MAX),
        Method(sym::NEXT),
        Method(sym::COUNT),
        Method(sym::KV),
    ]
}

fn iterable_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    members![
        Method(sym::ITER),
        Method(sym::ALL),
        Method(sym::ANY),
        Method(sym::FOLD),
        Method(sym::MAP),
        Method(sym::FILTER),
        Method(sym::CHOMP),
        Method(sym::CRIMP),
        Method(sym::CHAIN),
        Method(sym::ZIP),
        Method(sym::TAKE),
        Method(sym::SKIP),
        Method(sym::ENUMERATE),
        Method(sym::FIND),
        Method(sym::MIN),
        Method(sym::MAX),
    ]
}

fn sink_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    sinkable_members()
}

fn sinkable_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    members![
        Method(sym::SINK),
        Method(sym::PUT),
        Method(sym::PREMAP),
        Method(sym::PREFILTER),
        Method(sym::PRECHOMP),
        Method(sym::PRECRIMP),
    ]
}

pub(crate) fn iter_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let tag = field.tag();
    if classify(tag) == Some(Surface::Iterable) || ITER_ONLY_METHODS.contains(&tag) {
        BoundMethod::create(strand, rcvr, field, out);
        Ok(())
    } else {
        Err(Error::field(strand, field))
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
        .with_slots(async move |strand, [mut item]| {
            if !obj.next(strand, &mut out).await? {
                if let Some(mut default) = default {
                    out.store(default.take());
                    return Ok(());
                }
                return Err(Error::iter_stop(strand));
            }
            while obj.next(strand, &mut item).await? {
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
        .with_slots(async move |strand, [mut item, mut pred_fn, mut pred_out]| {
            if let Some(mut pred) = pred {
                pred_fn.store(pred.take());
            }
            while obj.next(strand, &mut item).await? {
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
        })
        .await
}

async fn iter_count<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut item]| {
            let mut count = 0usize;
            while obj.next(strand, &mut item).await? {
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
            async move |strand, [mut acc, mut next_acc, mut item, mut func_slot]| {
                acc.store(init.take());
                func_slot.store(func.take());
                while obj.next(strand, &mut item).await? {
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
        .map_err(|_| Error::type_error(strand, "expected Int"))?;
    if count < 0 {
        return Err(Error::value(strand, "expected non-negative Int"));
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
        .with_slots(async move |strand, [mut item, mut pred_fn, mut pred_out]| {
            pred_fn.store(pred.take());
            while obj.next(strand, &mut item).await? {
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
        })
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

/// Collect the sources for a `chain` or `zip`.
///
/// The leading argument is the method receiver, which arrives already
/// materialized as an `Iter`. The rest are arbitrary iterables passed by the
/// caller, so each needs an `iter` of its own.
async fn collect_sources<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    mut args: Args<'v, 'a>,
) -> Result<'v, 's, Vec<Value<'v>>> {
    let Some(Arg::Pos(rcvr)) = args.next() else {
        return Err(Error::type_error(strand, "expected receiver argument"));
    };
    let sources = vec![rcvr.dup()];
    strand
        .with_slots(async move |strand, [mut tmp]| {
            let mut sources = sources;
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
        strand
            .with_slots(async move |strand, [mut delegator]| {
                Output::set(strand, Slot::reborrow(&mut delegator), rcvr);
                let receiver = &strand.vm().singletons().input_iter;
                Delegated::new(receiver, &delegator)
                    .op_mcall(strand, method, args, out)
                    .await
            })
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type Iterable>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: iterable_members(),
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iterable_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(delegator) = this.delegator() {
            args.prepend_self(delegator.dup());
        }
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::ITER => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                obj.iter(strand, out).await
            }
            // `x.foo(..)` on an `Iterable` means `x.iter().foo(..)`: take the
            // receiver off the front of the arguments, materialize an iterator
            // from it, and re-dispatch the method onto that iterator.
            tag if classify(tag) == Some(Surface::Iterable) => {
                forward(strand, args, out, method, Surface::Iterable).await
            }
            _ => Err(Error::field(strand, method)),
        }
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type Sinkable>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: sinkable_members(),
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            sinkable_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(delegator) = this.delegator() {
            args.prepend_self(delegator.dup());
        }
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::SINK => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                obj.sink(strand, out).await
            }
            // `x.foo(..)` on a `Sinkable` means `x.sink().foo(..)`.
            tag if classify(tag) == Some(Surface::Sinkable) => {
                forward(strand, args, out, method, Surface::Sinkable).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }
}

pub(crate) fn sink_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if classify(field.tag()) == Some(Surface::Sinkable) {
        BoundMethod::create(strand, rcvr, field, out);
        Ok(())
    } else {
        Err(Error::field(strand, field))
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
        strand
            .with_slots(async move |strand, [mut delegator]| {
                Output::set(strand, Slot::reborrow(&mut delegator), rcvr);
                let receiver = &strand.vm().singletons().output_iter;
                Delegated::new(receiver, &delegator)
                    .op_mcall(strand, method, args, out)
                    .await
            })
            .await
    }
}

/// Field access for the `Iterable` surface.
///
/// **Precondition:** a type routing here from `op_get` must also define an
/// `op_mcall` that routes to [`iterable_mcall`]. These helpers hand back a
/// `BoundMethod` whose `op_call` re-enters the receiver's `op_mcall`; with the
/// default `op_mcall` (which is `op_get` followed by `op_call`) that is an
/// unbounded recursion rather than a dispatch.
pub(crate) fn iterable_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if classify(field.tag()) == Some(Surface::Iterable) {
        BoundMethod::create(strand, rcvr, field, out);
        Ok(())
    } else {
        Err(Error::field(strand, field))
    }
}

pub(crate) async fn iterable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut delegator]| {
            Output::set(strand, Slot::reborrow(&mut delegator), rcvr);
            let receiver = &strand.vm().singletons().iterable;
            Delegated::new(receiver, &delegator)
                .op_mcall(strand, method, args, out)
                .await
        })
        .await
}

/// Field access for the `Sinkable` surface.
///
/// Carries the same `op_mcall` precondition as [`iterable_get`].
pub(crate) fn sinkable_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if classify(field.tag()) == Some(Surface::Sinkable) {
        BoundMethod::create(strand, rcvr, field, out);
        Ok(())
    } else {
        Err(Error::field(strand, field))
    }
}

pub(crate) async fn sinkable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut delegator]| {
            Output::set(strand, Slot::reborrow(&mut delegator), rcvr);
            let receiver = &strand.vm().singletons().sinkable;
            Delegated::new(receiver, &delegator)
                .op_mcall(strand, method, args, out)
                .await
        })
        .await
}

/// Forward an `Iterable`/`Sinkable` method onto a materialized `Iter`/`Sink`.
///
/// `args` arrives with the receiver as the leading positional argument (either
/// prepended for delegated dispatch, or written explicitly as in `Iterable.map(x, f)`).
/// Popping it leaves exactly the method's own arguments behind, so dispatching
/// `method` onto the materialized value is literally `x.iter().foo(..)`.
///
/// Materialization is idempotent — `iter` on an `Iter` returns self — so this
/// costs one extra vtable call when the receiver was already an iterator, and
/// nothing at all in the common case where it was a container.
async fn forward<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    mut args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    method: Sym<'v, 'a>,
    surface: Surface,
) -> Result<'v, 's, ()> {
    let Some(Arg::Pos(obj)) = args.next() else {
        return Err(Error::type_error(strand, "expected receiver argument"));
    };
    strand
        .with_slots(async move |strand, [mut target]| {
            match surface {
                Surface::Iterable => obj.iter(strand, &mut target).await?,
                Surface::Sinkable => obj.sink(strand, &mut target).await?,
            }
            target.op_mcall(strand, method, args, out).await
        })
        .await
}

/// Field access for a type that is both `Iterable` and `Sinkable`.
///
/// The two surfaces are disjoint by construction (see [`classify`]), so the
/// name alone selects one; there is no ambiguity to resolve and no need to
/// try one and fall back to the other.
pub(crate) fn iterable_sinkable_get<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if classify(field.tag()).is_some() {
        BoundMethod::create(strand, rcvr, field, out);
        Ok(())
    } else {
        Err(Error::field(strand, field))
    }
}

/// Method dispatch for a type that is both `Iterable` and `Sinkable`.
pub(crate) async fn iterable_sinkable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match classify(method.tag()) {
        Some(Surface::Iterable) => iterable_mcall(strand, rcvr, method, args, out).await,
        Some(Surface::Sinkable) => sinkable_mcall(strand, rcvr, method, args, out).await,
        None => Err(Error::field(strand, method)),
    }
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Chain>")
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Zip>")
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

pub(crate) struct Kv<'v> {
    source: Value<'v>,
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

unsafe impl<'v> Collect for Kv<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.source.clear();
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Take>")
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Skip>")
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Enumerate>")
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

impl<'v> Protocol<'v> for Kv<'v> {
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
        crate::fmt!(strand, w, "<std.Kv>")
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
        let source = this.borrow(strand)?.source.dup();
        source.next(strand, out).await
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        strand
            .with_slots(
                async move |strand, [mut iter, mut item, mut key, mut value]| {
                    let source = this.borrow(strand)?.source.dup();
                    source.iter(strand, &mut iter).await?;
                    while iter.next(strand, &mut item).await? {
                        if context == SpreadContext::Pairs {
                            let unpack = sig::Unpack {
                                required: 2,
                                optional: vec![],
                                keys: vec![],
                                sym_index: vec![],
                                variadic: Variadic::None,
                            };
                            let cells = [UnsafeCell::new(Value::NIL), UnsafeCell::new(Value::NIL)];
                            item.op_unpack(strand, &unpack, unsafe { Slots::new(&cells) })
                                .await?;
                            key.store(unsafe { (*cells[0].get()).take() });
                            value.store(unsafe { (*cells[1].get()).take() });
                            sink.keyed(
                                strand,
                                Slot::reborrow(&mut key),
                                Slot::reborrow(&mut value),
                            )?;
                        } else {
                            sink.positional(strand, Slot::reborrow(&mut item))?;
                        }
                    }
                    Ok(())
                },
            )
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type Iter>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: iter_members(),
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iter_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(delegator) = this.delegator() {
            args.prepend_self(delegator.dup());
        }
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
                create_map(strand, &obj, func, out);
                Ok(())
            }
            sym::FILTER => {
                let ([obj, pred], []) = unpack!(strand, args, 2, 0)?;
                create_filter(strand, &obj, pred, out);
                Ok(())
            }
            sym::CHOMP => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                create_chomp(strand, &obj, out);
                Ok(())
            }
            sym::CRIMP => {
                let ([obj], [terminator]) = unpack!(strand, args, 1, 1)?;
                create_crimp(strand, &obj, terminator.as_deref(), out)
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
            sym::KV => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                create_kv(strand, obj.dup(), out);
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type Sink>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: sink_members(),
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            sink_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(delegator) = this.delegator() {
            args.prepend_self(delegator.dup());
        }
        match method.tag() {
            sym::INIT_METHOD => {
                let ([_self_val], []) = unpack!(strand, args, 1, 0)?;
                Ok(())
            }
            sym::PUT => {
                let ([obj, value], []) = unpack!(strand, args, 2, 0)?;
                obj.put(strand, value).await
            }
            sym::PREMAP => {
                let ([obj, func], []) = unpack!(strand, args, 2, 0)?;
                create_premap(strand, &obj, func, out);
                Ok(())
            }
            sym::PREFILTER => {
                let ([obj, pred], []) = unpack!(strand, args, 2, 0)?;
                create_prefilter(strand, &obj, pred, out);
                Ok(())
            }
            sym::PRECHOMP => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                create_prechomp(strand, &obj, out);
                Ok(())
            }
            sym::PRECRIMP => {
                let ([obj], [terminator]) = unpack!(strand, args, 1, 1)?;
                create_precrimp(strand, &obj, terminator.as_deref(), out)
            }
            _ => Err(Error::field(strand, method)),
        }
    }
}

/// Iterator adapter applying a function to each item yielded by the source.
///
/// The contravariant counterpart is [`Premap`]. The two are separate types
/// rather than one type with a direction flag: `map` and `premap` are distinct
/// names on disjoint surfaces (`ITERABLE_METHODS` vs `SINKABLE_METHODS`), so
/// nothing needs to resolve a direction at runtime.
pub(crate) struct Map<'v> {
    func: Value<'v>,
    obj: Value<'v>,
}

unsafe impl<'v> Collect for Map<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

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
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().map_iter);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Map>")
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
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Sink adapter applying a function to each item on its way to the downstream
/// sink. The covariant counterpart is [`Map`].
pub(crate) struct Premap<'v> {
    func: Value<'v>,
    obj: Value<'v>,
}

unsafe impl<'v> Collect for Premap<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

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

impl<'v> Protocol<'v> for Premap<'v> {
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
        crate::fmt!(strand, w, "<std.Premap>")
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
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            sink_get(strand, &this, field, out)
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
            _ => sink_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Iterator adapter yielding only the source items satisfying a predicate.
///
/// The contravariant counterpart is [`Prefilter`]; see [`Map`] for why the two
/// directions are separate types.
pub(crate) struct Filter<'v> {
    pred: Value<'v>,
    obj: Value<'v>,
}

unsafe impl<'v> Collect for Filter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

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
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().filter_iter);
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Filter>")
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
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Sink adapter forwarding to the downstream sink only the items satisfying a
/// predicate. The covariant counterpart is [`Filter`].
pub(crate) struct Prefilter<'v> {
    pred: Value<'v>,
    obj: Value<'v>,
}

unsafe impl<'v> Collect for Prefilter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

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

impl<'v> Protocol<'v> for Prefilter<'v> {
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
        crate::fmt!(strand, w, "<std.Prefilter>")
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
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            sink_get(strand, &this, field, out)
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
            _ => sink_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Length of the line terminator (`\r\n` or `\n`) ending `bytes`, or 0 if it
/// ends with neither.
///
/// A lone `\r` is deliberately *not* a terminator. It is a legitimate content
/// byte, and treating it as one would make `chomp` lossy on data that never
/// had a line structure to begin with.
pub(crate) fn line_terminator_len(bytes: &[u8]) -> usize {
    if bytes.ends_with(b"\r\n") {
        2
    } else if bytes.ends_with(b"\n") {
        1
    } else {
        0
    }
}

/// Writes `value` with one trailing line terminator removed, preserving its
/// `Str`/`Bin` type.
fn chomp_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    out: impl Output<'v>,
    who: &str,
) -> Result<'v, 's, ()> {
    match value.view(strand) {
        View::Str(str) => {
            let str = str.pin();
            let end = str.len() - line_terminator_len(str.as_bytes());
            Output::set(strand, out, &str[..end]);
        }
        View::Bin(bin) => {
            let bin = bin.pin();
            let end = bin.len() - line_terminator_len(&bin);
            Output::set(strand, out, &bin[..end]);
        }
        _ => {
            return Err(Error::type_error(
                strand,
                format!("{who}: expected `Str` or `Bin`"),
            ));
        }
    }
    Ok(())
}

/// Writes `value` with `terminator` appended, preserving its `Str`/`Bin` type.
///
/// The terminator is appended unconditionally. "Append only if missing" would
/// make the result depend on the item's own content, which is exactly the
/// implicit behavior these combinators exist to replace.
fn crimp_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    terminator: Option<&Value<'v>>,
    out: impl Output<'v>,
    who: &str,
) -> Result<'v, 's, ()> {
    let term: Vec<u8> = match terminator {
        None => b"\n".to_vec(),
        Some(term) => match term.view(strand) {
            View::Str(str) => str.pin().as_bytes().to_vec(),
            View::Bin(bin) => bin.pin().to_vec(),
            _ => {
                return Err(Error::type_error(
                    strand,
                    format!("{who}: terminator must be `Str` or `Bin`"),
                ));
            }
        },
    };
    let (bytes, is_str) = match value.view(strand) {
        View::Str(str) => (str.pin().as_bytes().to_vec(), true),
        View::Bin(bin) => (bin.pin().to_vec(), false),
        _ => {
            return Err(Error::type_error(
                strand,
                format!("{who}: expected `Str` or `Bin`"),
            ));
        }
    };
    let mut acc = BinEmbryo::new_with_capacity(strand, bytes.len() + term.len());
    acc.extend(strand, &bytes);
    acc.extend(strand, &term);
    if is_str {
        // Both halves were valid UTF-8 on their own, but a `Bin` terminator can
        // still split a character boundary, so this is a real check.
        acc.finish_str(strand, out)
            .map_err(|_| Error::type_error(strand, format!("{who}: terminator is not valid UTF-8")))
    } else {
        acc.finish(strand, out);
        Ok(())
    }
}

/// Iterator adapter removing one trailing line terminator from each item.
///
/// The contravariant counterpart is [`Prechomp`]; see [`Map`] for why the two
/// directions are separate types.
pub(crate) struct Chomp<'v> {
    obj: Value<'v>,
}

unsafe impl<'v> Collect for Chomp<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.obj.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.obj.clear();
    }
}

impl<'v> Protocol<'v> for Chomp<'v> {
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
        crate::fmt!(strand, w, "<std.Chomp>")
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
            .with_slots(async move |strand, [mut input, mut item]| {
                let borrow = this.borrow(strand)?;
                input.store(borrow.obj.dup());
                drop(borrow);
                if input.next(strand, &mut item).await? {
                    chomp_value(strand, &item, out, "iter.chomp")?;
                    Ok(true)
                } else {
                    Ok(false)
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iter_get(strand, &this, field, out)
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
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Sink adapter removing one trailing line terminator from each item on its way
/// to the downstream sink. The covariant counterpart is [`Chomp`].
pub(crate) struct Prechomp<'v> {
    obj: Value<'v>,
}

unsafe impl<'v> Collect for Prechomp<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.obj.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.obj.clear();
    }
}

impl<'v> Protocol<'v> for Prechomp<'v> {
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
        crate::fmt!(strand, w, "<std.Prechomp>")
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
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand
            .with_slots(async move |strand, [mut output, mut chomped]| {
                let borrow = this.borrow(strand)?;
                output.store(borrow.obj.dup());
                drop(borrow);
                chomp_value(strand, &value, &mut chomped, "sink.prechomp")?;
                output.put(strand, chomped).await
            })
            .await
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
            _ => sink_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Iterator adapter appending a line terminator to each item.
///
/// `terminator` is `None` when the default LF is in effect, which keeps the
/// common case allocation-free and platform-independent.
pub(crate) struct Crimp<'v> {
    obj: Value<'v>,
    terminator: Option<Value<'v>>,
}

unsafe impl<'v> Collect for Crimp<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.obj.accept(visit)?;
        if let Some(terminator) = &self.terminator {
            terminator.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.obj.clear();
        if let Some(terminator) = &mut self.terminator {
            terminator.clear();
        }
    }
}

impl<'v> Protocol<'v> for Crimp<'v> {
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
        crate::fmt!(strand, w, "<std.Crimp>")
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
            .with_slots(async move |strand, [mut input, mut item, mut term]| {
                let borrow = this.borrow(strand)?;
                input.store(borrow.obj.dup());
                let has_term = match &borrow.terminator {
                    Some(terminator) => {
                        term.store(terminator.dup());
                        true
                    }
                    None => false,
                };
                drop(borrow);
                if input.next(strand, &mut item).await? {
                    let term = has_term.then(|| &*term);
                    crimp_value(strand, &item, term, out, "iter.crimp")?;
                    Ok(true)
                } else {
                    Ok(false)
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            iter_get(strand, &this, field, out)
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
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

/// Sink adapter appending a line terminator to each item on its way to the
/// downstream sink. The covariant counterpart is [`Crimp`].
pub(crate) struct Precrimp<'v> {
    obj: Value<'v>,
    terminator: Option<Value<'v>>,
}

unsafe impl<'v> Collect for Precrimp<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.obj.accept(visit)?;
        if let Some(terminator) = &self.terminator {
            terminator.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.obj.clear();
        if let Some(terminator) = &mut self.terminator {
            terminator.clear();
        }
    }
}

impl<'v> Protocol<'v> for Precrimp<'v> {
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
        crate::fmt!(strand, w, "<std.Precrimp>")
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
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand
            .with_slots(async move |strand, [mut output, mut crimped, mut term]| {
                let borrow = this.borrow(strand)?;
                output.store(borrow.obj.dup());
                let has_term = match &borrow.terminator {
                    Some(terminator) => {
                        term.store(terminator.dup());
                        true
                    }
                    None => false,
                };
                drop(borrow);
                let term = has_term.then(|| &*term);
                crimp_value(strand, &value, term, &mut crimped, "sink.precrimp")?;
                output.put(strand, crimped).await
            })
            .await
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
            _ => sink_mcall(strand, &this, method, args, out).await,
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Map>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([func, obj], []) = unpack!(strand, args, 2, 0)?;
        create_map(strand, &obj, func, out);
        Ok(())
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
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Filter>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([pred, obj], []) = unpack!(strand, args, 2, 0)?;
        create_filter(strand, &obj, pred, out);
        Ok(())
    }
}

pub(crate) fn create_map<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut func: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    strand.builtin_types().map_iter.create(
        strand,
        Map {
            func: func.take(),
            obj: obj.dup(),
        },
        out,
    );
}

pub(crate) fn create_premap<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut func: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    strand.builtin_types().premap_iter.create(
        strand,
        Premap {
            func: func.take(),
            obj: obj.dup(),
        },
        out,
    );
}

pub(crate) fn create_filter<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut pred: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    strand.builtin_types().filter_iter.create(
        strand,
        Filter {
            pred: pred.take(),
            obj: obj.dup(),
        },
        out,
    );
}

pub(crate) fn create_prefilter<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut pred: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    strand.builtin_types().prefilter_iter.create(
        strand,
        Prefilter {
            pred: pred.take(),
            obj: obj.dup(),
        },
        out,
    );
}

pub(crate) fn create_chomp<'v>(strand: &mut Strand<'v, '_>, obj: &Value<'v>, out: impl Output<'v>) {
    strand
        .builtin_types()
        .chomp_iter
        .create(strand, Chomp { obj: obj.dup() }, out);
}

pub(crate) fn create_prechomp<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    out: impl Output<'v>,
) {
    strand
        .builtin_types()
        .prechomp_iter
        .create(strand, Prechomp { obj: obj.dup() }, out);
}

/// Validates an explicit `crimp`/`precrimp` terminator, rejecting anything that
/// is not `Str` or `Bin` at construction rather than on the first item.
fn check_terminator<'v, 's>(
    strand: &mut Strand<'v, 's>,
    terminator: Option<&Value<'v>>,
    who: &str,
) -> Result<'v, 's, Option<Value<'v>>> {
    match terminator {
        None => Ok(None),
        Some(terminator) => match terminator.view(strand) {
            View::Str(_) | View::Bin(_) => Ok(Some(terminator.dup())),
            _ => Err(Error::type_error(
                strand,
                format!("{who}: terminator must be `Str` or `Bin`"),
            )),
        },
    }
}

pub(crate) fn create_crimp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    obj: &Value<'v>,
    terminator: Option<&Value<'v>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let terminator = check_terminator(strand, terminator, "iter.crimp")?;
    strand.builtin_types().crimp_iter.create(
        strand,
        Crimp {
            obj: obj.dup(),
            terminator,
        },
        out,
    );
    Ok(())
}

pub(crate) fn create_precrimp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    obj: &Value<'v>,
    terminator: Option<&Value<'v>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let terminator = check_terminator(strand, terminator, "sink.precrimp")?;
    strand.builtin_types().precrimp_iter.create(
        strand,
        Precrimp {
            obj: obj.dup(),
            terminator,
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

pub(crate) fn create_kv<'v>(strand: &mut Strand<'v, '_>, source: Value<'v>, out: impl Output<'v>) {
    strand
        .builtin_types()
        .kv_iter
        .create(strand, Kv { source }, out);
}

pub(crate) async fn create_chain_from_args<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let sources = collect_sources(strand, args).await?;
    create_chain(strand, sources, out);
    Ok(())
}

pub(crate) async fn create_zip_from_args<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let sources = collect_sources(strand, args).await?;
    create_zip(strand, sources, out);
    Ok(())
}

/// The `std.Null` type object. `std.null` is its sole instance -- the same
/// relationship as `std.Nil`/`nil`, not a value fused with its own type.
pub(crate) struct NullType;

unsafe impl Collect for NullType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for NullType {
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
        // `null` acts as a complete iterator and sink by itself, not merely
        // something an `iter()`/`sink()` call can produce one from -- so it
        // claims `Iter`/`Sink` themselves, not just `Iterable`/`Sinkable`.
        let sings = strand.singletons();
        supertype.eq(strand, &this)
            || supertype.eq(strand, TypeObject::Value)
            || sings.input_iter.eq(strand, supertype)
            || sings.output_iter.eq(strand, supertype)
            || sings.iterable.eq(strand, supertype)
            || sings.sinkable.eq(strand, supertype)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Null>")
    }
}

/// Singleton iterator/sink that yields no items and discards all items.
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
        Output::set(strand, out, &strand.singletons().null_type)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Null>")
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
        if field.tag() == sym::INIT_METHOD {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else if classify(field.tag()) == Some(Surface::Sinkable) {
            sink_get(strand, &this, field, out)
        } else {
            iter_get(strand, &this, field, out)
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
            tag if classify(tag) == Some(Surface::Sinkable) => {
                sink_mcall(strand, &this, method, args, out).await
            }
            _ => iter_mcall(strand, &this, method, args, out).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::ErrorKind,
        gc::Annex,
        method,
        object::native::{Instance, Object, Type},
        test_support::with_vm,
        value::Empty,
        vm::{Builder, Stateful},
    };

    use super::*;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Test-only callable that wraps a Rust closure, used to drive `map`/`filter`/
    /// `find`/`all`/`any`/`fold` predicates without compiling Do source. The closure
    /// lives in the (immutable) `Annex`, and receives the call's `Args` plus an `out`
    /// `Slot` directly -- the same shape any real native method uses -- so it never
    /// needs to hold a `Value` outside a `Slot`/`Output`.
    struct NativeFn;

    /// Boxed closure body for [`NativeFnAnnex`]. Aliased mainly to keep
    /// clippy's `type_complexity` lint quiet; see [`NativeFn`] for the rationale.
    type NativeFnBody<'v> = Box<
        dyn for<'a, 's> Fn(&mut Strand<'v, 's>, Args<'v, 'a>, Slot<'v, 'a>) -> Result<'v, 's, ()>
            + 'v,
    >;

    struct NativeFnAnnex<'v> {
        body: NativeFnBody<'v>,
    }

    impl<'v> Annex for NativeFnAnnex<'v> {
        fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&self) {}
    }

    impl<'v> Object<'v> for NativeFn {
        const MODULE: &'v str = "test";
        const NAME: &'v str = "NativeFn";
        type Annex = NativeFnAnnex<'v>;
        type Type = ();
        type TypeAnnex = ();

        async fn call<'a, 's>(
            this: Instance<'v, 'a, Self>,
            strand: &'a mut Strand<'v, 's>,
            args: Args<'v, 'a>,
            out: Slot<'v, 'a>,
        ) -> Result<'v, 's, ()> {
            (this.annex().body)(strand, args, out)
        }
    }

    struct IterFixtureState<'v> {
        native_fn_ty: Type<'v, NativeFn>,
        bogus_sym: Sym<'v, 'v>,
    }

    struct IterFixtureStateTag;

    impl<'v> Stateful<'v> for IterFixtureState<'v> {
        type Tag = IterFixtureStateTag;
    }

    fn configure(vm: &mut Builder<'_>) {
        let native_fn_ty = vm.register_type::<NativeFn>();
        let bogus_sym = vm.sym("bogus");
        vm.register_state(IterFixtureState {
            native_fn_ty,
            bogus_sym,
        });
    }

    fn with_fixture_vm<const N: usize, R: 'static>(
        body: impl for<'v, 's, 'b> AsyncFnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R + 'static,
    ) -> R {
        crate::test_support::with_builder(async move |vm| {
            configure(vm);
            vm.enter_with_slots::<N, _>(body).await
        })
    }

    /// Construct a [`NativeFn`] wrapping `body` into `out`.
    fn make_native_fn<'v>(
        strand: &mut Strand<'v, '_>,
        body: impl for<'a, 's> Fn(&mut Strand<'v, 's>, Args<'v, 'a>, Slot<'v, 'a>) -> Result<'v, 's, ()>
        + 'v,
        out: impl Output<'v>,
    ) {
        let ty = strand.vm().state::<IterFixtureState>().native_fn_ty;
        ty.create_with_annex(
            strand,
            NativeFn,
            NativeFnAnnex {
                body: Box::new(body),
            },
            out,
        );
    }

    /// Build a fresh `Array` value containing `items` (as `Int`s) into `out`.
    fn make_int_array<'v>(strand: &mut Strand<'v, '_>, items: &[i64], mut out: Slot<'v, '_>) {
        Output::set(strand, &mut out, Empty::Array);
        let array = out.as_array(strand).unwrap();
        for &item in items {
            array.push(strand, item).unwrap();
        }
    }

    /// A minimal [`Spread`] sink that records what [`Kv::op_spread`] reports,
    /// distinguishing plain positional yields from `Pairs`-context key/value pairs.
    #[derive(Default)]
    struct CollectSpread {
        positional: Vec<i64>,
        pairs: Vec<(i64, i64)>,
    }

    impl<'v, 's> Spread<'v, 's> for CollectSpread {
        fn positional(
            &mut self,
            strand: &mut Strand<'v, 's>,
            value: Slot<'v, '_>,
        ) -> Result<'v, 's, ()> {
            self.positional.push(value.to_i64(strand)?);
            Ok(())
        }

        fn symbol(
            &mut self,
            _strand: &mut Strand<'v, 's>,
            _key: Sym<'v, '_>,
            _value: Slot<'v, '_>,
        ) -> Result<'v, 's, ()> {
            unreachable!("Kv::op_spread never yields symbol keys")
        }

        fn keyed(
            &mut self,
            strand: &mut Strand<'v, 's>,
            key: Slot<'v, '_>,
            value: Slot<'v, '_>,
        ) -> Result<'v, 's, ()> {
            self.pairs
                .push((key.to_i64(strand)?, value.to_i64(strand)?));
            Ok(())
        }
    }

    /// Drains an `Iter`-materializable value into a `Vec<i64>`.
    async fn collect_ints<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, Vec<i64>> {
        strand
            .with_slots(async move |strand, [mut it, mut item]| {
                value.iter(strand, &mut it).await?;
                let mut items = Vec::new();
                while it.next(strand, &mut item).await? {
                    items.push(item.to_i64(strand)?);
                }
                Ok(items)
            })
            .await
    }

    /// Drains a `Kv`-tagged iterator's `[k, v]` pairs into a `Vec<(i64, i64)>`,
    /// via `.next()` + indexing (not `op_spread` -- see the dedicated
    /// `kv_op_spread_*` test for that).
    async fn collect_pairs<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
    ) -> Result<'v, 's, Vec<(i64, i64)>> {
        strand
            .with_slots(async move |strand, [mut item, mut k, mut v]| {
                let mut pairs = Vec::new();
                while value.next(strand, &mut item).await? {
                    item.index(strand, 0i64, &mut k)?;
                    item.index(strand, 1i64, &mut v)?;
                    pairs.push((k.to_i64(strand)?, v.to_i64(strand)?));
                }
                Ok(pairs)
            })
            .await
    }

    // ── `iter_get`/`Iter::op_mcall` dispatch ───────────────────────────────────

    #[test]
    fn iter_get_dispatches_iterable_and_iter_only_fields_and_rejects_unrelated() {
        with_vm(async |strand, [mut arr, mut it, mut out]| {
            make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();

            // A forwarded `Iterable` method.
            it.get(strand, Sym::well_known(sym::MAP), &mut out).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));

            // An `Iter`-only method, withheld from `Iterable` but still valid here.
            it.get(strand, Sym::well_known(sym::NEXT), &mut out)
                .unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));

            let err = it
                .get(strand, Sym::well_known(sym::LEN), &mut out)
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);
        });
    }

    #[test]
    fn iter_next_advances_and_supports_default_and_rejects_default_and_else_together() {
        with_vm(async |strand, [mut arr, mut it, mut a, mut b]| {
            make_int_array(strand, &[1], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();

            let next_sym = Sym::well_known(sym::NEXT);
            let default_sym = Sym::well_known(sym::DEFAULT);
            let else_sym = Sym::well_known(sym::ELSE);

            method!(strand, &it, next_sym, &mut a).await.unwrap();
            assert_eq!(a.to_i64(strand).unwrap(), 1);
            method!(strand, &it, next_sym, &mut b, default_sym: 42i64)
                .await
                .unwrap();
            assert_eq!(b.to_i64(strand).unwrap(), 42);

            // `default:`/`else:` conflict is rejected up front, before either is ever
            // used -- reusing `a` as an arbitrary placeholder for `else:` is fine.
            let err = method!(strand, &it, next_sym, &mut b, default_sym: 1i64, else_sym: &a)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::UnexpectedKey);
        });
    }

    #[test]
    fn iter_find_locates_or_falls_back_and_rejects_default_and_else_together() {
        with_fixture_vm(async |strand, [mut arr, mut it, mut pred, mut out]| {
            let find_sym = Sym::well_known(sym::FIND);
            let default_sym = Sym::well_known(sym::DEFAULT);
            let else_sym = Sym::well_known(sym::ELSE);

            make_int_array(strand, &[1, 2, 3, 4], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([v], []) = unpack!(strand, args, 1, 0)?;
                    let cond = v.to_i64(strand)? > 2;
                    Output::set(strand, &mut out, cond);
                    Ok(())
                },
                Slot::reborrow(&mut pred),
            );
            method!(strand, &it, find_sym, &mut out, &pred)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 3);

            make_int_array(strand, &[1, 2], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([v], []) = unpack!(strand, args, 1, 0)?;
                    let cond = v.to_i64(strand)? > 5;
                    Output::set(strand, &mut out, cond);
                    Ok(())
                },
                Slot::reborrow(&mut pred),
            );
            method!(strand, &it, find_sym, &mut out, &pred, default_sym: 42i64)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 42);

            // No match, no default/else: a runtime error.
            arr.iter(strand, &mut it).await.unwrap();
            let err = method!(strand, &it, find_sym, &mut out, &pred)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Runtime);

            // `default:` and `else:` together are rejected before the predicate ever
            // runs -- `pred` is reused here purely as an arbitrary placeholder value.
            arr.iter(strand, &mut it).await.unwrap();
            let err = method!(
                strand,
                &it,
                find_sym,
                &mut out,
                &pred,
                default_sym: 1i64,
                else_sym: &pred
            )
            .await
            .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::UnexpectedKey);
        });
    }

    #[test]
    fn iter_all_any_short_circuit_with_and_without_predicate() {
        with_fixture_vm(async |strand, [mut arr, mut it, mut pred, mut out]| {
            let all_sym = Sym::well_known(sym::ALL);
            let any_sym = Sym::well_known(sym::ANY);

            make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, all_sym, &mut out).await.unwrap();
            assert!(out.to_bool(strand));

            make_int_array(strand, &[1, 0, 3], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, all_sym, &mut out).await.unwrap();
            assert!(!out.to_bool(strand));

            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, all_sym, &mut out).await.unwrap();
            assert!(out.to_bool(strand));

            make_int_array(strand, &[0, 0, 3], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, any_sym, &mut out).await.unwrap();
            assert!(out.to_bool(strand));

            make_int_array(strand, &[0], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, any_sym, &mut out).await.unwrap();
            assert!(!out.to_bool(strand));

            make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([v], []) = unpack!(strand, args, 1, 0)?;
                    let cond = v.to_i64(strand)? > 0;
                    Output::set(strand, &mut out, cond);
                    Ok(())
                },
                Slot::reborrow(&mut pred),
            );
            method!(strand, &it, all_sym, &mut out, &pred)
                .await
                .unwrap();
            assert!(out.to_bool(strand));

            arr.iter(strand, &mut it).await.unwrap();
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([v], []) = unpack!(strand, args, 1, 0)?;
                    let cond = v.to_i64(strand)? > 5;
                    Output::set(strand, &mut out, cond);
                    Ok(())
                },
                Slot::reborrow(&mut pred),
            );
            method!(strand, &it, any_sym, &mut out, &pred)
                .await
                .unwrap();
            assert!(!out.to_bool(strand));
        });
    }

    #[test]
    fn iter_count_and_fold_reduce_the_whole_sequence() {
        with_fixture_vm(async |strand, [mut arr, mut it, mut func, mut out]| {
            let count_sym = Sym::well_known(sym::COUNT);
            let fold_sym = Sym::well_known(sym::FOLD);

            make_int_array(strand, &[1, 2, 3, 4], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, count_sym, &mut out).await.unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 4);

            arr.iter(strand, &mut it).await.unwrap();
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([a, b], []) = unpack!(strand, args, 2, 0)?;
                    let sum = a.to_i64(strand)? + b.to_i64(strand)?;
                    Output::set(strand, &mut out, sum);
                    Ok(())
                },
                Slot::reborrow(&mut func),
            );
            method!(strand, &it, fold_sym, &mut out, 0i64, &func)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 10);

            arr.iter(strand, &mut it).await.unwrap();
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([a, b], []) = unpack!(strand, args, 2, 0)?;
                    let product = a.to_i64(strand)? * b.to_i64(strand)?;
                    Output::set(strand, &mut out, product);
                    Ok(())
                },
                Slot::reborrow(&mut func),
            );
            method!(strand, &it, fold_sym, &mut out, 1i64, &func)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 24);

            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, fold_sym, &mut out, 42i64, &func)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 42);
        });
    }

    #[test]
    fn iter_extrema_min_max_use_default_and_error_when_empty_without_one() {
        with_vm(async |strand, [mut arr, mut it, mut out]| {
            let min_sym = Sym::well_known(sym::MIN);
            let max_sym = Sym::well_known(sym::MAX);
            let default_sym = Sym::well_known(sym::DEFAULT);

            make_int_array(strand, &[3, 1, 4, 1, 5], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, min_sym, &mut out).await.unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 1);

            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, max_sym, &mut out).await.unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 5);

            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, min_sym, &mut out, default_sym: 42i64)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 42);

            arr.iter(strand, &mut it).await.unwrap();
            method!(strand, &it, max_sym, &mut out, default_sym: 42i64)
                .await
                .unwrap();
            assert_eq!(out.to_i64(strand).unwrap(), 42);

            arr.iter(strand, &mut it).await.unwrap();
            let err = method!(strand, &it, min_sym, &mut out).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::IterStop);
        });
    }

    // ── `Chain`/`Zip`/`Take`/`Skip`/`Enumerate`/`Kv` `Protocol` internals ─────

    #[test]
    fn chain_yields_sources_in_order_including_empty_sources() {
        with_vm(
            async |strand,
                   [
                mut empty,
                mut a,
                mut b,
                mut c,
                mut it_a,
                mut it_b,
                mut it_c,
                mut out,
            ]| {
                make_int_array(strand, &[], Slot::reborrow(&mut empty));
                empty.iter(strand, &mut it_a).await.unwrap();
                create_chain(strand, vec![it_a.take()], &mut out);
                assert_eq!(collect_ints(strand, &out).await.unwrap(), Vec::<i64>::new());

                make_int_array(strand, &[1, 2], Slot::reborrow(&mut a));
                make_int_array(strand, &[], Slot::reborrow(&mut b));
                make_int_array(strand, &[3, 4], Slot::reborrow(&mut c));
                a.iter(strand, &mut it_a).await.unwrap();
                b.iter(strand, &mut it_b).await.unwrap();
                c.iter(strand, &mut it_c).await.unwrap();
                create_chain(
                    strand,
                    vec![it_a.take(), it_b.take(), it_c.take()],
                    &mut out,
                );
                assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![1, 2, 3, 4]);
            },
        );
    }

    #[test]
    fn zip_stops_at_shortest_source_and_defaults_to_single_element_tuples() {
        with_vm(
            async |strand,
                   [
                mut a,
                mut b,
                mut it_a,
                mut it_b,
                mut zip,
                mut pair,
                mut lo,
                mut hi,
            ]| {
                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut a));
                a.iter(strand, &mut it_a).await.unwrap();
                create_zip(strand, vec![it_a.take()], &mut zip);
                let mut singles = Vec::new();
                while zip.next(strand, &mut pair).await.unwrap() {
                    pair.index(strand, 0i64, &mut lo).unwrap();
                    singles.push(lo.to_i64(strand).unwrap());
                }
                assert_eq!(singles, vec![1, 2, 3]);

                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut a));
                make_int_array(strand, &[4, 5], Slot::reborrow(&mut b));
                a.iter(strand, &mut it_a).await.unwrap();
                b.iter(strand, &mut it_b).await.unwrap();
                create_zip(strand, vec![it_a.take(), it_b.take()], &mut zip);

                let mut items = Vec::new();
                while zip.next(strand, &mut pair).await.unwrap() {
                    pair.index(strand, 0i64, &mut lo).unwrap();
                    pair.index(strand, 1i64, &mut hi).unwrap();
                    items.push((lo.to_i64(strand).unwrap(), hi.to_i64(strand).unwrap()));
                }
                assert_eq!(items, vec![(1, 4), (2, 5)]);
            },
        );
    }

    #[test]
    fn take_and_skip_clamp_at_the_source_length() {
        with_vm(async |strand, [mut arr, mut it, mut out]| {
            for (count, expected) in [(0usize, vec![]), (2, vec![1, 2]), (5, vec![1, 2, 3])] {
                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                create_take(strand, it.take(), count, &mut out);
                assert_eq!(
                    collect_ints(strand, &out).await.unwrap(),
                    expected,
                    "take {count}"
                );
            }
            for (count, expected) in [(0usize, vec![1, 2, 3]), (2, vec![3]), (5, vec![])] {
                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                create_skip(strand, it.take(), count, &mut out);
                assert_eq!(
                    collect_ints(strand, &out).await.unwrap(),
                    expected,
                    "skip {count}"
                );
            }
        });
    }

    #[test]
    fn enumerate_yields_sequential_indices_from_zero() {
        with_vm(
            async |strand, [mut arr, mut it, mut en, mut pair, mut idx, mut val]| {
                make_int_array(strand, &[10, 20, 30], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                create_enumerate(strand, it.take(), &mut en);

                let mut items = Vec::new();
                while en.next(strand, &mut pair).await.unwrap() {
                    pair.index(strand, 0i64, &mut idx).unwrap();
                    pair.index(strand, 1i64, &mut val).unwrap();
                    items.push((idx.to_i64(strand).unwrap(), val.to_i64(strand).unwrap()));
                }
                assert_eq!(items, vec![(0, 10), (1, 20), (2, 30)]);
            },
        );
    }

    #[test]
    fn kv_forwards_next_unchanged() {
        with_vm(async |strand, [mut arr, mut item, mut inner, mut kv]| {
            // `.kv()` doesn't transform items -- it only tags the iterator so that
            // spreading it treats items as pairs (see the `op_spread` test below).
            Output::set(strand, &mut arr, Empty::Array);
            let array = arr.as_array(strand).unwrap();
            for (k, v) in [(1i64, 10i64), (2, 20)] {
                Output::set(strand, &mut item, Empty::Array);
                let pair = item.as_array(strand).unwrap();
                pair.push(strand, k).unwrap();
                pair.push(strand, v).unwrap();
                array.push(strand, &item).unwrap();
            }
            arr.iter(strand, &mut inner).await.unwrap();
            create_kv(strand, inner.take(), &mut kv);
            assert_eq!(
                collect_pairs(strand, &kv).await.unwrap(),
                vec![(1, 10), (2, 20)]
            );

            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut inner).await.unwrap();
            create_kv(strand, inner.take(), &mut kv);
            assert!(!kv.next(strand, &mut item).await.unwrap());
        });
    }

    #[test]
    fn kv_op_spread_reports_pairs_context_as_keyed_and_others_as_positional() {
        with_vm(async |strand, [mut arr, mut item, mut inner, mut kv]| {
            Output::set(strand, &mut arr, Empty::Array);
            let array = arr.as_array(strand).unwrap();
            for (k, v) in [(1i64, 10i64), (2, 20)] {
                Output::set(strand, &mut item, Empty::Array);
                let pair = item.as_array(strand).unwrap();
                pair.push(strand, k).unwrap();
                pair.push(strand, v).unwrap();
                array.push(strand, &item).unwrap();
            }
            arr.iter(strand, &mut inner).await.unwrap();
            create_kv(strand, inner.take(), &mut kv);

            let kv_value: &Value = &kv;
            let mut sink = CollectSpread::default();
            strand
                .builtin_types()
                .kv_iter
                .cast(kv_value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    Kv::op_spread(recv, strand, SpreadContext::Pairs, &mut sink)
                        .await
                        .unwrap();
                })
                .await;
            assert_eq!(sink.pairs, vec![(1, 10), (2, 20)]);
            assert!(sink.positional.is_empty());
        });
    }

    // ── `Iterable`/`Sinkable` forwarding, via real `Array` values (still no Do
    //    source compiled -- this exercises the exact code path a compiled
    //    `x.map(f)`/`x.put(v)` call would take) ─────────────────────────────

    #[test]
    fn iterable_surface_forwards_via_op_mcall_and_op_get_and_rejects_unknown_methods() {
        with_fixture_vm(async |strand, [mut arr, mut pred, mut out, mut bound]| {
            let map_sym = Sym::well_known(sym::MAP);
            make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
            make_native_fn(
                strand,
                |strand, args, mut out| {
                    let ([v], []) = unpack!(strand, args, 1, 0)?;
                    let doubled = v.to_i64(strand)? * 2;
                    Output::set(strand, &mut out, doubled);
                    Ok(())
                },
                Slot::reborrow(&mut pred),
            );

            // Direct method call: array's own `op_mcall` forwards an unrecognized
            // `Iterable` method through delegated `iterable_mcall` dispatch.
            {
                let arr_val: &Value = &arr;
                method!(strand, arr_val, map_sym, &mut out, &pred)
                    .await
                    .unwrap();
            }
            assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![2, 4, 6]);

            // Field access then call: exercises `iterable_get`'s `BoundMethod` path.
            {
                let arr_val: &Value = &arr;
                arr_val.get(strand, map_sym, &mut bound).unwrap();
            }
            call!(strand, &bound, &mut out, &pred).await.unwrap();
            assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![2, 4, 6]);

            // A method outside the `Iterable` surface is rejected as an unknown field.
            let bogus = strand.vm().state::<IterFixtureState>().bogus_sym;
            let arr_val: &Value = &arr;
            let err = method!(strand, arr_val, bogus, &mut out).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);
        });
    }

    #[test]
    fn sinkable_surface_forwards_via_op_mcall_and_op_get_and_rejects_unknown_methods() {
        with_fixture_vm(async |strand, [mut arr, mut out, mut bound]| {
            let put_sym = Sym::well_known(sym::PUT);

            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            {
                let arr_val: &Value = &arr;
                method!(strand, arr_val, put_sym, &mut out, 5i64)
                    .await
                    .unwrap();
                method!(strand, arr_val, put_sym, &mut out, 7i64)
                    .await
                    .unwrap();
            }
            assert_eq!(arr.as_array(strand).unwrap().len(strand).unwrap(), 2);

            // Field access then call: exercises `sinkable_get`'s `BoundMethod` path.
            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            {
                let arr_val: &Value = &arr;
                arr_val.get(strand, put_sym, &mut bound).unwrap();
            }
            call!(strand, &bound, &mut out, 9i64).await.unwrap();
            assert_eq!(arr.as_array(strand).unwrap().len(strand).unwrap(), 1);

            let bogus = strand.vm().state::<IterFixtureState>().bogus_sym;
            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            let arr_val: &Value = &arr;
            let err = method!(strand, arr_val, bogus, &mut out).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);
        });
    }

    #[test]
    fn null_iterator_yields_nothing_and_sink_discards_puts() {
        with_vm(async |strand, []| {
            let null = strand.singletons().null.dup();
            null.put(strand, 5i64).await.unwrap();
            null.put(strand, 6i64).await.unwrap();
            assert_eq!(
                collect_ints(strand, &null).await.unwrap(),
                Vec::<i64>::new()
            );
        });
    }

    #[test]
    fn map_and_filter_transform_iterator_output_and_sink_input() {
        with_fixture_vm(
            async |strand,
                   [
                mut arr,
                mut it,
                mut func,
                mut out,
                mut acc,
                mut sink,
                mut transformed,
            ]| {
                let map_sym = Sym::well_known(sym::MAP);
                let filter_sym = Sym::well_known(sym::FILTER);

                make_int_array(strand, &[1, 2, 3, 4], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let doubled = v.to_i64(strand)? * 2;
                        Output::set(strand, &mut out, doubled);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                method!(strand, &it, map_sym, &mut out, &func)
                    .await
                    .unwrap();
                assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![2, 4, 6, 8]);

                arr.iter(strand, &mut it).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let cond = v.to_i64(strand)? % 2 == 0;
                        Output::set(strand, &mut out, cond);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                method!(strand, &it, filter_sym, &mut out, &func)
                    .await
                    .unwrap();
                assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![2, 4]);

                // `premap`: transforms values flowing into a sink.
                make_int_array(strand, &[], Slot::reborrow(&mut acc));
                acc.sink(strand, &mut sink).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let doubled = v.to_i64(strand)? * 2;
                        Output::set(strand, &mut out, doubled);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                create_premap(strand, &sink, Slot::reborrow(&mut func), &mut transformed);
                transformed.put(strand, 1i64).await.unwrap();
                transformed.put(strand, 2i64).await.unwrap();
                assert_eq!(collect_ints(strand, &acc).await.unwrap(), vec![2, 4]);

                // `prefilter`: discards values that fail the predicate before they
                // reach the sink.
                make_int_array(strand, &[], Slot::reborrow(&mut acc));
                acc.sink(strand, &mut sink).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let cond = v.to_i64(strand)? % 2 == 0;
                        Output::set(strand, &mut out, cond);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                create_prefilter(strand, &sink, Slot::reborrow(&mut func), &mut transformed);
                for v in [1i64, 2, 3, 4] {
                    transformed.put(strand, v).await.unwrap();
                }
                assert_eq!(collect_ints(strand, &acc).await.unwrap(), vec![2, 4]);
            },
        );
    }

    #[test]
    fn chain_and_zip_reject_keyword_arguments() {
        with_fixture_vm(async |strand, [mut arr, mut it, mut out]| {
            let chain_sym = Sym::well_known(sym::CHAIN);
            let zip_sym = Sym::well_known(sym::ZIP);
            let bogus_sym = strand.vm().state::<IterFixtureState>().bogus_sym;

            for method_sym in [chain_sym, zip_sym] {
                make_int_array(strand, &[1, 2], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                let err = method!(strand, &it, method_sym, &mut out, bogus_sym: 5i64)
                    .await
                    .unwrap_err();
                assert_eq!(err.kind(), ErrorKind::UnexpectedKey, "{method_sym:?}");
            }
        });
    }

    #[test]
    fn iter_and_sink_op_subtype_recognize_their_abstract_supertype() {
        with_vm(async |strand, []| {
            let iter_val = strand.singletons().input_iter.dup();
            let iterable_val = strand.singletons().iterable.dup();
            let unrelated = strand.singletons().array.dup();

            strand
                .builtin_types()
                .input_iter
                .cast(&iter_val)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(Iter::op_subtype(recv.clone(), strand, &iter_val));
                    assert!(Iter::op_subtype(recv.clone(), strand, &iterable_val));
                    assert!(!Iter::op_subtype(recv, strand, &unrelated));
                });

            let sink_val = strand.singletons().output_iter.dup();
            let sinkable_val = strand.singletons().sinkable.dup();

            strand
                .builtin_types()
                .output_iter
                .cast(&sink_val)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(Sink::op_subtype(recv.clone(), strand, &sink_val));
                    assert!(Sink::op_subtype(recv.clone(), strand, &sinkable_val));
                    assert!(!Sink::op_subtype(recv, strand, &unrelated));
                });
        });
    }

    // ── Abstract `Iterable`/`Sinkable`/`Iter`/`Sink` marker types: `op_debug` and
    //    `op_get` dispatched through the real `Value`, not the associated fns ────

    #[test]
    fn abstract_marker_types_op_debug_and_op_get_dispatch_init_method_and_reject_unrelated() {
        with_vm(async |strand, [mut ty, mut out]| {
            let init_sym = Sym::well_known(sym::INIT_METHOD);
            let bogus_sym = Sym::well_known(sym::LEN);

            for (singleton, expected_debug) in [
                (strand.singletons().iterable.dup(), "<type Iterable>"),
                (strand.singletons().sinkable.dup(), "<type Sinkable>"),
                (strand.singletons().input_iter.dup(), "<type Iter>"),
                (strand.singletons().output_iter.dup(), "<type Sink>"),
            ] {
                Output::set(strand, &mut ty, &singleton);
                assert_eq!(ty.to_debug(strand).unwrap(), expected_debug);

                // `INIT_METHOD` is special-cased in each type's own `op_get`, ahead
                // of the surface-specific `*_get` free function.
                ty.get(strand, init_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));

                // A field outside both surfaces falls through to a `Field` error.
                let err = ty.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field, "{expected_debug}");

                // `op_mcall`'s own `INIT_METHOD` arm: since these marker types are
                // reached directly (not via a concrete object's delegated dispatch, which
                // would prepend the receiver), the "self" argument has to be
                // supplied explicitly.
                method!(strand, &ty, init_sym, &mut out, &ty).await.unwrap();
            }
        });
    }

    // ── Free `sink_get`/`iterable_get`/`sinkable_get`/`iterable_sinkable_get`
    //    dispatch helpers, called directly with an arbitrary receiver ──────────

    #[test]
    fn free_dispatch_helpers_route_by_surface_and_reject_unrelated_fields() {
        with_vm(async |strand, [mut val, mut out]| {
            Output::set(strand, &mut val, 5i64);
            let map_sym = Sym::well_known(sym::MAP);
            let put_sym = Sym::well_known(sym::PUT);
            let bogus_sym = Sym::well_known(sym::LEN);

            sink_get(strand, &val, put_sym, Slot::reborrow(&mut out)).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            let err = sink_get(strand, &val, map_sym, Slot::reborrow(&mut out)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);

            iterable_get(strand, &val, map_sym, Slot::reborrow(&mut out)).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            let err = iterable_get(strand, &val, put_sym, Slot::reborrow(&mut out)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);

            sinkable_get(strand, &val, put_sym, Slot::reborrow(&mut out)).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            let err = sinkable_get(strand, &val, map_sym, Slot::reborrow(&mut out)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);

            iterable_sinkable_get(strand, &val, map_sym, Slot::reborrow(&mut out)).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            iterable_sinkable_get(strand, &val, put_sym, Slot::reborrow(&mut out)).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            let err = iterable_sinkable_get(strand, &val, bogus_sym, Slot::reborrow(&mut out))
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);
        });
    }

    // ── `Chain`/`Zip`/`Take`/`Skip`/`Enumerate`/`Kv`: `op_debug`/`op_get`/
    //    `op_mcall` dispatched through the real `Value`, not the associated fns ─

    #[test]
    fn wrapper_iterators_op_debug_op_get_and_op_mcall_dispatch_through_real_values() {
        with_fixture_vm(
            async |strand,
                   [
                mut arr,
                mut arr2,
                mut it_a,
                mut it_b,
                mut wrapper,
                mut out,
                mut item,
            ]| {
                let next_sym = Sym::well_known(sym::NEXT);
                let init_sym = Sym::well_known(sym::INIT_METHOD);
                let bogus_sym = strand.vm().state::<IterFixtureState>().bogus_sym;

                // Chain
                make_int_array(strand, &[1, 2], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it_a).await.unwrap();
                create_chain(strand, vec![it_a.take()], &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Chain>");
                wrapper.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                let err = wrapper.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
                method!(strand, &wrapper, init_sym, &mut out).await.unwrap();
                method!(strand, &wrapper, next_sym, &mut item)
                    .await
                    .unwrap();
                assert_eq!(item.to_i64(strand).unwrap(), 1);

                // Zip
                make_int_array(strand, &[10, 20], Slot::reborrow(&mut arr));
                make_int_array(strand, &[30, 40], Slot::reborrow(&mut arr2));
                arr.iter(strand, &mut it_a).await.unwrap();
                arr2.iter(strand, &mut it_b).await.unwrap();
                create_zip(strand, vec![it_a.take(), it_b.take()], &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Zip>");
                wrapper.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                let err = wrapper.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
                method!(strand, &wrapper, init_sym, &mut out).await.unwrap();
                method!(strand, &wrapper, next_sym, &mut item)
                    .await
                    .unwrap();
                item.index(strand, 0i64, &mut out).unwrap();
                assert_eq!(out.to_i64(strand).unwrap(), 10);

                // Take
                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it_a).await.unwrap();
                create_take(strand, it_a.take(), 2, &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Take>");
                wrapper.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                let err = wrapper.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
                method!(strand, &wrapper, init_sym, &mut out).await.unwrap();
                method!(strand, &wrapper, next_sym, &mut item)
                    .await
                    .unwrap();
                assert_eq!(item.to_i64(strand).unwrap(), 1);

                // Skip
                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it_a).await.unwrap();
                create_skip(strand, it_a.take(), 1, &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Skip>");
                wrapper.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                let err = wrapper.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
                method!(strand, &wrapper, init_sym, &mut out).await.unwrap();
                method!(strand, &wrapper, next_sym, &mut item)
                    .await
                    .unwrap();
                assert_eq!(item.to_i64(strand).unwrap(), 2);

                // Enumerate
                make_int_array(strand, &[5, 6], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it_a).await.unwrap();
                create_enumerate(strand, it_a.take(), &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Enumerate>");
                wrapper.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                let err = wrapper.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
                method!(strand, &wrapper, init_sym, &mut out).await.unwrap();
                method!(strand, &wrapper, next_sym, &mut item)
                    .await
                    .unwrap();
                item.index(strand, 0i64, &mut out).unwrap();
                assert_eq!(out.to_i64(strand).unwrap(), 0);

                // Kv (over a plain int source -- `.kv()` only tags spread behavior,
                // it doesn't transform `next` output; see `kv_forwards_next_unchanged`).
                make_int_array(strand, &[1, 2], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it_a).await.unwrap();
                create_kv(strand, it_a.take(), &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Kv>");
                // `Kv::op_iter` (unlike `next`, which every other case above already
                // exercises via `method!`) is only reached through `Value::iter`.
                wrapper.iter(strand, &mut out).await.unwrap();
                wrapper.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                let err = wrapper.get(strand, bogus_sym, &mut out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
                method!(strand, &wrapper, init_sym, &mut out).await.unwrap();
                method!(strand, &wrapper, next_sym, &mut item)
                    .await
                    .unwrap();
                assert_eq!(item.to_i64(strand).unwrap(), 1);
            },
        );
    }

    // ── `Null`: `op_debug`/`op_get`/`op_mcall` dispatched through the real
    //    `Value` ───────────────────────────────────────────────────────────────

    #[test]
    fn null_op_debug_op_get_and_op_mcall_dispatch_through_real_value() {
        with_vm(async |strand, [mut null_val, mut out]| {
            let next_sym = Sym::well_known(sym::NEXT);
            let put_sym = Sym::well_known(sym::PUT);
            let init_sym = Sym::well_known(sym::INIT_METHOD);

            Output::set(strand, &mut null_val, &strand.singletons().null);
            assert_eq!(null_val.to_debug(strand).unwrap(), "<std.Null>");

            // `INIT_METHOD` is special-cased ahead of both surfaces.
            null_val.get(strand, init_sym, &mut out).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            // An `Iterable`-surface field routes through `iter_get`.
            null_val.get(strand, next_sym, &mut out).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));
            // A `Sinkable`-surface field routes through the dedicated
            // `classify(..) == Sinkable` branch to `sink_get`.
            null_val.get(strand, put_sym, &mut out).unwrap();
            assert!(out.to_debug(strand).unwrap().contains("bound method"));

            // `Null::op_mcall`'s `INIT_METHOD` arm is reached directly (there's no
            // delegated dispatch to prepend a receiver), so it needs an explicit
            // "self" argument, same as the abstract marker types above.
            method!(strand, &null_val, init_sym, &mut out, &null_val)
                .await
                .unwrap();
            method!(strand, &null_val, put_sym, &mut out, 5i64)
                .await
                .unwrap();
            let err = method!(strand, &null_val, next_sym, &mut out)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::IterStop);
        });
    }

    // ── `Map`/`Filter` instances: `op_debug`/`op_get`/`op_mcall` dispatched
    //    through the real `Value`, exercising both the `has_input` (`iter_get`/
    //    `iter_mcall`) and `has_output` (`sink_get`/`sink_mcall`) branches ──────

    #[test]
    fn map_and_filter_instances_op_debug_op_get_and_op_mcall_dispatch_through_real_values() {
        with_fixture_vm(
            async |strand,
                   [
                mut arr,
                mut it,
                mut func,
                mut map_val,
                mut filter_val,
                mut sink_arr,
                mut sink,
                mut out,
            ]| {
                let init_sym = Sym::well_known(sym::INIT_METHOD);
                let next_sym = Sym::well_known(sym::NEXT);
                let put_sym = Sym::well_known(sym::PUT);

                // Map, has_input (`iter_get`/`iter_mcall`).
                make_int_array(strand, &[3, 4], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let doubled = v.to_i64(strand)? * 2;
                        Output::set(strand, &mut out, doubled);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                create_map(strand, &it, Slot::reborrow(&mut func), &mut map_val);
                assert_eq!(map_val.to_debug(strand).unwrap(), "<std.Map>");
                map_val.get(strand, init_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                map_val.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                // `Map::op_mcall`'s `INIT_METHOD` arm is reached directly (no
                // delegated dispatch prepends a receiver), so it needs an
                // explicit "self" argument.
                method!(strand, &map_val, init_sym, &mut out, &map_val)
                    .await
                    .unwrap();
                method!(strand, &map_val, next_sym, &mut out).await.unwrap();
                assert_eq!(out.to_i64(strand).unwrap(), 6);

                // Map, has_output (`sink_get`/`sink_mcall`).
                make_int_array(strand, &[], Slot::reborrow(&mut sink_arr));
                sink_arr.sink(strand, &mut sink).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let doubled = v.to_i64(strand)? * 2;
                        Output::set(strand, &mut out, doubled);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                create_premap(strand, &sink, Slot::reborrow(&mut func), &mut map_val);
                map_val.get(strand, put_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                method!(strand, &map_val, init_sym, &mut out, &map_val)
                    .await
                    .unwrap();
                method!(strand, &map_val, put_sym, &mut out, 5i64)
                    .await
                    .unwrap();
                assert_eq!(collect_ints(strand, &sink_arr).await.unwrap(), vec![10]);

                // Filter, has_input (`iter_get`/`iter_mcall`).
                make_int_array(strand, &[1, 2, 3, 4], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let cond = v.to_i64(strand)? % 2 == 0;
                        Output::set(strand, &mut out, cond);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                create_filter(strand, &it, Slot::reborrow(&mut func), &mut filter_val);
                assert_eq!(filter_val.to_debug(strand).unwrap(), "<std.Filter>");
                filter_val.get(strand, init_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                filter_val.get(strand, next_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                method!(strand, &filter_val, init_sym, &mut out, &filter_val)
                    .await
                    .unwrap();
                method!(strand, &filter_val, next_sym, &mut out)
                    .await
                    .unwrap();
                assert_eq!(out.to_i64(strand).unwrap(), 2);

                // Filter, has_output (`sink_get`/`sink_mcall`).
                make_int_array(strand, &[], Slot::reborrow(&mut sink_arr));
                sink_arr.sink(strand, &mut sink).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let cond = v.to_i64(strand)? % 2 == 0;
                        Output::set(strand, &mut out, cond);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                create_prefilter(strand, &sink, Slot::reborrow(&mut func), &mut filter_val);
                filter_val.get(strand, put_sym, &mut out).unwrap();
                assert!(out.to_debug(strand).unwrap().contains("bound method"));
                method!(strand, &filter_val, init_sym, &mut out, &filter_val)
                    .await
                    .unwrap();
                for v in [1i64, 2, 3, 4] {
                    method!(strand, &filter_val, put_sym, &mut out, v)
                        .await
                        .unwrap();
                }
                assert_eq!(collect_ints(strand, &sink_arr).await.unwrap(), vec![2, 4]);
            },
        );
    }

    // ── `MapType`/`FilterType`: `op_debug`/`op_call` dispatched through the real
    //    `Value` (the `std.Map`/`std.Filter` callables) ─────────────

    #[test]
    fn map_type_and_filter_type_op_debug_and_op_call() {
        with_fixture_vm(
            async |strand, [mut arr, mut it, mut func, mut ty, mut out]| {
                Output::set(strand, &mut ty, &strand.singletons().map_iter);
                assert_eq!(ty.to_debug(strand).unwrap(), "<type std.Map>");

                make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let doubled = v.to_i64(strand)? * 2;
                        Output::set(strand, &mut out, doubled);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                call!(strand, &ty, &mut out, &func, &it).await.unwrap();
                assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![2, 4, 6]);

                Output::set(strand, &mut ty, &strand.singletons().filter_iter);
                assert_eq!(ty.to_debug(strand).unwrap(), "<type std.Filter>");

                arr.iter(strand, &mut it).await.unwrap();
                make_native_fn(
                    strand,
                    |strand, args, mut out| {
                        let ([v], []) = unpack!(strand, args, 1, 0)?;
                        let cond = v.to_i64(strand)? % 2 == 0;
                        Output::set(strand, &mut out, cond);
                        Ok(())
                    },
                    Slot::reborrow(&mut func),
                );
                call!(strand, &ty, &mut out, &func, &it).await.unwrap();
                assert_eq!(collect_ints(strand, &out).await.unwrap(), vec![2]);
            },
        );
    }

    // ── `Take`/`Skip`/`Enumerate`/`Kv`/`Map`/`Filter` `op_type`, plus a few
    //    remaining branches not reached above ─────────────────────────────────

    #[test]
    fn wrapper_op_type_reports_the_abstract_iter_singleton() {
        // The `func`/`pred` value is never invoked below (`is_instance_of`/`sink`
        // only exercise `op_type`/`op_sink`), so a plain `Int` stands in for it --
        // no need for a working callable.
        with_fixture_vm(async |strand, [mut arr, mut it, mut func, mut wrapper]| {
            make_int_array(strand, &[1], Slot::reborrow(&mut arr));

            arr.iter(strand, &mut it).await.unwrap();
            create_take(strand, it.take(), 1, &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));

            arr.iter(strand, &mut it).await.unwrap();
            create_skip(strand, it.take(), 0, &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));

            arr.iter(strand, &mut it).await.unwrap();
            create_enumerate(strand, it.take(), &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));

            arr.iter(strand, &mut it).await.unwrap();
            create_kv(strand, it.take(), &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));

            // Map/Filter, has_input (no has_output): `op_type` reports `map_iter`/
            // `filter_iter`.
            arr.iter(strand, &mut it).await.unwrap();
            Output::set(strand, &mut func, 0i64);
            create_map(strand, &it, Slot::reborrow(&mut func), &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));

            arr.iter(strand, &mut it).await.unwrap();
            Output::set(strand, &mut func, 0i64);
            create_filter(strand, &it, Slot::reborrow(&mut func), &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));

            // Map/Filter, has_output (no has_input): `op_type` falls through to the
            // `output_iter` singleton, and `op_sink` (via `Value::sink`) succeeds.
            Output::set(strand, &mut func, 0i64);
            create_premap(strand, &it, Slot::reborrow(&mut func), &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));
            wrapper.sink(strand, &mut func).await.unwrap();

            Output::set(strand, &mut func, 0i64);
            create_prefilter(strand, &it, Slot::reborrow(&mut func), &mut wrapper);
            assert!(wrapper.is_instance_of(strand, TypeObject::Value));
            wrapper.sink(strand, &mut func).await.unwrap();
        });
    }

    // ── `chomp`/`crimp` adapters ────────────────────────────────────────────

    #[test]
    fn chomp_and_crimp_adapters_report_their_own_debug_and_surface() {
        with_vm(
            async |strand, [mut arr, mut it, mut wrapper, mut sink, mut out]| {
                let init_sym = Sym::well_known(sym::INIT_METHOD);

                // Iter direction: each reports itself and satisfies `Iter`.
                make_int_array(strand, &[1], Slot::reborrow(&mut arr));
                arr.iter(strand, &mut it).await.unwrap();
                create_chomp(strand, &it, &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Chomp>");
                assert!(wrapper.is_instance_of(strand, TypeObject::Iter));
                method!(strand, &wrapper, init_sym, &mut out, &wrapper)
                    .await
                    .unwrap();

                arr.iter(strand, &mut it).await.unwrap();
                create_crimp(strand, &it, None, &mut wrapper).unwrap();
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Crimp>");
                assert!(wrapper.is_instance_of(strand, TypeObject::Iter));
                method!(strand, &wrapper, init_sym, &mut out, &wrapper)
                    .await
                    .unwrap();

                // Sink direction: each reports itself and satisfies `Sink`.
                make_int_array(strand, &[], Slot::reborrow(&mut arr));
                arr.sink(strand, &mut sink).await.unwrap();
                create_prechomp(strand, &sink, &mut wrapper);
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Prechomp>");
                assert!(wrapper.is_instance_of(strand, TypeObject::Sink));
                method!(strand, &wrapper, init_sym, &mut out, &wrapper)
                    .await
                    .unwrap();

                create_precrimp(strand, &sink, None, &mut wrapper).unwrap();
                assert_eq!(wrapper.to_debug(strand).unwrap(), "<std.Precrimp>");
                assert!(wrapper.is_instance_of(strand, TypeObject::Sink));
                method!(strand, &wrapper, init_sym, &mut out, &wrapper)
                    .await
                    .unwrap();
            },
        );
    }

    #[test]
    fn crimp_rejects_a_non_string_terminator_at_construction() {
        // Validating in the constructor rather than on the first item means a
        // bad terminator surfaces where it was written, not wherever the
        // pipeline happens to be drained.
        with_vm(async |strand, [mut arr, mut it, mut term, mut wrapper]| {
            make_int_array(strand, &[1], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut it).await.unwrap();
            Output::set(strand, &mut term, 5i64);
            let err = create_crimp(strand, &it, Some(&term), &mut wrapper).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn zip_with_no_sources_yields_nothing() {
        with_vm(async |strand, [mut zip, mut item]| {
            create_zip(strand, vec![], &mut zip);
            assert!(!zip.next(strand, &mut item).await.unwrap());
        });
    }

    #[test]
    fn kv_op_spread_reports_non_pairs_context_as_positional() {
        with_vm(async |strand, [mut arr, mut inner, mut kv]| {
            make_int_array(strand, &[1, 2, 3], Slot::reborrow(&mut arr));
            arr.iter(strand, &mut inner).await.unwrap();
            create_kv(strand, inner.take(), &mut kv);

            let kv_value: &Value = &kv;
            let mut sink = CollectSpread::default();
            strand
                .builtin_types()
                .kv_iter
                .cast(kv_value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    Kv::op_spread(recv, strand, SpreadContext::Sequence, &mut sink)
                        .await
                        .unwrap();
                })
                .await;
            assert_eq!(sink.positional, vec![1, 2, 3]);
            assert!(sink.pairs.is_empty());
        });
    }

    // ── `Sinkable`/`Sink`'s own `op_mcall` catch-all `Field` error, and
    //    `sink_mcall`'s `SINK` arm (-> `sinkable_mcall`) ────────────────────────

    #[test]
    fn sinkable_and_sink_op_mcall_reject_unrelated_methods() {
        with_vm(async |strand, [mut ty, mut out]| {
            let bogus_sym = Sym::well_known(sym::LEN);

            Output::set(strand, &mut ty, &strand.singletons().sinkable);
            let err = method!(strand, &ty, bogus_sym, &mut out).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);

            Output::set(strand, &mut ty, &strand.singletons().output_iter);
            let err = method!(strand, &ty, bogus_sym, &mut out).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);
        });
    }

    #[test]
    fn sink_mcall_sink_method_forwards_through_sinkable_mcall() {
        with_vm(async |strand, [mut arr, mut sink, mut out]| {
            let sink_sym = Sym::well_known(sym::SINK);
            make_int_array(strand, &[], Slot::reborrow(&mut arr));
            arr.sink(strand, &mut sink).await.unwrap();
            // `.sink()` on an already-materialized sink is idempotent, and (since
            // the sink object's own `op_mcall` routes through `iter::sink_mcall`)
            // exercises `sink_mcall`'s `SINK` arm -> `sinkable_mcall`.
            method!(strand, &sink, sink_sym, &mut out).await.unwrap();
        });
    }

    // ── `MapType`/`FilterType::op_type` (the type of the `std.Map`/
    //    `std.Filter` callables themselves, not their instances) ─────────

    #[test]
    fn map_type_and_filter_type_op_type_report_the_universal_type_object() {
        with_vm(async |strand, [mut ty]| {
            Output::set(strand, &mut ty, &strand.singletons().map_iter);
            assert!(ty.is_instance_of(strand, TypeObject::Value));

            Output::set(strand, &mut ty, &strand.singletons().filter_iter);
            assert!(ty.is_instance_of(strand, TypeObject::Value));
        });
    }
}
