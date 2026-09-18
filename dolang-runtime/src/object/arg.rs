use std::{collections::HashSet, ops::ControlFlow};

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    bytecode::{Rest, Variadic},
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{iter, sym::SymObj},
    sig,
    strand::Strand,
    sym::{self, Sym},
    value::{Output, Slot, Slots, Value},
    vm::Vm,
};

use super::{
    protocol::{GcObj, Protocol, Recv, Spread, SpreadContext},
    record::Record,
    tuple,
};

pub(crate) type ArgItem<'v> = (Option<GcObj<'v, SymObj>>, Value<'v>);

pub(crate) struct ArgPack<'v> {
    inner: Vec<ArgItem<'v>>,
}

pub(crate) struct ArgIter<'v> {
    pack: GcObj<'v, ArgPack<'v>>,
    skip: HashSet<usize>,
    pos: usize,
    int: i64,
}

struct Action {
    source_index: usize,
    dest_slot: usize,
}

struct UnpackPlan {
    actions: Vec<Action>,
    pos_matched: usize,
    /// Leftover positional items, for a `*name` rest
    pos_rest: Vec<usize>,
    /// Leftover keyed items, for a `**name` rest
    key_rest: Vec<usize>,
}

impl<'v> ArgPack<'v> {
    pub(crate) fn new(inner: Vec<ArgItem<'v>>) -> Self {
        Self { inner }
    }

    pub(crate) fn from_args(vm: &Vm<'v>, args: Args<'v, '_>) -> Self {
        let mut inner = Vec::new();
        for arg in args {
            match arg {
                Arg::Pos(mut slot) => inner.push((None, slot.take())),
                Arg::Key(sym, mut slot) => inner.push((Some(vm.sym_obj(sym)), slot.take())),
            }
        }
        Self::new(inner)
    }
}

impl<'v> ArgIter<'v> {
    pub(crate) fn new(
        pack: GcObj<'v, ArgPack<'v>>,
        skip: HashSet<usize>,
        pos: usize,
        int: i64,
    ) -> Self {
        Self {
            pack,
            skip,
            pos,
            int,
        }
    }
}

fn first_visible_index<'v>(
    items: &[ArgItem<'v>],
    skip: &HashSet<usize>,
    start: usize,
) -> Option<usize> {
    (start..items.len()).find(|index| !skip.contains(index))
}

fn visible_len<'v>(items: &[ArgItem<'v>], skip: &HashSet<usize>, start: usize) -> usize {
    (start..items.len())
        .filter(|index| !skip.contains(index))
        .count()
}

fn unpack_plan<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    items: &[ArgItem<'v>],
    skip: &HashSet<usize>,
    start: usize,
    sig: &sig::Unpack<'v, 'a>,
) -> Result<'v, 's, UnpackPlan> {
    let mut actions = Vec::new();
    let mut pos = 0;
    let pos_count = sig.required + sig.optional.len();
    let mut keys_left = sig.keys.len();
    let mut seen_keys = vec![false; keys_left];
    let split = !matches!(sig.variadic, Variadic::Discard | Variadic::Capture);
    let collect_pos = split && sig.pos_rest() == Rest::Capture;
    let collect_keys = split && sig.key_rest() == Rest::Capture;
    let mut pos_rest = Vec::new();
    let mut key_rest = Vec::new();

    'top: for (idx, (key, _)) in items.iter().enumerate().skip(start) {
        if skip.contains(&idx) {
            continue;
        }
        if pos == pos_count
            && keys_left == 0
            && sig.pos_rest() != Rest::None
            && sig.key_rest() != Rest::None
            && !collect_pos
            && !collect_keys
        {
            break;
        }
        if let Some(sym) = key {
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
                        dest_slot: pos_count + i,
                    });
                    continue 'top;
                }
            }
            if sig.key_rest() == Rest::None {
                return Err(Error::unexpected_key(strand, unsafe {
                    Sym::from_tag(sym.tag)
                }));
            }
            if collect_keys {
                key_rest.push(idx);
            }
        } else if pos < pos_count {
            actions.push(Action {
                source_index: idx,
                dest_slot: pos,
            });
            pos += 1;
        } else if sig.pos_rest() == Rest::None {
            return Err(Error::unexpected_positional(strand, pos_count));
        } else if collect_pos {
            pos_rest.push(idx);
        }
    }

    if pos < sig.required {
        return Err(Error::missing_positional(strand, pos));
    }

    for (wanted, seen) in sig.keys.iter().zip(seen_keys.iter()) {
        if !*seen && wanted.default.is_none() {
            return Err(match &wanted.kind {
                sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                sig::UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
            });
        }
    }

    Ok(UnpackPlan {
        actions,
        pos_matched: pos,
        pos_rest,
        key_rest,
    })
}

/// Stores the tuple of a `*name` rest and the record of a `**name` rest.
fn store_split_rests<'v, 'a>(
    strand: &mut Strand<'v, '_>,
    sig: &sig::Unpack<'v, 'a>,
    out: &mut Slots<'v, 'a>,
    items: &[ArgItem<'v>],
    plan: &UnpackPlan,
) {
    if sig.variadic == Variadic::Capture {
        return;
    }
    if let Some(i) = sig.pos_rest_slot() {
        let values: Vec<_> = plan
            .pos_rest
            .iter()
            .map(|&index| items[index].1.dup())
            .collect();
        out.at(i)
            .store(Value::from_object(tuple::tuple(strand, values)));
    }
    if let Some(i) = sig.key_rest_slot() {
        let entries = plan
            .key_rest
            .iter()
            .map(|&index| {
                let (key, value) = &items[index];
                (key.clone().unwrap(), value.dup())
            })
            .collect();
        let record = Record::from_sym_entries(strand, entries);
        out.at(i).store(Value::from_object(GcObj::new(
            strand.vm().arena(),
            strand.builtin_types().record,
            record,
        )));
    }
}

fn fill_unpack_defaults<'v, 'a>(
    strand: &mut Strand<'v, '_>,
    sig: &sig::Unpack<'v, 'a>,
    out: &mut Slots<'v, 'a>,
    positional_matched: usize,
    actions: &[Action],
) {
    for (pos, default) in
        (positional_matched..).zip(sig.optional[(positional_matched - sig.required)..].iter())
    {
        out.at(pos).store(default.dup());
    }

    let pos_count = sig.required + sig.optional.len();
    for (i, wanted) in sig.keys.iter().enumerate() {
        let dest = pos_count + i;
        if actions.iter().all(|action| action.dest_slot != dest)
            && let Some(default) = &wanted.default
        {
            Output::set(strand, out.at(dest), default);
        }
    }
}

unsafe impl<'v> Collect for ArgPack<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in &self.inner {
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

unsafe impl<'v> Collect for ArgIter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.pack.accept(visit)
    }

    fn clear(&mut self) {
        self.skip.clear()
    }
}

impl<'v> Protocol<'v> for ArgPack<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().args)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<args>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.builtin_types().arg_iter.create(
            strand,
            ArgIter::new(this.to_strong(), HashSet::new(), 0, 0),
            out,
        );
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pack = this.borrow(strand)?;
        let plan = unpack_plan(strand, &pack.inner, &HashSet::new(), 0, sig)?;

        for action in &plan.actions {
            out.at(action.dest_slot)
                .store(pack.inner[action.source_index].1.dup())
        }

        fill_unpack_defaults(strand, sig, &mut out, plan.pos_matched, &plan.actions);
        store_split_rests(strand, sig, &mut out, &pack.inner, &plan);

        if sig.variadic == Variadic::Capture {
            let skip = plan
                .actions
                .iter()
                .map(|action| action.source_index)
                .collect();
            let pos = first_visible_index(&pack.inner, &skip, 0).unwrap_or(pack.inner.len());
            let positional_matched =
                i64::try_from(plan.pos_matched).map_err(|_| Error::overflow(strand))?;
            strand.builtin_types().arg_iter.create(
                strand,
                ArgIter::new(this.to_strong(), skip, pos, positional_matched),
                out.at(sig.len() - 1),
            );
        }

        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut int = 0i64;
        let pack = this.borrow(strand)?;
        for (key, value) in &pack.inner {
            let mut value = value.dup();
            if context == SpreadContext::Sequence {
                if let Some(key) = key {
                    value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_object(key.clone()), value.take()],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                } else {
                    value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_i64(strand, int), value.take()],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                    int += 1;
                }
            } else {
                if let Some(key) = key {
                    let mut key = Value::from_object(key.clone());
                    sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))?;
                } else {
                    sink.positional(strand, Slot::new(&mut value))?;
                }
            }
        }
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let len = i64::try_from(this.borrow(strand)?.inner.len())
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            _ => iter::iterable_get(strand, &this, field, out),
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
            sym::PUSH => {
                let mut pack = this.borrow_mut(strand)?;
                for arg in args {
                    match arg {
                        Arg::Pos(mut value) => pack.inner.push((None, value.take())),
                        Arg::Key(key, mut value) => {
                            pack.inner.push((Some(strand.sym_obj(key)), value.take()));
                        }
                    }
                }
                Ok(())
            }
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }
}

impl<'v> Protocol<'v> for ArgIter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<arg iter>")
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
        let mut iter = this.borrow_mut(strand)?;
        let pack_obj = iter.pack.clone();
        let pack = pack_obj
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let plan = unpack_plan(strand, &pack.inner, &iter.skip, iter.pos, sig)?;
        let len = pack.inner.len();

        for action in &plan.actions {
            iter.skip.insert(action.source_index);
            out.at(action.dest_slot)
                .store(pack.inner[action.source_index].1.dup());
        }
        fill_unpack_defaults(strand, sig, &mut out, plan.pos_matched, &plan.actions);
        // The rests take their items
        store_split_rests(strand, sig, &mut out, &pack.inner, &plan);
        iter.skip.extend(plan.pos_rest.iter().chain(&plan.key_rest));
        drop(pack);

        let pack = iter
            .pack
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let pos = first_visible_index(&pack.inner, &iter.skip, iter.pos).unwrap_or(len);
        drop(pack);
        iter.pos = pos;
        iter.int = iter
            .int
            .checked_add(
                i64::try_from(plan.pos_matched + plan.pos_rest.len())
                    .map_err(|_| Error::overflow(strand))?,
            )
            .ok_or_else(|| Error::overflow(strand))?;

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
        let mut iter = this.borrow_mut(strand)?;
        let pack = iter
            .pack
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let item = first_visible_index(&pack.inner, &iter.skip, iter.pos).map(|index| {
            let (key, value) = &pack.inner[index];
            (index, key.clone(), value.dup())
        });
        drop(pack);
        if let Some((index, key, value)) = item {
            iter.pos = index + 1;
            let key = match key {
                None => {
                    let key = Value::from_i64(strand, iter.int);
                    iter.int += 1;
                    key
                }
                Some(key) => Value::from_object(key),
            };
            out.store(Value::from_object(tuple::tuple(strand, [key, value])));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut iter = this.borrow_mut(strand)?;
        loop {
            let pack = iter
                .pack
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            let item = first_visible_index(&pack.inner, &iter.skip, iter.pos).map(|index| {
                let (key, value) = &pack.inner[index];
                (index, key.clone(), value.dup())
            });
            drop(pack);
            let Some((index, key, value)) = item else {
                break;
            };
            if context == SpreadContext::Sequence {
                if let Some(key) = key {
                    let mut value =
                        Value::from_object(tuple::tuple(strand, [Value::from_object(key), value]));
                    sink.positional(strand, Slot::new(&mut value))?;
                } else {
                    let mut value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_i64(strand, iter.int), value],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                    iter.int += 1;
                }
            } else {
                if let Some(key) = key {
                    let mut key = Value::from_object(key);
                    let mut value = value;
                    sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))?;
                } else {
                    let mut value = value;
                    sink.positional(strand, Slot::new(&mut value))?;
                }
            }
            iter.pos = index + 1;
        }
        Ok(())
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let iter = this.borrow(strand)?;
                let pack = iter
                    .pack
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                let len = i64::try_from(visible_len(&pack.inner, &iter.skip, iter.pos))
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            _ => iter::iter_get(strand, &this, field, out),
        }
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
