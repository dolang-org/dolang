use std::{collections::VecDeque, fmt, mem, ops::ControlFlow};

use crate::{
    arg::{Arg, Args},
    bytecode::Variadic,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    object::{BoundMethod, iter, sym::SymObj},
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, Value},
    vm::Vm,
};

use super::{
    protocol::{GcObj, Protocol, Recv, Spread, SpreadContext, default_spread},
    tuple,
};

pub(crate) struct ArgIter<'v> {
    inner: VecDeque<ArgItem<'v>>,
    int: i64,
}

pub(crate) struct PosArgIter<'v> {
    inner: VecDeque<ArgItem<'v>>,
}

type ArgItem<'v> = Option<(Option<GcObj<'v, SymObj>>, Value<'v>)>;

impl<'v> ArgIter<'v> {
    pub(crate) fn new(inner: VecDeque<ArgItem<'v>>) -> Self {
        Self { inner, int: 0 }
    }

    pub(crate) fn push(&mut self, vm: &Vm<'v>, args: Args<'v, '_>) {
        for arg in args {
            match arg {
                Arg::Pos(mut slot) => self.inner.push_back(Some((None, slot.take()))),
                Arg::Key(sym, mut slot) => self
                    .inner
                    .push_back(Some((Some(vm.sym_obj(sym)), slot.take()))),
            }
        }
    }

    pub(crate) fn from_args(vm: &Vm<'v>, args: Args<'v, '_>) -> Self {
        let mut this = Self::new(Default::default());
        this.push(vm, args);
        this
    }

    fn take_remaining(&mut self) -> VecDeque<ArgItem<'v>> {
        self.int = 0;
        mem::take(&mut self.inner)
    }
}

impl<'v> PosArgIter<'v> {
    fn new(inner: VecDeque<ArgItem<'v>>) -> Self {
        Self { inner }
    }
}

unsafe impl<'v> Collect for ArgIter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in self.inner.iter().filter_map(|elem| elem.as_ref()) {
            if let Some(key) = key {
                key.accept(visit)?;
            }
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.inner.clear()
    }
}

unsafe impl<'v> Collect for PosArgIter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (_, value) in self.inner.iter().filter_map(|elem| elem.as_ref()) {
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.inner.clear()
    }
}

impl<'v> Protocol<'v> for ArgIter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().args.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<arg input iter>").into_do(strand)
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
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;

        // Phase 1: Scan and validate without mutating
        struct Action {
            source_index: usize,
            dest_slot: usize,
        }

        let mut actions = Vec::new();
        let mut pos = 0;
        let pos_count = sig.required + sig.optional.len();
        let mut keys_left = sig.keys.len();
        let mut seen_keys = vec![false; keys_left];

        'top: for (idx, elem) in borrow.inner.iter().enumerate() {
            if pos == pos_count && keys_left == 0 && sig.variadic != Variadic::None {
                break;
            }
            let key = if let Some((key, _)) = elem {
                key
            } else {
                continue;
            };
            if let Some(sym) = key {
                if keys_left == 0 && sig.variadic == Variadic::None {
                    return Err(Error::unexpected_key(strand, unsafe {
                        Sym::from_tag(sym.tag)
                    }));
                }
                for (i, (wanted, seen)) in sig.keys.iter().zip(seen_keys.iter_mut()).enumerate() {
                    if *seen {
                        continue;
                    }
                    if let sig::UnpackKeyKind::Sym(wanted_sym) = &wanted.kind
                        && wanted_sym.tag() == sym.tag
                    {
                        *seen = true;
                        keys_left -= 1;
                        actions.push(Action {
                            source_index: idx,
                            dest_slot: sig.required + i,
                        });
                        continue 'top;
                    }
                }
                if sig.variadic == Variadic::None {
                    return Err(Error::unexpected_key(strand, unsafe {
                        Sym::from_tag(sym.tag)
                    }));
                }
            } else {
                if pos >= pos_count {
                    if sig.variadic == Variadic::None {
                        return Err(Error::unexpected_positional(strand, sig.required));
                    } else {
                        continue;
                    }
                }
                actions.push(Action {
                    source_index: idx,
                    dest_slot: pos,
                });
                pos += 1;
            }
        }

        // Validate BEFORE any mutation
        if pos < sig.required {
            return Err(Error::missing_positional(strand, pos));
        }

        // Check for missing required keys before mutation
        if sig.variadic == Variadic::None && keys_left != 0 {
            for (wanted, seen) in sig.keys.iter().zip(seen_keys.iter()) {
                if *seen {
                    continue;
                }
                if wanted.default.is_none() {
                    return Err(match &wanted.kind {
                        sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                        sig::UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
                    });
                }
            }
        }

        // Phase 2: All validation passed, now mutate atomically
        // Take elements and store values
        for action in actions.iter() {
            let value = borrow.inner[action.source_index].take().unwrap().1;
            out.at(action.dest_slot).store(value);
        }

        // Remove consumed elements in one efficient pass
        borrow.inner.retain(|e| e.is_some());

        // Fill in optional defaults
        for default in sig.optional[(pos - sig.required)..].iter() {
            out.at(pos).store(default.dup());
            pos += 1;
        }

        // Fill in key defaults
        if sig.variadic == Variadic::None && keys_left != 0 {
            for (i, (wanted, seen)) in sig.keys.iter().zip(seen_keys.iter()).enumerate() {
                if *seen {
                    continue;
                }
                if let Some(default) = &wanted.default {
                    Output::set(strand, out.at(sig.required + i), default);
                }
            }
        }

        // Update iterator position
        borrow.int = borrow
            .int
            .checked_add_unsigned(pos.try_into().map_err(|_| Error::overflow(strand))?)
            .ok_or_else(|| Error::overflow(strand))?;

        match sig.variadic {
            Variadic::None => {
                // Nothing - validation already done
            }
            Variadic::Discard => {
                // Allow extra args but don't build iterator
                // Nothing to store
            }
            Variadic::Capture => {
                // Build iterator for remaining items
                Output::set(strand, out.at(sig.required + sig.keys.len()), &this);
            }
        }

        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        while let Some(elem) = borrow.inner.pop_front() {
            let (key, value) = if let Some(pair) = elem {
                pair
            } else {
                continue;
            };
            let key = match key {
                None => {
                    let key = Value::from_i64(strand, borrow.int);
                    borrow.int += 1;
                    key
                }
                Some(key) => Value::from_object(key),
            };
            out.store(Value::from_object(tuple::tuple(strand, [key, value])));
            return Ok(true);
        }
        Ok(false)
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        match context {
            SpreadContext::Args => {
                let mut borrow = this.borrow_mut(strand)?;
                while let Some(elem) = borrow.inner.pop_front() {
                    let (key, value) = if let Some(pair) = elem {
                        pair
                    } else {
                        continue;
                    };
                    if let Some(key) = key {
                        let mut value = value;
                        let mut key = Value::from_object(key);
                        sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))?;
                    } else {
                        let mut value = value;
                        sink.positional(strand, Slot::new(&mut value))?;
                    }
                }
                Ok(())
            }
            _ => default_spread(strand, this.clone(), context, sink).await,
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::PUSH => {
                this.borrow_mut(strand)?.push(strand, args);
                Ok(())
            }
            sym::POS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let remaining = this.borrow_mut(strand)?.take_remaining();
                for (key, _) in remaining.iter().filter_map(|elem| elem.as_ref()) {
                    if let Some(key) = key.as_ref() {
                        return Err(Error::unexpected_key(strand, unsafe {
                            Sym::from_tag(key.tag)
                        }));
                    }
                }
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.vm().builtin_types().pos_arg_iter,
                    PosArgIter::new(remaining),
                )));
                Ok(())
            }
            sym::POS_KEYS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let remaining = this.borrow_mut(strand)?.take_remaining();
                let mut pos = VecDeque::new();
                let mut keyed = VecDeque::new();
                for elem in remaining {
                    let Some((key, value)) = elem else {
                        continue;
                    };
                    if key.is_none() {
                        pos.push_back(Some((None, value)));
                    } else {
                        keyed.push_back(Some((key, value)));
                    }
                }
                out.store(Value::from_object(tuple::tuple(
                    strand,
                    [
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.vm().builtin_types().pos_arg_iter,
                            PosArgIter::new(pos),
                        )),
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.vm().builtin_types().arg_iter,
                            ArgIter::new(keyed),
                        )),
                    ],
                )));
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "args.len is a field, not a method",
            )),
            _ => iter::iter_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let input = i64::try_from(this.borrow(strand)?.inner.len())
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::PUSH => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            sym::POS | sym::POS_KEYS => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => iter::iter_get(strand, &this, field, out),
        }
    }
}

impl<'v> Protocol<'v> for PosArgIter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<pos arg input iter>").into_do(strand)
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
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let len = borrow.inner.len();
        let pos_count = sig.required + sig.optional.len();

        if sig.required > len {
            return Err(Error::missing_positional(strand, len));
        }
        if pos_count < len && sig.variadic == Variadic::None {
            return Err(Error::unexpected_positional(strand, sig.required));
        }

        let min = pos_count.min(len);
        for i in 0..min {
            let value = borrow
                .inner
                .pop_front()
                .expect("checked len above")
                .expect("pos arg iter should not contain tombstones")
                .1;
            out.at(i).store(value);
        }

        for (i, default) in sig.optional[(min.saturating_sub(sig.required))..]
            .iter()
            .enumerate()
        {
            out.at(min + i).store(default.dup());
        }

        for (i, key) in sig.keys.iter().enumerate() {
            if let Some(default) = &key.default {
                out.at(pos_count + i).store(default.dup());
            } else {
                return Err(match &key.kind {
                    sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                    sig::UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
                });
            }
        }

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
        if let Some((_, value)) = borrow.inner.pop_front().flatten() {
            out.store(value);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        while let Some((_, mut value)) = borrow.inner.pop_front().flatten() {
            sink.positional(strand, Slot::new(&mut value))?;
        }
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
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
