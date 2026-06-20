use std::{collections::HashSet, fmt, ops::ControlFlow};

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
    protocol::{GcObj, Protocol, Recv, Spread, SpreadContext},
    tuple,
};

pub(crate) type ArgItem<'v> = (Option<GcObj<'v, SymObj>>, Value<'v>);

pub(crate) struct ArgPack<'v> {
    inner: Vec<ArgItem<'v>>,
    has_keys: bool,
}

pub(crate) struct ArgIter<'v> {
    pack: GcObj<'v, ArgPack<'v>>,
    skip: HashSet<usize>,
    pos: usize,
    int: i64,
    pos_only: bool,
}

struct Action {
    source_index: usize,
    dest_slot: usize,
}

struct UnpackPlan {
    actions: Vec<Action>,
    pos_matched: usize,
}

impl<'v> ArgPack<'v> {
    pub(crate) fn new(inner: Vec<ArgItem<'v>>) -> Self {
        let has_keys = inner.iter().any(|(key, _)| key.is_some());
        Self { inner, has_keys }
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
        positional_only: bool,
    ) -> Self {
        Self {
            pack,
            skip,
            pos,
            int,
            pos_only: positional_only,
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

    'top: for (idx, (key, _)) in items.iter().enumerate().skip(start) {
        if skip.contains(&idx) {
            continue;
        }
        if pos == pos_count && keys_left == 0 && sig.variadic != Variadic::None {
            break;
        }
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

    if pos < sig.required {
        return Err(Error::missing_positional(strand, pos));
    }

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

    Ok(UnpackPlan {
        actions,
        pos_matched: pos,
    })
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

    for (i, wanted) in sig.keys.iter().enumerate() {
        let dest = sig.required + i;
        if actions.iter().all(|action| action.dest_slot != dest)
            && let Some(default) = &wanted.default
        {
            Output::set(strand, out.at(dest), default);
        }
    }
}

fn first_visible_key<'v>(
    items: &[ArgItem<'v>],
    skip: &HashSet<usize>,
    start: usize,
) -> Option<Sym<'v, 'static>> {
    for (idx, (key, _)) in items.iter().enumerate().skip(start) {
        if skip.contains(&idx) {
            continue;
        }
        if let Some(sym) = key {
            return Some(unsafe { Sym::from_tag(sym.tag) });
        }
    }
    None
}

fn split_skip_sets<'v>(
    items: &[ArgItem<'v>],
    skip: &HashSet<usize>,
    start: usize,
) -> (HashSet<usize>, HashSet<usize>) {
    let mut pos_skip = skip.clone();
    let mut key_skip = skip.clone();
    for index in 0..start {
        pos_skip.insert(index);
        key_skip.insert(index);
    }
    for (index, (key, _)) in items.iter().enumerate().skip(start) {
        if skip.contains(&index) {
            continue;
        }
        if key.is_some() {
            pos_skip.insert(index);
        } else {
            key_skip.insert(index);
        }
    }
    (pos_skip, key_skip)
}

unsafe impl<'v> Collect for ArgPack<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().args.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<args>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().arg_iter,
            ArgIter::new(this.to_strong(), HashSet::new(), 0, 0, false),
        )));
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let pack = this.get();
        let plan = unpack_plan(strand, &pack.inner, &HashSet::new(), 0, sig)?;

        for action in &plan.actions {
            out.at(action.dest_slot)
                .store(pack.inner[action.source_index].1.dup())
        }

        fill_unpack_defaults(strand, sig, &mut out, plan.pos_matched, &plan.actions);

        if sig.variadic == Variadic::Capture {
            let skip = plan
                .actions
                .iter()
                .map(|action| action.source_index)
                .collect();
            let pos = first_visible_index(&pack.inner, &skip, 0).unwrap_or(pack.inner.len());
            let positional_matched =
                i64::try_from(plan.pos_matched).map_err(|_| Error::overflow(strand))?;
            out.at(sig.len() - 1).store(Value::from_object(GcObj::new(
                strand.arena(),
                strand.builtin_types().arg_iter,
                ArgIter::new(this.to_strong(), skip, pos, positional_matched, false),
            )));
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
        for (key, value) in &this.get().inner {
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
                let len =
                    i64::try_from(this.get().inner.len()).map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            sym::POS_ONLY | sym::POS_KEYS => {
                BoundMethod::create(strand, &this, field, out);
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::POS_ONLY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let pack = this.get();
                if pack.has_keys {
                    return Err(Error::unexpected_key(
                        strand,
                        first_visible_key(&pack.inner, &HashSet::new(), 0)
                            .expect("has_keys implies a key"),
                    ));
                }
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().arg_iter,
                    ArgIter::new(this.to_strong(), HashSet::new(), 0, 0, true),
                )));
                Ok(())
            }
            sym::POS_KEYS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let pack = this.get();
                let (pos_skip, key_skip) = split_skip_sets(&pack.inner, &HashSet::new(), 0);
                let pos_pos =
                    first_visible_index(&pack.inner, &pos_skip, 0).unwrap_or(pack.inner.len());
                let key_pos =
                    first_visible_index(&pack.inner, &key_skip, 0).unwrap_or(pack.inner.len());
                out.store(Value::from_object(tuple::tuple(
                    strand,
                    [
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().arg_iter,
                            ArgIter::new(this.to_strong(), pos_skip, pos_pos, 0, true),
                        )),
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().arg_iter,
                            ArgIter::new(this.to_strong(), key_skip, key_pos, 0, false),
                        )),
                    ],
                )));
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
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<arg iter>").into_do(strand)
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
        let plan = unpack_plan(strand, &iter.pack.inner, &iter.skip, iter.pos, sig)?;

        for action in &plan.actions {
            let value = iter.pack.inner[action.source_index].1.dup();
            iter.skip.insert(action.source_index);
            out.at(action.dest_slot).store(value);
        }
        fill_unpack_defaults(strand, sig, &mut out, plan.pos_matched, &plan.actions);

        let len = iter.pack.inner.len();
        iter.pos = first_visible_index(&iter.pack.inner, &iter.skip, iter.pos).unwrap_or(len);
        iter.int = iter
            .int
            .checked_add(i64::try_from(plan.pos_matched).map_err(|_| Error::overflow(strand))?)
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
        if let Some(index) = first_visible_index(&iter.pack.inner, &iter.skip, iter.pos) {
            let (key, value) = {
                let (key, value) = &iter.pack.inner[index];
                (key.clone(), value.dup())
            };
            iter.pos = index + 1;
            if iter.pos_only {
                debug_assert!(
                    key.is_none(),
                    "positional-only iterators skip keyed entries"
                );
                out.store(value);
                return Ok(true);
            }

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
        while let Some(index) = first_visible_index(&iter.pack.inner, &iter.skip, iter.pos) {
            let (key, value) = &iter.pack.inner[index];
            if context == SpreadContext::Sequence && !iter.pos_only {
                if let Some(key) = key {
                    let mut value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_object(key.clone()), value.dup()],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                } else {
                    let mut value = Value::from_object(tuple::tuple(
                        strand,
                        [Value::from_i64(strand, iter.int), value.dup()],
                    ));
                    sink.positional(strand, Slot::new(&mut value))?;
                    iter.int += 1;
                }
            } else {
                if let Some(key) = key {
                    let mut key = Value::from_object(key.clone());
                    let mut value = value.dup();
                    sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))?;
                } else {
                    let mut value = value.dup();
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
                let len = i64::try_from(visible_len(&iter.pack.inner, &iter.skip, iter.pos))
                    .map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            sym::POS_ONLY | sym::POS_KEYS => {
                BoundMethod::create(strand, &this, field, out);
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::POS_ONLY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let iter = this.borrow(strand)?;
                if let Some(sym) = first_visible_key(&iter.pack.inner, &iter.skip, iter.pos) {
                    return Err(Error::unexpected_key(strand, sym));
                }
                let mut skip = iter.skip.clone();
                for index in 0..iter.pos {
                    skip.insert(index);
                }
                let pos = first_visible_index(&iter.pack.inner, &skip, iter.pos)
                    .unwrap_or(iter.pack.inner.len());
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().arg_iter,
                    ArgIter::new(iter.pack.clone(), skip, pos, 0, true),
                )));
                Ok(())
            }
            sym::POS_KEYS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let iter = this.borrow(strand)?;
                let (pos_skip, key_skip) = split_skip_sets(&iter.pack.inner, &iter.skip, iter.pos);
                let pos_pos = first_visible_index(&iter.pack.inner, &pos_skip, iter.pos)
                    .unwrap_or(iter.pack.inner.len());
                let key_pos = first_visible_index(&iter.pack.inner, &key_skip, iter.pos)
                    .unwrap_or(iter.pack.inner.len());
                out.store(Value::from_object(tuple::tuple(
                    strand,
                    [
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().arg_iter,
                            ArgIter::new(iter.pack.clone(), pos_skip, pos_pos, 0, true),
                        )),
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().arg_iter,
                            ArgIter::new(iter.pack.clone(), key_skip, key_pos, 0, false),
                        )),
                    ],
                )));
                Ok(())
            }
            _ => iter::iter_mcall(strand, &this, method, args, out).await,
        }
    }
}
