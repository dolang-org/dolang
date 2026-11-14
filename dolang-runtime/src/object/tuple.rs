use std::{marker::PhantomData, ops::ControlFlow};

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    bytecode::Rest,
    call,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit},
    object::protocol::{Protocol, Recv, instance_get_fallback, members},
    sig::{Unpack, UnpackKeyKind},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, TypeObject, Value},
    vm::{Alloc, Vm},
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

pub(crate) fn tuple<'v, I: IntoIterator<Item = Value<'v>>>(
    vm: &Vm<'v>,
    iter: I,
) -> GcObj<'v, TupleObj<'v>>
where
    I::IntoIter: ExactSizeIterator,
{
    let iter = iter.into_iter();
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
    Output::set(alloc, out, Value::from_object(tuple(vm, iter)));
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
        let tuple = unsafe { self.0.finish_tail(header, TupleObj(PhantomData), (), iter) };
        Output::set(alloc, out, Value::from_object(tuple));
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

pub(crate) fn from_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, '_>,
) -> Result<'v, 's, Vec<Value<'v>>> {
    let mut values = Vec::new();

    for (index, arg) in args.enumerate() {
        if (index + 1) % crate::INTERRUPT_INTERVAL == 0 {
            strand.check_trap_gc()?;
        }
        match arg {
            Arg::Pos(mut value) => values.push(value.take()),
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        }
    }

    Ok(values)
}

pub(crate) async fn from_builtin_args<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut args: Args<'v, '_>,
) -> Result<'v, 's, Vec<Value<'v>>> {
    let mut values = Vec::new();
    let mut counter = 0;

    loop {
        counter += 1;
        if counter % crate::INTERRUPT_INTERVAL == 0 {
            strand.check_trap_gc()?;
        }
        match args.next() {
            Some(Arg::Pos(mut item)) => values.push(item.take()),
            Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                let mut sink = TupleSpread(&mut values);
                expand
                    .op_spread(strand, SpreadContext::Sequence, &mut sink)
                    .await?;
            }
            Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
            None => break,
        }
    }

    Ok(values)
}

struct TupleSpread<'a, 'v>(&'a mut Vec<Value<'v>>);

impl<'a, 'v, 's> Spread<'v, 's> for TupleSpread<'a, 'v> {
    fn positional(
        &mut self,
        _strand: &mut Strand<'v, 's>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.0.push(value.take());
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
        Output::set(strand, out, &strand.singletons().tuple)
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
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(other) = other.downcast_ref(strand.builtin_types().tuple) {
            other.pin_sync(strand, |strand, other| {
                let left = this.items();
                let right = other.items();
                if left.len() != right.len() {
                    return Ok(Value::from_bool(false));
                }
                for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if !l.op_eq(strand, r)?.to_bool(strand)? {
                        return Ok(Value::from_bool(false));
                    }
                }
                Ok(Value::from_bool(true))
            })
        } else {
            Err(Error::unsupported(strand, "`==`"))
        }
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(other) = other.downcast_ref(strand.builtin_types().tuple) {
            other.pin_sync(strand, |strand, other| {
                let left = this.items();
                let right = other.items();
                for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if !l.op_eq(strand, r)?.to_bool(strand)? {
                        return Ok(Value::from_bool(l.op_lt(strand, r)?.to_bool(strand)?));
                    }
                }
                Ok(Value::from_bool(right.len() > left.len()))
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
                    create(strand, slice.iter().map(Value::dup), out);
                }
                range::Slice::Stepped(indices) => {
                    create(
                        strand,
                        indices.into_iter().map(|i| this.items()[i].dup()),
                        out,
                    );
                }
            }
            return Ok(());
        }
        let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
        match index::element(this.items().len(), index).and_then(|index| this.items().get(index)) {
            Some(value) => {
                Output::set(strand, out, value);
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
        strand.builtin_types().tuple_iter.create(
            strand,
            Iter {
                tuple: this.to_strong(),
                index: 0,
            },
            out,
        );
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let index = unpack_from(strand, sig, &mut out, this.items(), 0, false)?;
        if let Some(i) = sig.pos_rest_slot() {
            create(
                strand,
                this.items()[index..].iter().map(Value::dup),
                out.at(i),
            );
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.items();
        strand.with_slots_sync(|strand, [mut tmp]| {
            for item in borrow.iter() {
                Output::set(strand, &mut tmp, item);
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
                let ([index], [default, or_else]) =
                    unpack!(strand, args, 1, 0, default = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let index = index.to_i64(strand).map_err(|_| Error::index(strand))?;
                let items = this.items();
                match index::element(items.len(), index).and_then(|index| items.get(index)) {
                    Some(value) => out.store(value.dup()),
                    None => {
                        if let Some(mut default) = default {
                            out.store(default.take())
                        } else if let Some(or_else) = or_else {
                            call!(strand, or_else, out)?;
                        }
                    }
                }
                Ok(())
            }
            sym::PAIRS => {
                let _ = unpack!(strand, args, 0, 0)?;
                strand.builtin_types().tuple_pairs.create(
                    strand,
                    Pairs {
                        tuple: this.to_strong(),
                        index: 0,
                    },
                    out,
                );
                Ok(())
            }
            sym::CONTAINS => {
                let ([needle], []) = unpack!(strand, args, 1, 0)?;
                let inner = this.items();
                let mut found = false;
                for (i, elem) in inner.iter().enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_trap()?;
                    }
                    if elem.op_eq(strand, &needle)?.to_bool(strand)? {
                        found = true;
                        break;
                    }
                }
                Output::set(strand, out, found);
                Ok(())
            }
            sym::COPY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if this.delegator().is_some() {
                    return with_slots!(strand, |mut receiver, mut ty| {
                        Output::set(strand, Slot::reborrow(&mut receiver), &this);
                        receiver.op_type(strand, Slot::reborrow(&mut ty));
                        call!(strand, &ty, out, &receiver)
                    });
                }
                Output::set(strand, out, &this);
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
                Output::set(strand, out, this.items().len());
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
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<tuple iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let count = borrow.tuple.pin_sync(strand, |strand, tuple| {
            unpack_from(strand, sig, &mut out, tuple.items(), borrow.index, false)
        })?;
        if let Some(i) = sig.pos_rest_slot() {
            strand.builtin_types().tuple_iter.create(
                strand,
                Iter {
                    tuple: borrow.tuple.clone(),
                    index: borrow.index + count,
                },
                out.at(i),
            );
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        let found = borrow.tuple.pin_sync(strand, |strand, tuple| {
            let value = tuple.items().get(index);
            if let Some(value) = value {
                Output::set(strand, out, value);
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
        Output::set(strand, out, &strand.singletons().input_iter)
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
        iter::iter_mcall(strand, &this, method, args, out).await
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
    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: crate::object::protocol::SpreadContext,
        sink: &'a mut dyn crate::object::protocol::Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, &this, sink).await
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<tuple pair iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let count = borrow.tuple.pin_sync(strand, |strand, tuple| {
            unpack_from(strand, sig, &mut out, tuple.items(), borrow.index, true)
        })?;
        if let Some(i) = sig.pos_rest_slot() {
            strand.builtin_types().tuple_pairs.create(
                strand,
                Pairs {
                    tuple: borrow.tuple.clone(),
                    index: borrow.index + count,
                },
                out.at(i),
            );
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        let value = borrow
            .tuple
            .pin_sync(strand, |_, tuple| tuple.items().get(index).map(Value::dup));
        let Some(value) = value else {
            return Ok(false);
        };
        create(strand, [Value::from_i64(strand, index as i64), value], out);
        borrow.index += 1;
        Ok(true)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
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
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}

fn unpack_from<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    borrow: &[Value<'v>],
    start: usize,
    pair: bool,
) -> Result<'v, 's, usize> {
    let len = borrow.len() - start;
    let pos_count = sig.required + sig.optional.len();
    if sig.required > len {
        return Err(Error::missing_positional(strand, sig.required));
    }
    if pos_count < len && sig.pos_rest() == Rest::None {
        return Err(Error::unexpected_positional(strand, sig.required));
    }
    let min = pos_count.min(len);
    for (i, elem) in borrow[start..(start + min)].iter().enumerate() {
        if pair {
            let value = i64::try_from(start + i).map_err(|_| Error::overflow(strand))?;
            let items = [Value::from_i64(strand, value), elem.dup()];
            create(strand, items, out.at(i))
        } else {
            out.at(i).store(elem.dup())
        }
    }
    if len < pos_count {
        for (i, default) in sig.optional[(len - sig.required)..].iter().enumerate() {
            out.at(min + i).store(default.dup());
        }
    }
    for (i, key) in sig.keys.iter().enumerate() {
        if let Some(default) = &key.default {
            out.at(pos_count + i).store(default.dup());
        } else {
            return Err(match &key.kind {
                UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
            });
        }
    }
    Ok(min)
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
    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        let mut values = Vec::new();
        let mut sink = TupleSpread(&mut values);
        items
            .op_spread(strand, SpreadContext::Sequence, &mut sink)
            .await?;
        create(strand, values, out);
        Ok(())
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
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().iterable)
                .unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Tuple>")
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
                let ([self_val, items], []) = unpack!(strand, args, 2, 0)?;
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().tuple, &mut native, items)?;
                    self_val.op_fill(strand, &strand.singletons().tuple, native.take())?;
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
