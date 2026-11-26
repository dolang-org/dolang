use std::{marker::PhantomData, ops::ControlFlow};

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit, with_pin},
    object::protocol::{Protocol, Recv, Unpack, instance_get_fallback, members},
    strand::{Strand, with_slots_dynamic},
    sym::{self, Sym},
    unpack,
    value::{Input, Output, Slot, Slots, TypeObject, Value},
    vm::{Alloc, AllocExt, Vm},
    with_slots,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        GcObj, GcObjPin, Header, Inspect, Spread, SpreadContext, instance_mcall_fallback,
        is_special_mcall, type_mcall_fallback,
    },
    range,
};

/// A new tuple of the values `iter` yields, which it runs once the tuple is allocated.
///
/// # Safety
/// The tuple is loose: it must move straight into a scanned location.
pub(crate) unsafe fn tuple<'v, I: IntoIterator<Item = Value<'v>>>(
    vm: &Vm<'v>,
    iter: I,
) -> GcObj<'v, TupleObj<'v>>
where
    I::IntoIter: ExactSizeIterator,
{
    let iter = iter.into_iter();
    // SAFETY: the header carries the tuple vtable; the looseness is the caller's
    unsafe {
        gc::Embryo::new_tail(vm.arena(), iter.len()).finish_tail(
            Header::new(vm.arena(), vm.builtin_types().tuple.vtbl),
            TupleObj(PhantomData),
            (),
            iter,
        )
    }
}

/// Creates a tuple from `iter` and places it into `out`.
pub(crate) fn create<'v, I: IntoIterator<Item = Value<'v>>>(
    alloc: &mut impl Alloc<'v>,
    iter: I,
    out: impl Output<'v>,
) where
    I::IntoIter: ExactSizeIterator,
{
    let vm = alloc.alloc_vm(crate::vm::private::Sealed);
    // SAFETY: the new tuple is stored into `out` at once
    let tuple = unsafe { Value::from_object(tuple(vm, iter)) };
    alloc.store(out, tuple);
}

/// Creates a `(key, value)` pair and places it into `out`.
///
/// The tuple is allocated first, then the key built, so nothing is held loose across an
/// allocation; `value` is duplicated straight into the tuple.
pub(crate) fn create_pair<'v>(
    alloc: &mut impl Alloc<'v>,
    key: impl Input<'v>,
    value: &Value<'v>,
    out: impl Output<'v>,
) {
    let embryo = embryo(alloc, 2);
    // SAFETY: the tuple is allocated; the key and the copy of `value` move straight into
    // it
    let pair = unsafe { [Value::from_input(alloc, key), value.dup()] };
    embryo.finish(alloc, pair, out);
}

/// Allocates a tuple of `len` elements without initializing it; see
/// [`ObjEmbryo`](super::protocol::ObjEmbryo) for the contract.
pub(crate) fn embryo<'v>(alloc: &mut impl Alloc<'v>, len: usize) -> TupleEmbryo<'v> {
    let vm = alloc.alloc_vm(crate::vm::private::Sealed);
    TupleEmbryo(unsafe { gc::Embryo::new_tail(vm.arena(), len) })
}

/// An allocated but uninitialized tuple, created by [`embryo`].
pub(crate) struct TupleEmbryo<'v>(gc::Embryo<'v, Header, TupleObj<'v>>);

impl<'v> TupleEmbryo<'v> {
    /// Fills the tuple from `iter` and places it into `out`.
    ///
    /// # Panics
    /// If `iter` yields a different number of elements than the embryo was
    /// allocated for.
    pub(crate) fn finish(
        self,
        alloc: &mut impl Alloc<'v>,
        iter: impl IntoIterator<Item = Value<'v>>,
        out: impl Output<'v>,
    ) {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        let header = unsafe { Header::new(vm.arena(), vm.builtin_types().tuple.vtbl) };
        // SAFETY: the finished tuple is stored into `out` at once
        let tuple = unsafe {
            Value::from_object(self.0.finish_tail(header, TupleObj(PhantomData), (), iter))
        };
        alloc.store(out, tuple);
    }

    /// Fills the tuple by moving the values out of `slots`, leaving them nil, and
    /// places it into `out`.
    ///
    /// # Panics
    /// If `slots` yields a different number of slots than the embryo was allocated
    /// for.
    pub(crate) fn finish_moving<'a>(
        self,
        alloc: &mut impl Alloc<'v>,
        slots: impl IntoIterator<Item = Slot<'v, 'a>>,
        out: impl Output<'v>,
    ) where
        'v: 'a,
    {
        let vm = alloc.alloc_vm(crate::vm::private::Sealed);
        let header = unsafe { Header::new(vm.arena(), vm.builtin_types().tuple.vtbl) };
        // SAFETY: the tuple is allocated, and each value moves from its rooted slot
        // straight into it
        let values = slots
            .into_iter()
            .map(|mut slot| unsafe { slot.take(alloc) });
        // SAFETY: the finished tuple is stored into `out` at once
        let tuple = unsafe {
            Value::from_object(
                self.0
                    .finish_tail(header, TupleObj(PhantomData), (), values),
            )
        };
        alloc.store(out, tuple);
    }
}

impl<'v, 'a> Recv<'v, 'a, TupleObj<'v>> {
    /// The tuple's elements.
    pub(crate) fn items(&self) -> &'a [Value<'v>] {
        &self.annex().tail
    }
}

impl<'v, 'a> GcObjPin<'v, 'a, TupleObj<'v>> {
    /// The tuple's elements.
    pub(crate) fn items(&self) -> &'a [Value<'v>] {
        &self.annex().tail
    }
}

/// Store a tuple of `args`, which must all be positional, into `out`.
pub(crate) fn from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let args = args.positional(strand)?;
    let embryo = embryo(strand, args.len());
    embryo.finish_moving(strand, args, out);
    Ok(())
}

/// Store a tuple of `args` into `out`, expanding `...` spreads (keyed `ITER`).
pub(crate) async fn from_builtin_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut args: Args<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let spreads = args
        .reborrow()
        .any(|arg| matches!(arg, Arg::Key(sym, _) if sym.tag() == sym::ITER));
    if !spreads {
        return from_args(strand, args, out);
    }
    gather(strand, out, async |strand, sink| {
        let mut counter = 0;
        loop {
            counter += 1;
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?;
            }
            match args.next() {
                // SAFETY: the value moves straight into the borrowed sink
                Some(Arg::Pos(item)) => strand.store(sink.0.push(), item),
                Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                    expand
                        .op_spread(strand, SpreadContext::Sequence, sink)
                        .await?;
                }
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => return Ok(()),
            }
        }
    })
    .await
}

/// Store a tuple of the values `fill` gathers into its sink, whose number is known only
/// once it returns, into `out`.
///
/// The values gather in slots pushed onto a frame until the tuple is allocated.
async fn gather<'v, 's>(
    strand: &mut Strand<'v, 's>,
    out: impl Output<'v>,
    fill: impl AsyncFnOnce(&mut Strand<'v, 's>, &mut TupleSpread<'v, '_>) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    with_slots_dynamic!(strand, 0, |mut values| {
        fill(strand, &mut TupleSpread(&values)).await?;
        let embryo = embryo(strand, values.len());
        embryo.finish_moving(strand, values.iter_mut(), out);
        Ok(())
    })
}

struct TupleSpread<'v, 'a>(&'a Slots<'v, 'a>);

impl<'v, 's> Spread<'v, 's> for TupleSpread<'v, '_> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        strand.store(self.0.push(), value);
        Ok(())
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::unexpected_key(strand, key))
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Slot<'v, '_>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::unexpected_key(strand, key))
    }
}

/// A `Tuple` object. Its elements are the tail of its annex.
pub(crate) struct TupleObj<'v>(PhantomData<Value<'v>>);

pub(crate) type TupleAnnex<'v> = gc::Tail<(), [Value<'v>]>;

unsafe impl<'v> Collect for TupleObj<'v> {
    const CYCLIC: bool = true;
    type Annex = TupleAnnex<'v>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> gc::TailAnnex<Value<'v>> for () {
    fn accept(&self, elements: &[Value<'v>], visit: &mut dyn Visit) -> ControlFlow<()> {
        for element in elements {
            element.accept(visit)?
        }
        ControlFlow::Continue(())
    }

    // A tuple is immutable: any cycle through it passes through a mutable object,
    // which breaks the cycle when cleared.
    fn clear(&self, _elements: &[Value<'v>]) {}
}

impl<'v> Protocol<'v> for TupleObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().tuple)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "(")?;
        let items = this.items();
        let mut iter = items.iter();
        if let Some(first) = iter.next() {
            first.op_debug(strand, w)?;
            for item in iter {
                crate::fmt!(strand, w, ", ")?;
                item.op_debug(strand, w)?;
            }
        }
        // Match the literal syntax, where `(x)` is only a group
        if items.len() == 1 {
            crate::fmt!(strand, w, ",")?;
        }
        crate::fmt!(strand, w, ")")
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(!this.items().is_empty())
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let inner = this.items();
        for i in 0..inner.len() {
            if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap()?;
            }
            let elem = unsafe { inner.get_unchecked(i) };
            elem.op_hash(strand, hasher)?;
        }
        Ok(())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        if let Some(other) = other.downcast_ref(strand.builtin_types().tuple) {
            with_pin!(strand, other, |other| {
                let left = this.items();
                let right = other.items();
                if left.len() != right.len() {
                    strand.store(&mut out, Value::from_bool(false));
                    return Ok(());
                }
                for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if !l.eq(strand, r)? {
                        strand.store(&mut out, Value::from_bool(false));
                        return Ok(());
                    }
                }
                strand.store(&mut out, Value::from_bool(true));
                Ok(())
            })
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        if let Some(other) = other.downcast_ref(strand.builtin_types().tuple) {
            with_pin!(strand, other, |other| {
                let left = this.items();
                let right = other.items();
                for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if !l.eq(strand, r)? {
                        let flag = l.lt(strand, r)?;
                        strand.store(&mut out, flag);
                        return Ok(());
                    }
                }
                strand.store(&mut out, Value::from_bool(right.len() > left.len()));
                Ok(())
            })
        } else {
            Err(Error::unsupported(strand, "`<`"))
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(slice) = range::slice(index, strand, this.items().len())? {
            match slice {
                range::Slice::Contiguous { start, end } => {
                    let slice = this
                        .items()
                        .get(start..end)
                        .ok_or_else(|| Error::index(strand))?;
                    // SAFETY: `create` allocates the tuple before it runs the iterator,
                    // so the copies move straight into it
                    create(strand, slice.iter().map(|v| unsafe { v.dup() }), out);
                }
                range::Slice::Stepped(indices) => {
                    // SAFETY: as above
                    create(
                        strand,
                        indices
                            .into_iter()
                            .map(|i| unsafe { this.items()[i].dup() }),
                        out,
                    );
                }
            }
            return Ok(());
        }
        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
        match index::element(this.items().len(), index).and_then(|index| this.items().get(index)) {
            Some(value) => {
                strand.store(out, value);
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let embryo = strand.builtin_types().tuple_iter.embryo(strand);
        // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
        embryo.finish(
            strand,
            Iter {
                tuple: unsafe { this.to_strong() },
                index: 0,
            },
            out,
        );
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let items = this.items();
        unpack.sequence(
            strand,
            items.len(),
            |strand, i, slot| {
                strand.store(slot, &items[i]);
                Ok(())
            },
            |strand, consumed, slot| {
                // SAFETY: `create` allocates the tuple before it runs the iterator, so
                // the copies move straight into it
                create(
                    strand,
                    items[consumed..].iter().map(|v| unsafe { v.dup() }),
                    slot,
                );
                Ok(())
            },
        )
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.items();
        with_slots!(strand, |mut tmp| {
            for item in borrow.iter() {
                strand.store(&mut tmp, item);
                sink.positional(strand, Slot::reborrow(&mut tmp))?;
            }
            Ok(())
        })
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => index, ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let items = this.items();
                match index::element(items.len(), index).and_then(|index| items.get(index)) {
                    Some(value) => strand.store(&mut out, value),
                    None => {
                        if let Some(mut default) = default {
                            strand.store(&mut out, &mut default)
                        } else if let Some(or_else) = or_else {
                            call!(strand, or_else, out)?;
                        }
                    }
                }
                Ok(())
            }
            sym::PAIRS => {
                unpack!(strand, args =>);
                let embryo = strand.builtin_types().tuple_pairs.embryo(strand);
                // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
                embryo.finish(
                    strand,
                    Pairs {
                        tuple: unsafe { this.to_strong() },
                        index: 0,
                    },
                    out,
                );
                Ok(())
            }
            sym::CONTAINS => {
                unpack!(strand, args => needle);
                let inner = this.items();
                let mut found = false;
                for (i, elem) in inner.iter().enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if elem.eq(strand, &needle)? {
                        found = true;
                        break;
                    }
                }
                strand.store(out, found);
                Ok(())
            }
            sym::COPY => {
                unpack!(strand, args =>);
                if this.delegator().is_some() {
                    return with_slots!(strand, |mut receiver, mut ty| {
                        strand.store(Slot::reborrow(&mut receiver), &this);
                        receiver.op_type(strand, Slot::reborrow(&mut ty));
                        call!(strand, &ty, &receiver, out)
                    });
                }
                strand.store(out, &this);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "tuple.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
            }
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                strand.store(out, this.items().len());
                Ok(())
            }
            sym::PAIRS | sym::GET | sym::CONTAINS | sym::COPY => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::iterable_get(strand, &this, field, out),
        }
    }
}

pub(crate) struct Iter<'v> {
    tuple: GcObj<'v, TupleObj<'v>>,
    index: usize,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.tuple.accept(visit)
    }

    fn clear(&mut self) {
        // tuple can't be cleared, but the tuple itself can be
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "TupleIter";

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        with_pin!(strand, borrow.tuple, |tuple| {
            let items = tuple.items();
            let start = borrow.index.min(items.len());
            unpack.sequence(
                strand,
                items.len() - start,
                |strand, i, slot| {
                    strand.store(slot, &items[start + i]);
                    Ok(())
                },
                |strand, consumed, slot| {
                    let embryo = strand.builtin_types().tuple_iter.embryo(strand);
                    // SAFETY: the reference moves straight into the allocated iterator
                    embryo.finish(
                        strand,
                        Iter {
                            tuple: unsafe { borrow.tuple.dup() },
                            index: start + consumed,
                        },
                        slot,
                    );
                    Ok(())
                },
            )
        })
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        let found = with_pin!(strand, borrow.tuple, |tuple| {
            let value = tuple.items().get(index);
            if let Some(value) = value {
                strand.store(out, value);
            }
            value.is_some()
        });
        borrow.index += usize::from(found);
        Ok(found)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

pub(crate) struct Pairs<'v> {
    tuple: GcObj<'v, TupleObj<'v>>,
    index: usize,
}

unsafe impl<'v> Collect for Pairs<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.tuple.accept(visit)
    }

    fn clear(&mut self) {
        // tuple can't be cleared, but the tuple itself can be
    }
}

impl<'v> Protocol<'v> for Pairs<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "TuplePairs";

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        with_pin!(strand, borrow.tuple, |tuple| {
            let items = tuple.items();
            let start = borrow.index.min(items.len());
            unpack.sequence(
                strand,
                items.len() - start,
                |strand, i, slot| {
                    let index = i64::try_from(start + i).map_err(|_| Error::overflow(strand))?;
                    create_pair(strand, index, &items[start + i], slot);
                    Ok(())
                },
                |strand, consumed, slot| {
                    let embryo = strand.builtin_types().tuple_pairs.embryo(strand);
                    // SAFETY: the reference moves straight into the allocated iterator
                    embryo.finish(
                        strand,
                        Pairs {
                            tuple: unsafe { borrow.tuple.dup() },
                            index: start + consumed,
                        },
                        slot,
                    );
                    Ok(())
                },
            )
        })
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        let found = with_pin!(
            strand,
            borrow.tuple,
            |tuple| match tuple.items().get(index) {
                Some(value) => {
                    create_pair(strand, index as i64, value, out);
                    true
                }
                None => false,
            }
        );
        borrow.index += usize::from(found);
        Ok(found)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

// ── Tuple Class ─────────────────────────────────────────────────

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Tuple";

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => items);
        gather(strand, out, async |strand, sink| {
            super::iter::spread_iter(strand, &items, sink).await
        })
        .await
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
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().iterable)
                .unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                Method(sym::INIT_METHOD),
                Method(sym::SPREAD_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::GET),
                Method(sym::PAIRS),
                Method(sym::COPY),
                Method(sym::CONTAINS),
                Method(sym::INDEX_METHOD),
                Method(sym::ITER_METHOD),
                Method(sym::UNPACK_METHOD),
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
            sym::INIT_METHOD
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::GET
            | sym::PAIRS
            | sym::COPY
            | sym::CONTAINS
            | sym::INDEX_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
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
                unpack!(strand, args => self_val, items);
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().tuple, items, &mut native)?;
                    self_val.op_fill(
                        strand,
                        &strand.singletons().tuple,
                        Slot::reborrow(&mut native),
                    )?;
                    Ok(())
                })
            }
            _ => {
                let vm = strand.vm();
                type_mcall_fallback(strand, &vm.singletons().tuple, method, args, out).await
            }
        }
    }
}
