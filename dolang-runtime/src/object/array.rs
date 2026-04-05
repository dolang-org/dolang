use std::{fmt, mem, ops::ControlFlow};

use dolang_util::pairsort;

use crate::{
    arg::{Arg, Args},
    bytecode::Variadic,
    call,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    sig::{Unpack, UnpackKeyKind},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{self, Slot, Slots, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, default_spread,
        dispatch_native_method,
    },
    tuple,
};

pub(crate) struct Iter<'v> {
    array: GcObj<'v, Array<'v>>,
    index: usize,
}

struct ArraySpread<'a, 'v>(&'a mut Vec<Value<'v>>);

impl<'a, 'v, 's> Spread<'v, 's> for ArraySpread<'a, 'v> {
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

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }

    fn clear(&mut self) {
        // array can't be cleared, but the array itself can be
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<array input iterator>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        value::Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let array_borrow = borrow
            .array
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let count = unpack_from(strand, sig, &mut out, &array_borrow, borrow.index, false)?;
        drop(array_borrow);
        borrow.index += count;
        if sig.variadic == Variadic::Capture {
            value::Output::set(strand, out.at(sig.len() - 1), &this);
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
        let array_borrow = borrow
            .array
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        if let Some(value) = array_borrow.inner.get(index) {
            value::Output::set(strand, out, value);
            mem::drop(array_borrow);
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

pub(crate) struct Sink<'v> {
    array: GcObj<'v, Array<'v>>,
}

unsafe impl<'v> Collect for Sink<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }

    fn clear(&mut self) {
        // Can't clear `array`, but array itself is clearable
    }
}

impl<'v> Protocol<'v> for Sink<'v> {
    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<array output iterator>").into_do(strand)
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        value::Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let mut array_borrow = borrow
            .array
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?;
        array_borrow.inner.push(value.take());
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::sink_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::sink_mcall(strand, &this, method, args, out).await
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().output_iter.dup())
    }
}

pub(crate) struct Pairs<'v> {
    array: GcObj<'v, Array<'v>>,
    index: usize,
}

unsafe impl<'v> Collect for Pairs<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.array.accept(visit)
    }

    fn clear(&mut self) {
        // array can't be cleared, but the array itself can
    }
}

impl<'v> Protocol<'v> for Pairs<'v> {
    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        Self::op_debug(this, strand, w)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<array pair iterator>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        value::Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let array_borrow = borrow
            .array
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let count = unpack_from(strand, sig, &mut out, &array_borrow, borrow.index, true)?;
        mem::drop(array_borrow);
        borrow.index += count;
        if sig.variadic == Variadic::Capture {
            value::Output::set(strand, out.at(sig.len() - 1), &this);
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
        let array_borrow = borrow
            .array
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let inner = &array_borrow.inner;
        if let Some(value) = inner.get(index) {
            out.store(Value::from_object(tuple::tuple(
                strand,
                [Value::from_i64(strand, index as i64), value.dup()],
            )));
            mem::drop(array_borrow);
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

pub(crate) struct Array<'v> {
    pub(crate) inner: Vec<Value<'v>>,
}

unsafe impl<'v> Collect for Array<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for item in self.inner.iter() {
            item.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<'v> Array<'v> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    pub(crate) async fn from_builtin_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut counter = 0;

        loop {
            counter += 1;
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_interrupt_gc()?;
            }
            match args.next() {
                Some(Arg::Pos(mut item)) => this.inner.push(item.take()),
                Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                    let mut sink = ArraySpread(&mut this.inner);
                    expand
                        .op_spread(strand, SpreadContext::Sequence, &mut sink)
                        .await?;
                    continue;
                }
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => break,
            };
        }

        Ok(this)
    }

    async fn sort<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key = Sym::well_known(sym::KEY);
        let reverse = Sym::well_known(sym::REVERSE);
        let ([], [key, reverse]) = unpack!(strand, args, 0, 0, key = None, reverse = None)?;
        let reverse = reverse
            .map(|r| {
                r.as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "reverse: expected bool"))
            })
            .transpose()?
            .unwrap_or(false);

        if let Some(key_fn) = key {
            let borrow = this.borrow(strand)?;
            let len = borrow.inner.len();
            strand
                .with_slots_dynamic(len, async |strand, mut slots| {
                    {
                        for (i, value) in borrow.inner.iter().enumerate() {
                            if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                                strand.check_interrupt_gc()?;
                            }
                            let out = slots.at(i);
                            call!(strand, &key_fn, out, value).await?;
                        }
                    }

                    let keys = unsafe {
                        std::slice::from_raw_parts_mut(
                            slots.as_inner().as_ptr().cast::<Value<'v>>().cast_mut(),
                            len,
                        )
                    };
                    drop(borrow);
                    let mut borrow = this.borrow_mut(strand)?;
                    let mut compare_count = 0usize;
                    pairsort::sort_by(keys, borrow.inner.as_mut_slice(), |lhs, rhs| {
                        compare_count += 1;
                        if compare_count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                            strand.check_interrupt()?;
                        }
                        let result = if reverse {
                            rhs.op_lt(strand, lhs)?
                        } else {
                            lhs.op_lt(strand, rhs)?
                        };
                        Ok(result.to_bool(strand))
                    })
                })
                .await
        } else {
            let mut borrow = this.borrow_mut(strand)?;
            let mut payload = vec![(); borrow.inner.len()];
            let mut compare_count = 0usize;
            pairsort::sort_by(
                borrow.inner.as_mut_slice(),
                payload.as_mut_slice(),
                |lhs, rhs| {
                    compare_count += 1;
                    if compare_count.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_interrupt()?;
                    }
                    let result = if reverse {
                        rhs.op_lt(strand, lhs)?
                    } else {
                        lhs.op_lt(strand, rhs)?
                    };
                    Ok(result.to_bool(strand))
                },
            )
        }
    }
}

impl<'v> Protocol<'v> for Array<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, &strand.vm().singletons().sinkable)
            || supertype.eq(strand, &strand.vm().singletons().array)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let this_borrow = this.borrow(strand)?;
        write!(w, "[").into_do(strand)?;
        let inner = &this_borrow.inner;
        let mut iter = inner.iter();
        if let Some(first) = iter.next() {
            first.op_debug(strand, w)?;
            for i in 1..inner.len() {
                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_interrupt()?;
                }
                write!(w, ", ").into_do(strand)?;
                unsafe { inner.get_unchecked(i) }.op_debug(strand, w)?;
            }
        }
        write!(w, "]").into_do(strand)
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner = &borrow.inner;
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
        let other = if let Some(other) = other.downcast_native(strand, strand.builtin_types().array)
        {
            other
        } else {
            return Ok(Value::FALSE);
        };
        let this_borrow = this.borrow(strand)?;
        let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        let left = &this_borrow.inner;
        let right = &other_borrow.inner;
        if left.len() != right.len() {
            return Ok(Value::from_bool(false));
        }
        for i in 0..left.len() {
            if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_interrupt()?;
            }
            let l = unsafe { left.get_unchecked(i) };
            let r = unsafe { right.get_unchecked(i) };
            if !l.op_eq(strand, r).to_bool(strand) {
                return Ok(Value::from_bool(false));
            }
        }
        Ok(Value::from_bool(true))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().array) {
            other
        } else {
            return Ok(Value::FALSE);
        };
        let this_borrow = this.borrow(strand)?;
        let other_borrow = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        let left = &this_borrow.inner;
        let right = &other_borrow.inner;
        for i in 0..left.len().min(right.len()) {
            if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_interrupt()?;
            }
            let l = unsafe { left.get_unchecked(i) };
            let r = unsafe { right.get_unchecked(i) };
            if l.op_lt(strand, r)?.to_bool(strand) {
                return Ok(Value::from_bool(true));
            }
        }
        if right.len() > left.len() {
            return Ok(Value::from_bool(true));
        }
        Ok(Value::from_bool(false))
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let index = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
        let borrow = this.borrow(strand)?;
        let index =
            index::element(borrow.inner.len(), index).ok_or_else(|| Error::index(strand))?;
        match borrow.inner.get(index) {
            Some(value) => {
                value::Output::set(strand, out, value);
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let index = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
        let mut borrow = this.borrow_mut(strand)?;
        let index =
            index::element(borrow.inner.len(), index).ok_or_else(|| Error::index(strand))?;
        match borrow.inner.get_mut(index) {
            Some(slot) => {
                *slot = value.take();
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::PUSH => {
                let mut borrow = this.borrow_mut(strand)?;
                for arg in args {
                    match arg {
                        Arg::Pos(mut slot) => borrow.inner.push(slot.take()),
                        Arg::Key(key, _) => {
                            return Err(Error::unexpected_key(strand, key));
                        }
                    }
                }
                Ok(())
            }
            sym::INSERT => {
                let mut borrow = this.borrow_mut(strand)?;
                let index = match args.next() {
                    None => return Err(Error::missing_positional(strand, 0)),
                    Some(Arg::Key(key, _)) => {
                        return Err(Error::unexpected_key(strand, key));
                    }
                    Some(Arg::Pos(slot)) => slot,
                };
                let mut value = match args.next() {
                    None => return Err(Error::missing_positional(strand, 1)),
                    Some(Arg::Key(key, _)) => {
                        return Err(Error::unexpected_key(strand, key));
                    }
                    Some(Arg::Pos(slot)) => slot,
                };
                let i = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
                let i =
                    index::position(borrow.inner.len(), i).ok_or_else(|| Error::index(strand))?;
                if i > borrow.inner.len() {
                    return Err(Error::index(strand));
                }
                match args.next() {
                    None => borrow.inner.insert(i, value.take()),
                    Some(Arg::Key(key, _)) => {
                        return Err(Error::unexpected_key(strand, key));
                    }
                    Some(Arg::Pos(slot)) => {
                        let mut values = vec![value, slot];
                        for arg in args {
                            match arg {
                                Arg::Pos(slot) => values.push(slot),
                                Arg::Key(key, _) => {
                                    return Err(Error::unexpected_key(strand, key));
                                }
                            }
                        }
                        borrow
                            .inner
                            .splice(i..i, values.into_iter().map(|mut v| v.take()));
                    }
                }
                Ok(())
            }
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
                match index::element(borrow.inner.len(), index)
                    .and_then(|index| borrow.inner.get(index))
                {
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
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([], [index, default, or_else]) =
                    unpack!(strand, args, 0, 1, default = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let value = {
                    let mut borrow = this.borrow_mut(strand)?;
                    match index {
                        Some(index) => {
                            let index = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
                            index::element(borrow.inner.len(), index)
                                .map(|index| borrow.inner.remove(index))
                        }
                        None => borrow.inner.pop(),
                    }
                };
                out.store(match value {
                    Some(value) => value,
                    None => {
                        if let Some(mut default) = default {
                            default.take()
                        } else if let Some(or_else) = or_else {
                            let mut value = Value::NIL;
                            call!(strand, or_else, Slot::new(&mut value)).await?;
                            value
                        } else {
                            return Err(Error::index(strand));
                        }
                    }
                });
                Ok(())
            }
            sym::DELETE => {
                let ([index], []) = unpack!(strand, args, 1, 0)?;
                let index = index.as_i64(strand).ok_or_else(|| Error::index(strand))?;
                let mut borrow = this.borrow_mut(strand)?;
                let deleted = index::element(borrow.inner.len(), index).is_some();
                if let Some(index) = index::element(borrow.inner.len(), index) {
                    borrow.inner.remove(index);
                }
                value::Output::set(strand, out, deleted);
                Ok(())
            }
            sym::CLEAR => {
                let _ = unpack!(strand, args, 0, 0)?;
                this.borrow_mut(strand)?.inner.clear();
                Ok(())
            }
            sym::SORT => Self::sort(this, strand, args).await,
            sym::PAIRS => {
                let _ = unpack!(strand, args, 0, 0)?;
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().array_pairs,
                    Pairs {
                        array: this.to_strong(),
                        index: 0,
                    },
                )));
                Ok(())
            }
            sym::CONTAINS => {
                let ([needle], []) = unpack!(strand, args, 1, 0)?;
                let borrow = this.borrow(strand)?;
                let len = borrow.inner.len();
                let mut found = false;
                for i in 0..len {
                    if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                        strand.check_interrupt()?;
                    }
                    if unsafe { borrow.inner.get_unchecked(i) }
                        .op_eq(strand, &needle)
                        .to_bool(strand)
                    {
                        found = true;
                        break;
                    }
                }
                value::Output::set(strand.vm(), out, found);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "array.len is a field, not a method",
            )),
            sym::ITER => iter::iterable_mcall(strand, &this, method, args, out).await,
            sym::SINK => iter::sinkable_mcall(strand, &this, method, args, out).await,
            _ => Err(Error::field(strand, method)),
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                value::Output::set(strand, out, this.borrow(strand)?.inner.len() as i64);
                Ok(())
            }
            sym::PUSH
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::CLEAR
            | sym::SORT
            | sym::PAIRS
            | sym::CONTAINS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => {
                if let Ok(()) = iter::iterable_get(strand, &this, field, Slot::reborrow(&mut out)) {
                    Ok(())
                } else {
                    iter::sinkable_get(strand, &this, field, out)
                }
            }
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().array_iter,
            Iter {
                array: this.to_strong(),
                index: 0,
            },
        )));
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        match context {
            SpreadContext::Sequence => {
                let borrow = this.borrow(strand)?;
                for item in borrow.inner.iter() {
                    let mut tmp = item.dup();
                    sink.positional(strand, Slot::new(&mut tmp))?;
                }
                Ok(())
            }
            _ => default_spread(strand, this.clone(), context, sink).await,
        }
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let index = unpack_from(strand, sig, &mut out, &borrow, 0, false)?;
        if sig.variadic == Variadic::Capture {
            out.at(sig.len() - 1).store(Value::from_object(GcObj::new(
                strand.arena(),
                strand.builtin_types().array_iter,
                Iter {
                    array: this.to_strong(),
                    index,
                },
            )));
        }
        Ok(())
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let array = this.to_strong();
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().array_sink,
            Sink { array },
        )));
        Ok(())
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().array.dup())
    }
}

fn unpack_from<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sig: &Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    borrow: &Array<'v>,
    start: usize,
    pair: bool,
) -> Result<'v, 's, usize> {
    let len = borrow.inner.len() - start;
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
    for (i, elem) in borrow.inner[start..(start + min)]
        .iter()
        .chain(backfill.iter())
        .enumerate()
    {
        if pair {
            out.at(i).store(Value::from_object(tuple::tuple(
                strand,
                [
                    Value::from_i64(
                        strand,
                        i64::try_from(i).map_err(|_| Error::overflow(strand))?,
                    ),
                    elem.dup(),
                ],
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
                out.at(i).store(Value::from_object(tuple::tuple(
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

// ── Array Class ─────────────────────────────────────────────────

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
        let mut array = Array::new();
        let mut sink = ArraySpread(&mut array.inner);
        // FIXME: `array` is not GC-scannable, but then again if it were it would also
        // be mutably borrowed, which would inhibit GC.  This needs a resolution.
        items
            .op_spread(strand, SpreadContext::Sequence, &mut sink)
            .await?;
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().array,
            array,
        )));
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
            || supertype.eq(strand, &strand.vm().singletons().sinkable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.array>").into_do(strand)
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
                Sym::well_known(sym::PUSH),
                Sym::well_known(sym::INSERT),
                Sym::well_known(sym::GET),
                Sym::well_known(sym::POP),
                Sym::well_known(sym::DELETE),
                Sym::well_known(sym::CLEAR),
                Sym::well_known(sym::SORT),
                Sym::well_known(sym::PAIRS),
                Sym::well_known(sym::CONTAINS),
                Sym::well_known(sym::INDEX_METHOD),
                Sym::well_known(sym::ASSIGN_METHOD),
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
            | sym::PUSH
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::CLEAR
            | sym::SORT
            | sym::PAIRS
            | sym::CONTAINS
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
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
                let native = Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().array,
                    Array::new(),
                ));
                self_val.op_fill(strand, &strand.vm().singletons().array, native)?;
                Ok(())
            }
            _ => {
                dispatch_native_method(strand, &strand.vm().singletons().array, method, args, out)
                    .await
            }
        }
    }
}
