use std::{fmt, ops::ControlFlow};

use crate::{
    arg::Args,
    bytecode::Variadic,
    call,
    error::{Error, Result, ResultExt},
    gc::{self, Collect, arena::Visit},
    object::protocol::{Protocol, Recv},
    sig::{Unpack, UnpackKeyKind},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, index, iter,
    protocol::{GcObj, Header, Inspect, Spread, SpreadContext, dispatch_native_method},
    range,
};

pub(crate) fn tuple<'v, I: IntoIterator<Item = Value<'v>>>(
    vm: &Vm<'v>,
    iter: I,
) -> GcObj<'v, [Value<'v>]>
where
    I::IntoIter: ExactSizeIterator,
{
    unsafe {
        gc::Base::from_header_iter(
            vm.arena(),
            Header::new(vm.arena(), vm.builtin_types().tuple.vtbl),
            iter.into_iter(),
        )
    }
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

impl<'v> Protocol<'v> for [Value<'v>] {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, &strand.vm().singletons().tuple)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().tuple.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "(").into_do(strand)?;
        let mut iter = this.receiver.get().iter();
        if let Some(first) = iter.next() {
            first.op_debug(strand, w)?;
            for item in iter {
                write!(w, ", ").into_do(strand)?;
                item.op_debug(strand, w)?;
            }
        }
        write!(w, ")").into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, _strand: &mut Strand<'v, 's>) -> bool {
        !this.get().is_empty()
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let inner = this.receiver.get();
        for i in 0..inner.len() {
            if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_interrupt()?;
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
            let left = this.receiver.get();
            let right = other.get();
            if left.len() != right.len() {
                return Ok(Value::from_bool(false));
            }
            for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_interrupt()?;
                }
                if !l.op_eq(strand, r).to_bool(strand) {
                    return Ok(Value::from_bool(false));
                }
            }
            Ok(Value::from_bool(true))
        } else {
            Ok(Value::from_bool(false))
        }
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        if let Some(other) = other.downcast_ref(strand.builtin_types().tuple) {
            let left = this.receiver.get();
            let right = other.get();
            for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_interrupt()?;
                }
                if l.op_lt(strand, r)?.to_bool(strand) {
                    return Ok(Value::from_bool(true));
                }
            }
            if right.len() > left.len() {
                return Ok(Value::from_bool(true));
            }
            Ok(Value::from_bool(false))
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some((start, end)) = range::slice_bounds(index, strand, this.get().len())? {
            let slice = this
                .get()
                .get(start..end)
                .ok_or_else(|| Error::index(strand))?;
            out.store(Value::from_object(tuple(
                strand.vm(),
                slice.iter().map(Value::dup),
            )));
            return Ok(());
        }
        let index = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
        match index::element(this.get().len(), index).and_then(|index| this.get().get(index)) {
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().tuple_iter,
            Iter {
                tuple: this.to_strong(),
                index: 0,
            },
        )));
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !sig.keys.is_empty() {
            let first_key = sig.keys.first().unwrap();
            return Err(match &first_key.kind {
                UnpackKeyKind::Sym(s) => Error::missing_key(strand, *s),
                UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
            });
        }
        let index = unpack_from(strand, sig, &mut out, this.get(), 0, false)?;
        if sig.variadic == Variadic::Capture {
            out.at(sig.len() - 1).store(Value::from_object(GcObj::new(
                strand.arena(),
                strand.builtin_types().tuple_iter,
                Iter {
                    tuple: this.to_strong(),
                    index,
                },
            )));
        }
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        for item in borrow.iter() {
            let mut tmp = item.dup();
            sink.positional(strand, Slot::new(&mut tmp))?;
        }
        Ok(())
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
                let index = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
                let borrow = this.borrow(strand)?;
                match index::element(borrow.len(), index).and_then(|index| borrow.get(index)) {
                    Some(value) => out.store(value.dup()),
                    None => {
                        if let Some(mut default) = default {
                            out.store(default.take())
                        } else if let Some(or_else) = or_else {
                            call!(strand, or_else, out).await?;
                        }
                    }
                }
                Ok(())
            }
            sym::PAIRS => {
                let _ = unpack!(strand, args, 0, 0)?;
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().tuple_pairs,
                    Pairs {
                        tuple: this.to_strong(),
                        index: 0,
                    },
                )));
                Ok(())
            }
            sym::CONTAINS => {
                let ([needle], []) = unpack!(strand, args, 1, 0)?;
                let inner = this.receiver.get();
                let mut found = false;
                for (i, elem) in inner.iter().enumerate() {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_interrupt()?;
                    }
                    if elem.op_eq(strand, &needle).to_bool(strand) {
                        found = true;
                        break;
                    }
                }
                Output::set(strand, out, found);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "tuple.len is a field, not a method",
            )),
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
                Output::set(strand, out, this.get().len() as i64);
                Ok(())
            }
            sym::PAIRS | sym::GET | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::iterable_get(strand, &this, field, out),
        }
    }
}

pub(crate) struct Iter<'v> {
    tuple: GcObj<'v, [Value<'v>]>,
    index: usize,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.tuple.accept(visit)
    }

    fn clear(&mut self) {
        // tuple can't be cleared, but the tuple itself can be
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<tuple input iterator>").into_do(strand)
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
        if !sig.keys.is_empty() {
            let first_key = sig.keys.first().unwrap();
            return Err(match &first_key.kind {
                UnpackKeyKind::Sym(s) => Error::missing_key(strand, *s),
                UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
            });
        }
        let mut borrow = this.borrow_mut(strand)?;
        let count = unpack_from(strand, sig, &mut out, &borrow.tuple, borrow.index, false)?;
        borrow.index += count;
        if sig.variadic == Variadic::Capture {
            Output::set(strand, out.at(sig.len() - 1), &this);
        }
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        if let Some(value) = borrow.tuple.get(index) {
            Output::set(strand, out, value);
            borrow.index += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
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
    tuple: GcObj<'v, [Value<'v>]>,
    index: usize,
}

unsafe impl<'v> Collect for Pairs<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.tuple.accept(visit)
    }

    fn clear(&mut self) {
        // tuple can't be cleared, but the tuple itself can be
    }
}

impl<'v> Protocol<'v> for Pairs<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<tuple pair iterator>").into_do(strand)
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
        if !sig.keys.is_empty() {
            let first_key = sig.keys.first().unwrap();
            return Err(match &first_key.kind {
                UnpackKeyKind::Sym(s) => Error::missing_key(strand, *s),
                UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
            });
        }
        let mut borrow = this.borrow_mut(strand)?;
        let count = unpack_from(strand, sig, &mut out, &borrow.tuple, borrow.index, true)?;
        borrow.index += count;
        if sig.variadic == Variadic::Capture {
            Output::set(strand, out.at(sig.len() - 1), &this);
        }
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let index = borrow.index;
        if let Some(value) = borrow.tuple.get(index) {
            out.store(Value::from_object(tuple(
                strand,
                [Value::from_i64(strand, index as i64), value.dup()],
            )));
            borrow.index += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
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
    if pos_count < len && sig.variadic == Variadic::None {
        return Err(Error::unexpected_positional(strand, sig.required));
    }
    let backfill = if len < pos_count {
        &sig.optional[(len - sig.required)..]
    } else {
        &[]
    };
    let min = pos_count.min(len);
    for (i, elem) in borrow[start..(start + min)]
        .iter()
        .chain(backfill.iter())
        .enumerate()
    {
        if pair {
            let value = i64::try_from(i).map_err(|_| Error::overflow(strand))?;
            out.at(i).store(Value::from_object(tuple(
                strand,
                [Value::from_i64(strand, value), elem.dup()],
            )))
        } else {
            out.at(i).store(elem.dup())
        }
    }
    for (i, key) in sig.keys.iter().enumerate() {
        if let Some(default) = &key.default {
            if pair {
                let key_value = match &key.kind {
                    UnpackKeyKind::Sym(sym) => Value::from_object(strand.sym_obj(*sym)),
                    UnpackKeyKind::Const(value) => value.dup(),
                };
                out.at(i).store(Value::from_object(tuple(
                    strand,
                    [key_value, default.dup()],
                )))
            } else {
                out.at(i + min).store(default.dup())
            }
        } else {
            return Err(match &key.kind {
                UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
            });
        }
    }
    Ok(min + sig.keys.len())
}

// ── Tuple Class ─────────────────────────────────────────────────

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
    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        let mut values = Vec::new();
        let mut sink = TupleSpread(&mut values);
        items
            .op_spread(strand, SpreadContext::Sequence, &mut sink)
            .await?;
        out.store(Value::from_object(tuple(strand.vm(), values)));
        Ok(())
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        use crate::error::ResultExt;
        write!(w, "<type std.tuple>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: vec![
                Sym::well_known(sym::STR_METHOD),
                Sym::well_known(sym::DBG_METHOD),
                Sym::well_known(sym::EQ_METHOD),
                Sym::well_known(sym::LT_METHOD),
                Sym::well_known(sym::HASH_METHOD),
                Sym::well_known(sym::LEN),
                Sym::well_known(sym::GET),
                Sym::well_known(sym::PAIRS),
                Sym::well_known(sym::CONTAINS),
                Sym::well_known(sym::INDEX_METHOD),
                Sym::well_known(sym::ITER_METHOD),
                Sym::well_known(sym::UNPACK_METHOD),
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
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::GET
            | sym::PAIRS
            | sym::CONTAINS
            | sym::INDEX_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
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
                let ([self_val], []) = unpack!(strand, args, 1, 0)?;
                let native = Value::from_object(tuple(strand, std::iter::empty::<Value<'v>>()));
                self_val.op_fill(strand, &strand.vm().singletons().tuple, native)?;
                Ok(())
            }
            _ => {
                let vm = strand.vm();
                dispatch_native_method(strand, &vm.singletons().tuple, method, args, out).await
            }
        }
    }
}
