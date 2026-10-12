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
            Delegated, Dispatch, Inspect, Member, Protocol, Recv, Spread, SpreadContext,
            instance_get_fallback, instance_mcall_fallback, is_special_mcall, members,
            recv_special_mcall,
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
const ITER_ONLY_METHODS: &[sym::Tag] = &[sym::NEXT, sym::COUNT, sym::KV, sym::SPREAD_METHOD];

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
        Method(sym::SPREAD_METHOD),
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

/// Spreads the items of `value`'s iterator positionally.
///
/// Native objects inherit this from `Iter`; raw protocol types whose spreading
/// is their iteration forward to it by hand.
pub(crate) async fn spread_iter<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: impl Input<'v>,
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async move |strand, [mut root, mut iter, mut item]| {
            Output::set(strand, Slot::reborrow(&mut root), value);
            root.op_iter(strand, Slot::reborrow(&mut iter)).await?;
            let mut count = 0usize;
            while iter.op_next(strand, Slot::reborrow(&mut item)).await? {
                sink.positional(strand, Slot::reborrow(&mut item))?;
                count += 1;
                if count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap_gc()?;
                }
            }
            Ok(())
        })
        .await
}

/// Unpacks `sig` positionally from a cursor, returning the cursor after the
/// items bound.
///
/// `next` yields the item at a cursor and the cursor following it. Keys take
/// their defaults, since a sequence has no keyed items, and the caller stores
/// any rest.
pub(crate) fn unpack_cursor<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    mut index: usize,
    mut next: impl FnMut(&mut Strand<'v, 's>, usize) -> Option<(usize, Value<'v>)>,
) -> Result<'v, 's, usize> {
    let pos_count = sig.required + sig.optional.len();
    for i in 0..(pos_count + sig.keys.len()) {
        if i < pos_count {
            if let Some((next_index, value)) = next(strand, index) {
                out.at(i).store(value);
                index = next_index;
            } else if i >= sig.required {
                out.at(i).store(sig.optional[i - sig.required].dup());
            } else {
                return Err(Error::missing_positional(strand, i));
            }
        } else {
            let key = &sig.keys[i - pos_count];
            if let Some(default) = &key.default {
                out.at(i).store(default.dup());
            } else {
                return Err(match &key.kind {
                    sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                    sig::UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
                });
            }
        }
    }
    if sig.pos_rest() == crate::bytecode::Rest::None && next(strand, index).is_some() {
        return Err(Error::unexpected_positional(strand, pos_count));
    }
    Ok(index)
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
        instance_get_fallback(strand, rcvr, field, out)
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
                    item.op_lt(strand, &out)?.to_bool(strand)?
                } else {
                    out.op_lt(strand, &item)?.to_bool(strand)?
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
                    pred_out.to_bool(strand)?
                } else {
                    item.to_bool(strand)?
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
                if pred_out.to_bool(strand)? {
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
    if is_special_mcall(method.tag()) {
        return instance_mcall_fallback(strand, rcvr, method, args, out).await;
    }

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
            is_final: false,
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
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, &this, method, args, out).await;
        }

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
            is_final: false,
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
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, &this, method, args, out).await;
        }

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
        instance_get_fallback(strand, rcvr, field, out)
    }
}

pub(crate) async fn sink_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if is_special_mcall(method.tag()) {
        return instance_mcall_fallback(strand, rcvr, method, args, out).await;
    }

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
        instance_get_fallback(strand, rcvr, field, out)
    }
}

pub(crate) async fn iterable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if is_special_mcall(method.tag()) {
        return instance_mcall_fallback(strand, rcvr, method, args, out).await;
    }

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
        instance_get_fallback(strand, rcvr, field, out)
    }
}

pub(crate) async fn sinkable_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if is_special_mcall(method.tag()) {
        return instance_mcall_fallback(strand, rcvr, method, args, out).await;
    }

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
        instance_get_fallback(strand, rcvr, field, out)
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
    if is_special_mcall(method.tag()) {
        return instance_mcall_fallback(strand, rcvr, method, args, out).await;
    }

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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
    }
}

impl<'v> Protocol<'v> for Kv<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().kv)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<std.Kv>")
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let unpack = sig::Unpack {
            required: 2,
            optional: vec![],
            keys: vec![],
            sym_index: vec![],
            variadic: Variadic::NONE,
        };
        strand
            .with_slots(
                async move |strand, [mut iter, mut item, mut key, mut value]| {
                    let source = this.borrow(strand)?.source.dup();
                    source.iter(strand, &mut iter).await?;
                    while iter.next(strand, &mut item).await? {
                        let cells = [UnsafeCell::new(Value::NIL), UnsafeCell::new(Value::NIL)];
                        item.op_unpack(strand, &unpack, unsafe { Slots::new(&cells) })
                            .await?;
                        key.store(unsafe { (*cells[0].get()).take() });
                        value.store(unsafe { (*cells[1].get()).take() });
                        sink.keyed(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut value))?;
                    }
                    Ok(())
                },
            )
            .await
    }
}

impl<'v> Protocol<'v> for Iter {
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let Some(delegator) = this.delegator() else {
            return Err(Error::not_supported(strand));
        };
        spread_iter(strand, delegator, sink).await
    }

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
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
            || strand
                .singletons()
                .iterable
                .eq(strand, supertype)
                .unwrap_or(false)
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
            is_final: false,
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
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, &this, method, args, out).await;
        }

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
            sym::SPREAD_METHOD => {
                let ([obj], []) = unpack!(strand, args, 1, 0)?;
                Output::set(strand, out, obj);
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
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
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
        crate::fmt!(strand, w, "<type Sink>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            is_final: false,
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
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, &this, method, args, out).await;
        }

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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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
                    if res.to_bool(strand)? {
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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
                if output.to_bool(strand)? {
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
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
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
            || sings.input_iter.eq(strand, supertype).unwrap_or(false)
            || sings.output_iter.eq(strand, supertype).unwrap_or(false)
            || sings.iterable.eq(strand, supertype).unwrap_or(false)
            || sings.sinkable.eq(strand, supertype).unwrap_or(false)
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

    // Unpacks as no items, so each rest is `null` again
    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if sig.required > 0 {
            return Err(Error::missing_positional(strand, sig.required));
        }
        for (i, default) in sig.optional.iter().enumerate() {
            out.at(i).store(default.dup());
        }
        let keyed = sig.optional.len();
        for (i, key) in sig.keys.iter().enumerate() {
            let Some(default) = &key.default else {
                return Err(match &key.kind {
                    sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                    sig::UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
                });
            };
            out.at(keyed + i).store(default.dup());
        }
        if let Some(i) = sig.pos_rest_slot() {
            Output::set(strand, out.at(i), &this);
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        spread_iter(strand, this.clone(), sink).await
    }
}
