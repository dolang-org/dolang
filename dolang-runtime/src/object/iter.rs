use std::ops::ControlFlow;

mod combinators;

pub(crate) use combinators::{Batches, FlatMap, SkipWhile, TakeWhile, Tuples, Windows};

use crate::{
    arg::{Arg, Args},
    bytecode::Variadic,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    io::IoCaps,
    object::{
        BoundMethod,
        interface::Request,
        io::{provide_bridge, recv_get, recv_mcall},
        protocol::{
            Delegated, Dispatch, Inspect, Member, Protocol, Recv, Spread, SpreadContext, Unpack,
            abstract_call, abstract_init, instance_get_fallback, is_special_mcall, members,
            recv_special_mcall,
        },
        tuple,
    },
    sig,
    strand::{Strand, with_scan, with_slots_dynamic},
    sym::{self, Sym},
    unpack,
    value::{
        BinEmbryo, Input, Output, Slot, Slots, TypeObject, Value, ValueCell, view::View,
        with_u8_slice,
    },
    vm::{AllocExt, Vm},
    with_slots,
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
    sym::KV,
    sym::FIND,
    sym::MIN,
    sym::MAX,
    sym::FLAT_MAP,
    sym::FLATTEN,
    sym::TAKE_WHILE,
    sym::SKIP_WHILE,
    sym::BATCHES,
    sym::TUPLES,
    sym::WINDOWS,
    sym::GROUP_BY,
];

/// Methods of [`Iter`] that [`Iterable`] deliberately does *not* forward.
///
/// - `next` is stateful: a container has no iteration position of its own, so
///   forwarding it would mint a fresh iterator per call and a `while` loop
///   over `next` would spin on the first element forever.
/// - `count` is `len` spelled expensively, and `Dict`/`dict_view`/`Pack`
///   already define `count` with unrelated semantics.
/// - `buffer` would buffer a fresh iterator on every call.
const ITER_ONLY_METHODS: &[sym::Tag] = &[sym::NEXT, sym::COUNT, sym::BUFFER, sym::SPREAD_METHOD];

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
        Method(sym::ITER_METHOD),
        Method(sym::ITER),
        Method(sym::ALL),
        Method(sym::ANY),
        Method(sym::BUFFER),
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
        Method(sym::FLAT_MAP),
        Method(sym::FLATTEN),
        Method(sym::TAKE_WHILE),
        Method(sym::SKIP_WHILE),
        Method(sym::BATCHES),
        Method(sym::TUPLES),
        Method(sym::WINDOWS),
        Method(sym::GROUP_BY),
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
        Method(sym::KV),
        Method(sym::FIND),
        Method(sym::MIN),
        Method(sym::MAX),
        Method(sym::FLAT_MAP),
        Method(sym::FLATTEN),
        Method(sym::TAKE_WHILE),
        Method(sym::SKIP_WHILE),
        Method(sym::BATCHES),
        Method(sym::TUPLES),
        Method(sym::WINDOWS),
        Method(sym::GROUP_BY),
    ]
}

fn sink_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    members![
        Method(sym::SINK_METHOD),
        Method(sym::SINK),
        Method(sym::PUT),
        Method(sym::PREMAP),
        Method(sym::PREFILTER),
        Method(sym::PRECHOMP),
        Method(sym::PRECRIMP),
    ]
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
    with_slots!(strand, |mut root, mut iter, mut item| {
        strand.store(Slot::reborrow(&mut root), value);
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

/// The least or greatest item, by `key` of each if given. The first of equal
/// items wins.
async fn iter_extrema<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    default: Option<Slot<'v, 'a>>,
    key: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
    is_min: bool,
) -> Result<'v, 's, ()> {
    let keyed = key.is_some();
    with_slots!(strand, |mut item, mut func, mut best, mut candidate| {
        if let Some(mut key) = key {
            strand.store(&mut func, &mut key);
        }
        if !obj.next(strand, &mut out).await? {
            if let Some(mut default) = default {
                strand.store(&mut out, &mut default);
                return Ok(());
            }
            return Err(Error::iter_stop(strand));
        }
        if keyed {
            call!(strand, &func, &out, &mut best)?;
        }
        while obj.next(strand, &mut item).await? {
            let replace = if keyed {
                call!(strand, &func, &item, &mut candidate)?;
                if is_min {
                    candidate.lt(strand, &best)?
                } else {
                    best.lt(strand, &candidate)?
                }
            } else if is_min {
                item.lt(strand, &out)?
            } else {
                out.lt(strand, &item)?
            };
            if replace {
                strand.swap(Slot::reborrow(&mut out), &mut item);
                if keyed {
                    strand.swap(Slot::reborrow(&mut best), &mut candidate);
                }
            }
            strand.check_trap_gc()?;
        }
        Ok(())
    })
}

async fn iter_all_any<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    pred: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
    want_all: bool,
) -> Result<'v, 's, ()> {
    let has_pred = pred.is_some();
    with_slots!(strand, |mut item, mut pred_fn, mut pred_out| {
        if let Some(mut pred) = pred {
            strand.store(&mut pred_fn, &mut pred);
        }
        while obj.next(strand, &mut item).await? {
            let passed = if has_pred {
                call!(strand, &pred_fn, &item, &mut pred_out)?;
                pred_out.to_bool(strand)?
            } else {
                item.to_bool(strand)?
            };
            if want_all && !passed {
                strand.store(&mut out, Value::FALSE);
                return Ok(());
            }
            if !want_all && passed {
                strand.store(&mut out, Value::TRUE);
                return Ok(());
            }
            strand.check_trap_gc()?;
        }
        strand.store(&mut out, Value::from_bool(want_all));
        Ok(())
    })
}

async fn iter_count<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |mut item| {
        let mut count = 0usize;
        while obj.next(strand, &mut item).await? {
            count += 1;
            if count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap_gc()?;
            }
        }
        let value = i64::try_from(count).map_err(|_| Error::overflow(strand))?;
        strand.store(out, value);
        Ok(())
    })
}

async fn iter_fold<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    obj: &Value<'v>,
    mut init: Slot<'v, 'a>,
    mut func: Slot<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |mut acc, mut next_acc, mut item, mut func_slot| {
        strand.store(&mut acc, &mut init);
        strand.store(&mut func_slot, &mut func);
        while obj.next(strand, &mut item).await? {
            call!(strand, &func_slot, &acc, &item, &mut next_acc)?;
            strand.swap(Slot::reborrow(&mut acc), &mut next_acc);
            strand.check_trap_gc()?;
        }
        strand.store(&mut out, &mut acc);
        Ok(())
    })
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
    with_slots!(strand, |mut item, mut pred_fn, mut pred_out| {
        strand.store(&mut pred_fn, &mut pred);
        while obj.next(strand, &mut item).await? {
            call!(strand, &pred_fn, &item, &mut pred_out)?;
            if pred_out.to_bool(strand)? {
                strand.store(&mut out, &mut item);
                return Ok(());
            }
            strand.check_trap_gc()?;
        }
        if let Some(mut default) = default {
            strand.store(&mut out, &mut default);
            return Ok(());
        }
        if let Some(or_else) = or_else {
            return call!(strand, or_else, out);
        }
        Err(Error::runtime(strand, "find: no matching item"))
    })
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
        strand.store(&mut out, &mut default);
        return Ok(());
    }
    if let Some(or_else) = or_else {
        return call!(strand, or_else, out);
    }
    Err(Error::iter_stop(strand))
}

/// Collect the sources for a `chain` or `zip` into `sources`, one slot per argument.
///
/// The leading argument is the method receiver, which arrives already
/// materialized as an `Iter`. The rest are arbitrary iterables passed by the
/// caller, so each needs an `iter` of its own.
async fn collect_sources<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut args: Args<'v, '_>,
    sources: &mut Slots<'v, '_>,
) -> Result<'v, 's, ()> {
    let Some(Arg::Pos(rcvr)) = args.next() else {
        return Err(Error::type_error(strand, "expected receiver argument"));
    };
    strand.store(sources.at(0), &rcvr);
    for (i, arg) in args.enumerate() {
        let slot = match arg {
            Arg::Pos(slot) => slot,
            Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
        };
        slot.iter(strand, sources.at(i + 1)).await?;
    }
    Ok(())
}

pub(crate) struct Iter;
pub(crate) struct Iterable;
pub(crate) struct Sinkable;

unsafe impl Collect for Iter {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for Iterable {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for Sinkable {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

pub(crate) struct Sink;

unsafe impl Collect for Sink {
    const CYCLIC: bool = false;
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
        with_slots!(strand, |mut delegator| {
            strand.store(Slot::reborrow(&mut delegator), rcvr);
            let receiver = &strand.vm().singletons().input_iter;
            Delegated::new(receiver, &delegator)
                .op_mcall(strand, method, args, out)
                .await
        })
    }
}

/// [`iter_mcall`] for an iterator that can already unpack: `buffer` returns the
/// receiver instead of wrapping it.
pub(crate) async fn unpack_iter_mcall<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    if method.tag() == sym::BUFFER {
        unpack!(strand, args =>);
        strand.store(out, rcvr);
        Ok(())
    } else {
        iter_mcall(strand, rcvr, method, args, out).await
    }
}

impl<'v> Protocol<'v> for Iterable {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Iterable";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
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
        if method.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            return abstract_init(strand, &this, args);
        }
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, this, method, args, out).await;
        }

        if let Some(delegator) = this.delegator() {
            args.prepend_self(strand, delegator);
        }
        match method.tag() {
            sym::ITER => {
                unpack!(strand, args => obj);
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

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Iterable"))
    }
}

impl<'v> Protocol<'v> for Sinkable {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Sinkable";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
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
        if method.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            return abstract_init(strand, &this, args);
        }
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, this, method, args, out).await;
        }

        if let Some(delegator) = this.delegator() {
            args.prepend_self(strand, delegator);
        }
        match method.tag() {
            sym::SINK => {
                unpack!(strand, args => obj);
                obj.sink(strand, out).await
            }
            // `x.foo(..)` on a `Sinkable` means `x.sink().foo(..)`.
            tag if classify(tag) == Some(Surface::Sinkable) => {
                forward(strand, args, out, method, Surface::Sinkable).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Sinkable"))
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
    if method.tag() == sym::SINK {
        sinkable_mcall(strand, rcvr, method, args, out).await
    } else {
        with_slots!(strand, |mut delegator| {
            strand.store(Slot::reborrow(&mut delegator), rcvr);
            let receiver = &strand.vm().singletons().output_iter;
            Delegated::new(receiver, &delegator)
                .op_mcall(strand, method, args, out)
                .await
        })
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
    with_slots!(strand, |mut delegator| {
        strand.store(Slot::reborrow(&mut delegator), rcvr);
        let receiver = &strand.vm().singletons().iterable;
        Delegated::new(receiver, &delegator)
            .op_mcall(strand, method, args, out)
            .await
    })
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
    with_slots!(strand, |mut delegator| {
        strand.store(Slot::reborrow(&mut delegator), rcvr);
        let receiver = &strand.vm().singletons().sinkable;
        Delegated::new(receiver, &delegator)
            .op_mcall(strand, method, args, out)
            .await
    })
}

/// Run an `Iterable`/`Sinkable` method as `Iter`'s/`Sink`'s on the value the
/// receiver materializes.
///
/// `args` arrives with the receiver as the leading positional argument (either
/// prepended for delegated dispatch, or written explicitly as in `Iterable.map(x, f)`).
/// Popping it leaves exactly the method's own arguments behind. The method isn't
/// looked up on the materialized value, which need only implement `(next)` or
/// `(put)`: `Iter`'s or `Sink`'s implementation wraps it directly, as
/// `Iter.map(x.iter(), f)` does.
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
    with_slots!(strand, |mut target| {
        match surface {
            Surface::Iterable => {
                obj.iter(strand, &mut target).await?;
                iter_mcall(strand, &target, method, args, out).await
            }
            Surface::Sinkable => {
                obj.sink(strand, &mut target).await?;
                sink_mcall(strand, &target, method, args, out).await
            }
        }
    })
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Chain";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_slots!(strand, |mut source| {
            loop {
                {
                    let borrow = this.borrow(strand)?;
                    match borrow.sources.get(borrow.index) {
                        Some(next) => strand.store(&mut source, next),
                        None => return Ok(false),
                    }
                }
                if source.next(strand, Slot::reborrow(&mut out)).await? {
                    return Ok(true);
                }
                this.borrow_mut(strand)?.index += 1;
            }
        })
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Zip";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        let len = this.borrow(strand)?.sources.len();
        if len == 0 {
            return Ok(false);
        }
        // The first `len` slots root the sources, the rest their items
        with_slots_dynamic!(strand, 2 * len, |slots| {
            let mut slots: Vec<_> = slots.into_iter().collect();
            let (sources, items) = slots.split_at_mut(len);
            {
                let borrow = this.borrow(strand)?;
                for (slot, source) in sources.iter_mut().zip(borrow.sources.iter()) {
                    strand.store(slot, source);
                }
            }
            for (source, item) in sources.iter().zip(items) {
                if !source.next(strand, Slot::reborrow(item)).await? {
                    return Ok(false);
                }
            }
            let embryo = tuple::embryo(strand, len);
            embryo.finish_moving(strand, slots.drain(len..), out);
            Ok(true)
        })
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

/// What `Iter.(spread)` returns: spreads `source`'s items positionally.
///
/// A class that overrides `(spread)` defers to the inherited behavior by
/// returning `Iter.(spread) $self`; handing back the receiver itself would
/// re-enter the override.
pub(crate) struct IterSpread<'v> {
    source: Value<'v>,
}

unsafe impl<'v> Collect for Take<'v> {
    const CYCLIC: bool = true;
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
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.source.accept(visit)?;
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.source.clear();
    }
}

unsafe impl<'v> Collect for IterSpread<'v> {
    const CYCLIC: bool = true;
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Take";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        with_slots!(strand, |mut source| {
            {
                let borrow = this.borrow(strand)?;
                if borrow.remaining == 0 {
                    return Ok(false);
                }
                strand.store(&mut source, &borrow.source);
            }
            if !source.next(strand, out).await? {
                return Ok(false);
            }
            this.borrow_mut(strand)?.remaining -= 1;
            Ok(true)
        })
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Skip";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        with_slots!(strand, |mut item, mut source| {
            loop {
                let skipping = {
                    let mut borrow = this.borrow_mut(strand)?;
                    let skipping = borrow.remaining > 0;
                    if skipping {
                        borrow.remaining -= 1;
                    }
                    strand.store(&mut source, &borrow.source);
                    skipping
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Enumerate";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        with_slots!(strand, |mut item, mut source| {
            let index = {
                let borrow = this.borrow(strand)?;
                strand.store(&mut source, &borrow.source);
                borrow.index
            };
            if !source.next(strand, &mut item).await? {
                return Ok(false);
            }
            let next_index = index
                .checked_add(1)
                .ok_or_else(|| Error::overflow(strand))?;
            tuple::create_pair(strand, index, &item, out);
            this.borrow_mut(strand)?.index = next_index;
            Ok(true)
        })
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Kv";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().kv)
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
        with_slots!(strand, |mut source,
                             mut iter,
                             mut item,
                             mut key,
                             mut value| {
            {
                let borrow = this.borrow(strand)?;
                strand.store(&mut source, &borrow.source);
            }
            source.iter(strand, &mut iter).await?;
            while iter.next(strand, &mut item).await? {
                let pair: [ValueCell<'v>; 2] = Default::default();
                let visit =
                    |visitor: &mut dyn Visit| pair.iter().try_for_each(|cell| cell.accept(visitor));
                // SAFETY: `visit` visits both cells, which only the `Unpack` writes; they are
                // read only once it is gone
                unsafe {
                    with_scan!(strand, visit, {
                        let _op = strand.start_op();
                        item.op_unpack(strand, Unpack::new(&unpack, &pair)).await?;
                        strand.store(&mut key, &pair[0]);
                        strand.store(&mut value, &pair[1]);
                    })
                }
                sink.keyed(strand, Slot::reborrow(&mut key), Slot::reborrow(&mut value))?;
            }
            Ok(())
        })
    }
}

impl<'v> Protocol<'v> for IterSpread<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "IterSpread";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, TypeObject::Value)
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut source| {
            {
                let borrow = this.borrow(strand)?;
                strand.store(&mut source, &borrow.source);
            }
            spread_iter(strand, source, sink).await
        })
    }
}

impl<'v> Protocol<'v> for Iter {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Iter";
    const TYPE_OBJECT: bool = true;

    // The default `(iter)` of a class that subclasses `Iter`: an iterator
    // iterates as itself.
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let Some(delegator) = this.delegator() else {
            return Err(Error::unsupported(strand, "iteration"));
        };
        strand.store(out, delegator);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let Some(delegator) = this.delegator() else {
            return Err(Error::unsupported(strand, "spread"));
        };
        spread_iter(strand, delegator, sink).await
    }

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
        if method.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            return abstract_init(strand, &this, args);
        }
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, this, method, args, out).await;
        }

        if let Some(delegator) = this.delegator() {
            args.prepend_self(strand, delegator);
        }
        let default = Sym::well_known(sym::DEFAULT);
        let else_key = Sym::well_known(sym::ELSE);
        match method.tag() {
            // Reached only when the class doesn't define its own `(iter)`.
            sym::ITER_METHOD => {
                unpack!(strand, args => obj);
                strand.store(out, obj);
                Ok(())
            }
            // Returning `obj` itself would re-enter an overriding `(spread)`
            // that defers here, so wrap it in a proxy that spreads its items.
            sym::SPREAD_METHOD => {
                unpack!(strand, args => obj);
                create_iter_spread(strand, &obj, out);
                Ok(())
            }
            sym::ITER => {
                unpack!(strand, args => obj);
                obj.iter(strand, out).await
            }
            sym::NEXT => {
                unpack!(strand, args => obj, ?:default, ?else_key: or_else);
                iter_next(strand, &obj, default, or_else, out).await
            }
            sym::BUFFER => {
                unpack!(strand, args => obj);
                super::buffered::create(strand, &obj, out);
                Ok(())
            }
            sym::ALL => {
                unpack!(strand, args => obj, ?pred);
                iter_all_any(strand, &obj, pred, out, true).await
            }
            sym::ANY => {
                unpack!(strand, args => obj, ?pred);
                iter_all_any(strand, &obj, pred, out, false).await
            }
            sym::COUNT => {
                unpack!(strand, args => obj);
                iter_count(strand, &obj, out).await
            }
            sym::FOLD => {
                unpack!(strand, args => obj, init, func);
                iter_fold(strand, &obj, init, func, out).await
            }
            sym::MAP => {
                unpack!(strand, args => obj, func);
                create_map(strand, &obj, func, out);
                Ok(())
            }
            sym::FILTER => {
                unpack!(strand, args => obj, pred);
                create_filter(strand, &obj, pred, out);
                Ok(())
            }
            sym::CHOMP => {
                unpack!(strand, args => obj);
                create_chomp(strand, &obj, out);
                Ok(())
            }
            sym::CRIMP => {
                unpack!(strand, args => obj, ?terminator);
                create_crimp(strand, &obj, terminator.as_deref(), out)
            }
            sym::CHAIN => create_chain_from_args(strand, args, out).await,
            sym::ZIP => create_zip_from_args(strand, args, out).await,
            sym::TAKE => {
                unpack!(strand, args => obj, count);
                let count = nonnegative_count(strand, &count)?;
                create_take(strand, &obj, count, out);
                Ok(())
            }
            sym::SKIP => {
                unpack!(strand, args => obj, count);
                let count = nonnegative_count(strand, &count)?;
                create_skip(strand, &obj, count, out);
                Ok(())
            }
            sym::ENUMERATE => {
                unpack!(strand, args => obj);
                create_enumerate(strand, &obj, out);
                Ok(())
            }
            sym::KV => {
                unpack!(strand, args => obj);
                create_kv(strand, &obj, out);
                Ok(())
            }
            sym::FIND => {
                unpack!(strand, args => obj, pred, ?:default, ?else_key: or_else);
                iter_find(strand, &obj, pred, default, or_else, out).await
            }
            sym::MIN => {
                let key = Sym::well_known(sym::KEY);
                unpack!(strand, args => obj, ?:default, ?:key);
                iter_extrema(strand, &obj, default, key, out, true).await
            }
            sym::MAX => {
                let key = Sym::well_known(sym::KEY);
                unpack!(strand, args => obj, ?:default, ?:key);
                iter_extrema(strand, &obj, default, key, out, false).await
            }
            sym::FLAT_MAP => {
                unpack!(strand, args => obj, func);
                combinators::create_flat_map(strand, &obj, Some(func), out);
                Ok(())
            }
            sym::FLATTEN => {
                unpack!(strand, args => obj);
                combinators::create_flat_map(strand, &obj, None, out);
                Ok(())
            }
            sym::TAKE_WHILE => {
                unpack!(strand, args => obj, pred);
                combinators::create_take_while(strand, &obj, pred, out);
                Ok(())
            }
            sym::SKIP_WHILE => {
                unpack!(strand, args => obj, pred);
                combinators::create_skip_while(strand, &obj, pred, out);
                Ok(())
            }
            sym::BATCHES => {
                unpack!(strand, args => obj, count);
                let count = combinators::positive_count(strand, &count)?;
                combinators::create_batches(strand, &obj, count, out);
                Ok(())
            }
            sym::TUPLES => {
                unpack!(strand, args => obj, count);
                let count = combinators::positive_count(strand, &count)?;
                combinators::create_tuples(strand, &obj, count, out);
                Ok(())
            }
            sym::WINDOWS => {
                unpack!(strand, args => obj, count);
                let count = combinators::positive_count(strand, &count)?;
                combinators::create_windows(strand, &obj, count, out);
                Ok(())
            }
            sym::GROUP_BY => {
                unpack!(strand, args => obj, key);
                combinators::iter_group_by(strand, &obj, key, out).await
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Iter"))
    }
}

impl<'v> Protocol<'v> for Sink {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Sink";
    const TYPE_OBJECT: bool = true;

    // The default `(sink)` of a class that subclasses `Sink`: a sink accepts
    // values as itself.
    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let Some(delegator) = this.delegator() else {
            return Err(Error::unsupported(strand, "sink"));
        };
        strand.store(out, delegator);
        Ok(())
    }

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
        if method.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            return abstract_init(strand, &this, args);
        }
        if is_special_mcall(method.tag()) {
            return recv_special_mcall(strand, this, method, args, out).await;
        }

        if let Some(delegator) = this.delegator() {
            args.prepend_self(strand, delegator);
        }
        match method.tag() {
            // Reached only when the class doesn't define its own `(sink)`.
            sym::SINK_METHOD => {
                unpack!(strand, args => obj);
                strand.store(out, obj);
                Ok(())
            }
            sym::SINK => {
                unpack!(strand, args => obj);
                obj.sink(strand, out).await
            }
            sym::PUT => {
                unpack!(strand, args => obj, value);
                obj.put(strand, value).await
            }
            sym::PREMAP => {
                unpack!(strand, args => obj, func);
                create_premap(strand, &obj, func, out);
                Ok(())
            }
            sym::PREFILTER => {
                unpack!(strand, args => obj, pred);
                create_prefilter(strand, &obj, pred, out);
                Ok(())
            }
            sym::PRECHOMP => {
                unpack!(strand, args => obj);
                create_prechomp(strand, &obj, out);
                Ok(())
            }
            sym::PRECRIMP => {
                unpack!(strand, args => obj, ?terminator);
                create_precrimp(strand, &obj, terminator.as_deref(), out)
            }
            _ => Err(Error::field(strand, method)),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Sink"))
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Map";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().map_iter);
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
        with_slots!(strand, |mut input, mut func, mut item| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut input, &borrow.obj);
            strand.store(&mut func, &borrow.func);
            drop(borrow);
            if input.next(strand, &mut item).await? {
                call!(strand, &func, &item, out)?;
                Ok(true)
            } else {
                Ok(false)
            }
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Premap";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().output_iter);
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut output, mut func| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut func, &borrow.func);
            strand.store(&mut output, &borrow.obj);
            drop(borrow);
            call!(strand, &func, value, &mut output)?;
            this.borrow(strand)?.obj.put(strand, output).await
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Filter";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().filter_iter);
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_slots!(strand, |mut input, mut pred, mut res| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut pred, &borrow.pred);
            strand.store(&mut input, &borrow.obj);
            drop(borrow);
            loop {
                if !input.next(strand, &mut out).await? {
                    return Ok(false);
                }
                call!(strand, &pred, &out, &mut res)?;
                if res.to_bool(strand)? {
                    return Ok(true);
                }
            }
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Prefilter";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().output_iter);
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut output, mut pred| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut pred, &borrow.pred);
            strand.store(&mut output, &borrow.obj);
            drop(borrow);
            call!(strand, &pred, &value, &mut output)?;
            if output.to_bool(strand)? {
                this.borrow(strand)?.obj.put(strand, value).await
            } else {
                Ok(())
            }
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
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
            strand.store(out, &str[..end]);
        }
        View::Bin(bin) => {
            let bin = bin.pin();
            let end = bin.len() - line_terminator_len(&bin);
            strand.store(out, &bin[..end]);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Chomp";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter);
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
        with_slots!(strand, |mut input, mut item| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut input, &borrow.obj);
            drop(borrow);
            if input.next(strand, &mut item).await? {
                chomp_value(strand, &item, out, "iter.chomp")?;
                Ok(true)
            } else {
                Ok(false)
            }
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Prechomp";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().output_iter);
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut output, mut chomped| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut output, &borrow.obj);
            drop(borrow);
            chomp_value(strand, &value, &mut chomped, "sink.prechomp")?;
            output.put(strand, chomped).await
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Crimp";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter);
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
        with_slots!(strand, |mut input, mut item, mut term| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut input, &borrow.obj);
            let has_term = match &borrow.terminator {
                Some(terminator) => {
                    strand.store(&mut term, terminator);
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
                unpack!(strand, args => _self_val);
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Precrimp";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().output_iter);
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut output, mut crimped, mut term| {
            let borrow = this.borrow(strand)?;
            strand.store(&mut output, &borrow.obj);
            let has_term = match &borrow.terminator {
                Some(terminator) => {
                    strand.store(&mut term, terminator);
                    true
                }
                None => false,
            };
            drop(borrow);
            let term = has_term.then(|| &*term);
            crimp_value(strand, &value, term, &mut crimped, "sink.precrimp")?;
            output.put(strand, crimped).await
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => _self_val);
                Ok(())
            }
            _ => sink_mcall(strand, &this, method, args, out).await,
        }
    }
}

pub(crate) struct MapType;

unsafe impl Collect for MapType {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for MapType {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Map";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj);
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => func, obj);
        create_map(strand, &obj, func, out);
        Ok(())
    }
}

pub(crate) struct FilterType;

unsafe impl Collect for FilterType {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for FilterType {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Filter";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj);
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => pred, obj);
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
    let embryo = strand.builtin_types().map_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the value and the copy move straight into it
    let adapter = unsafe {
        Map {
            func: func.take(strand),
            obj: obj.dup(),
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_premap<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut func: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().premap_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the value and the copy move straight into it
    let adapter = unsafe {
        Premap {
            func: func.take(strand),
            obj: obj.dup(),
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_filter<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut pred: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().filter_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the value and the copy move straight into it
    let adapter = unsafe {
        Filter {
            pred: pred.take(strand),
            obj: obj.dup(),
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_prefilter<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    mut pred: Slot<'v, '_>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().prefilter_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the value and the copy move straight into it
    let adapter = unsafe {
        Prefilter {
            pred: pred.take(strand),
            obj: obj.dup(),
        }
    };
    embryo.finish(strand, adapter, out);
}

pub(crate) fn create_chomp<'v>(strand: &mut Strand<'v, '_>, obj: &Value<'v>, out: impl Output<'v>) {
    let embryo = strand.builtin_types().chomp_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copy moves straight into it
    embryo.finish(
        strand,
        Chomp {
            obj: unsafe { obj.dup() },
        },
        out,
    );
}

pub(crate) fn create_prechomp<'v>(
    strand: &mut Strand<'v, '_>,
    obj: &Value<'v>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().prechomp_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copy moves straight into it
    embryo.finish(
        strand,
        Prechomp {
            obj: unsafe { obj.dup() },
        },
        out,
    );
}

/// Validates an explicit `crimp`/`precrimp` terminator, rejecting anything that
/// is not `Str` or `Bin` at construction rather than on the first item.
fn check_terminator<'v, 's>(
    strand: &mut Strand<'v, 's>,
    terminator: Option<&Value<'v>>,
    who: &str,
) -> Result<'v, 's, ()> {
    match terminator {
        None => Ok(()),
        Some(terminator) => match terminator.view(strand) {
            View::Str(_) | View::Bin(_) => Ok(()),
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
    check_terminator(strand, terminator, "iter.crimp")?;
    let embryo = strand.builtin_types().crimp_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copies move straight into it
    embryo.finish(
        strand,
        Crimp {
            obj: unsafe { obj.dup() },
            terminator: terminator.map(|terminator| unsafe { terminator.dup() }),
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
    check_terminator(strand, terminator, "sink.precrimp")?;
    let embryo = strand.builtin_types().precrimp_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the copies move straight into it
    embryo.finish(
        strand,
        Precrimp {
            obj: unsafe { obj.dup() },
            terminator: terminator.map(|terminator| unsafe { terminator.dup() }),
        },
        out,
    );
    Ok(())
}

/// Creates a chain of the values in `sources`, moving them out and leaving the slots
/// nil.
pub(crate) fn create_chain<'v, 'a>(
    strand: &mut Strand<'v, '_>,
    sources: impl IntoIterator<Item = Slot<'v, 'a>>,
    out: impl Output<'v>,
) where
    'v: 'a,
{
    let embryo = strand.builtin_types().chain_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the values move from their rooted slots
    // straight into it
    let sources = sources
        .into_iter()
        .map(|mut slot| unsafe { slot.take(strand) })
        .collect();
    embryo.finish(strand, Chain { sources, index: 0 }, out);
}

/// Creates a zip of the values in `sources`, moving them out and leaving the slots
/// nil.
pub(crate) fn create_zip<'v, 'a>(
    strand: &mut Strand<'v, '_>,
    sources: impl IntoIterator<Item = Slot<'v, 'a>>,
    out: impl Output<'v>,
) where
    'v: 'a,
{
    let embryo = strand.builtin_types().zip_iter.embryo(strand);
    // SAFETY: the adapter is allocated; the values move from their rooted slots
    // straight into it
    let sources = sources
        .into_iter()
        .map(|mut slot| unsafe { slot.take(strand) })
        .collect();
    embryo.finish(strand, Zip { sources }, out);
}

pub(crate) fn create_take<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    remaining: usize,
    out: impl Output<'v>,
) {
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let embryo = strand.builtin_types().take_iter.embryo(strand);
    embryo.finish(
        strand,
        Take {
            source: unsafe { source.dup() },
            remaining,
        },
        out,
    );
}

pub(crate) fn create_skip<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    remaining: usize,
    out: impl Output<'v>,
) {
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let embryo = strand.builtin_types().skip_iter.embryo(strand);
    embryo.finish(
        strand,
        Skip {
            source: unsafe { source.dup() },
            remaining,
        },
        out,
    );
}

pub(crate) fn create_enumerate<'v>(
    strand: &mut Strand<'v, '_>,
    source: &Value<'v>,
    out: impl Output<'v>,
) {
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let embryo = strand.builtin_types().enumerate_iter.embryo(strand);
    let enumerate = Enumerate {
        source: unsafe { source.dup() },
        index: 0,
    };
    embryo.finish(strand, enumerate, out);
}

pub(crate) fn create_kv<'v>(strand: &mut Strand<'v, '_>, source: &Value<'v>, out: impl Output<'v>) {
    // SAFETY: the adapter is allocated; the copy moves straight into it
    let embryo = strand.builtin_types().kv_iter.embryo(strand);
    embryo.finish(
        strand,
        Kv {
            source: unsafe { source.dup() },
        },
        out,
    );
}

fn create_iter_spread<'v>(strand: &mut Strand<'v, '_>, source: &Value<'v>, out: impl Output<'v>) {
    // SAFETY: the proxy is allocated; the copy moves straight into it
    let embryo = strand.builtin_types().iter_spread.embryo(strand);
    embryo.finish(
        strand,
        IterSpread {
            source: unsafe { source.dup() },
        },
        out,
    );
}

pub(crate) async fn create_chain_from_args<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let len = args.len();
    with_slots_dynamic!(strand, len, |mut sources| {
        collect_sources(strand, args, &mut sources).await?;
        create_chain(strand, sources.iter_mut(), out);
        Ok(())
    })
}

pub(crate) async fn create_zip_from_args<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let len = args.len();
    with_slots_dynamic!(strand, len, |mut sources| {
        collect_sources(strand, args, &mut sources).await?;
        create_zip(strand, sources.iter_mut(), out);
        Ok(())
    })
}

/// The `std.Null` type object. `std.null` is its sole instance -- the same
/// relationship as `std.Nil`/`nil`, not a value fused with its own type.
pub(crate) struct NullType;

unsafe impl Collect for NullType {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for NullType {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Null";
    const TYPE_OBJECT: bool = true;

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
        supertype: &crate::value::Value<'v>,
    ) -> bool {
        // `null` acts as a complete iterator and sink by itself, not merely
        // something an `iter()`/`sink()` call can produce one from -- so it
        // claims `Iter`/`Sink` themselves, not just `Iterable`/`Sinkable`. As a
        // byte stream it is always at its end and discards what it is written.
        let sings = strand.singletons();
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Io).unwrap_or(false)
            || sings.input_iter.eq(strand, supertype).unwrap_or(false)
            || sings.output_iter.eq(strand, supertype).unwrap_or(false)
            || sings.iterable.eq(strand, supertype).unwrap_or(false)
            || sings.sinkable.eq(strand, supertype).unwrap_or(false)
    }
}

/// Singleton iterator/sink that yields no items and discards all items, and as
/// an `Io`, reads no bytes and discards those written.
pub(crate) struct Null;

unsafe impl Collect for Null {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Null {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Null";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().null_type)
    }

    // Iter protocol: never yields any items
    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
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
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack.sequence(
            strand,
            0,
            |_, _, _| Ok(()),
            |strand, _, slot| {
                strand.store(slot, &this);
                Ok(())
            },
        )
    }

    // Sink protocol: discards everything
    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
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
        match field.tag() {
            sym::INIT_METHOD
            | sym::READ_SOME
            | sym::WRITE_SOME
            | sym::SEEK
            | sym::FORK
            | sym::CLOSE => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::READABLE | sym::WRITABLE => {
                strand.store(out, true);
                Ok(())
            }
            sym::SEEKABLE | sym::FORKABLE => {
                strand.store(out, false);
                Ok(())
            }
            sym::READ
            | sym::WRITE
            | sym::FLUSH
            | sym::READ_ALL
            | sym::READ_ALL_STR
            | sym::CHUNKS
            | sym::IO => recv_get(strand, &this, field, out),
            tag if classify(tag) == Some(Surface::Sinkable) => sink_get(strand, &this, field, out),
            _ => iter_get(strand, &this, field, out),
        }
    }

    /// Provides [`ByteIo`](crate::io::ByteIo) through the `Io` bridge, which calls
    /// its own `read_some` and `write_some`.
    fn op_provide<'a, 's>(
        this: Recv<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        let (strand, answer) = request.parts();
        with_slots!(strand, |mut stream| {
            strand.store(Slot::reborrow(&mut stream), &this);
            let caps = IoCaps::READ | IoCaps::WRITE;
            provide_bridge(&mut Request::new(strand, answer), &stream, caps)
        })
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
                unpack!(strand, args => _self_val);
                Ok(())
            }
            // Always at the end of the stream
            sym::READ_SOME => {
                unpack!(strand, args => _size);
                BinEmbryo::new().finish(strand, out);
                Ok(())
            }
            // Discards what it is written, all of it at once
            sym::WRITE_SOME => {
                unpack!(strand, args => data);
                let len = with_u8_slice!(strand, data, |bytes| bytes.len())
                    .ok_or_else(|| Error::type_error(strand, "expected `Str` or `Bin`"))?;
                strand.store(out, len);
                Ok(())
            }
            // The one `null` is never closed
            sym::CLOSE => {
                unpack!(strand, args =>);
                Ok(())
            }
            sym::SEEK | sym::FORK => Err(Error::unsupported(strand, "`std.null` doesn't seek")),
            sym::READ
            | sym::WRITE
            | sym::FLUSH
            | sym::READ_ALL
            | sym::READ_ALL_STR
            | sym::CHUNKS
            | sym::IO => recv_mcall(strand, &this, method, args, out).await,
            tag if classify(tag) == Some(Surface::Sinkable) => {
                sink_mcall(strand, &this, method, args, out).await
            }
            _ => unpack_iter_mcall(strand, &this, method, args, out).await,
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
