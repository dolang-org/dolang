use std::{
    collections::HashSet,
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::{Arg, Args},
    bytecode::{Rest, Variadic},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::protocol::members,
    sig,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod,
    arg::ArgPack,
    iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_mcall_fallback,
        is_special_mcall, type_mcall_fallback,
    },
    sym::SymObj,
    tuple,
};

/// An item of a record or argument pack: its symbol key, or `None` for a
/// positional item
pub(crate) type ArgItem<'v> = (Option<GcObj<'v, SymObj>>, Value<'v>);

// ── Record ──────────────────────────────────────────────────────────

/// An immutable sequence of positional and symbol-keyed items, in the order
/// they were given
pub(crate) struct Record<'v> {
    items: Box<[ArgItem<'v>]>,
}

unsafe impl<'v> Collect for Record<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in &self.items {
            if let Some(key) = key {
                key.accept(visit)?;
            }
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.items = Box::new([]);
    }
}

impl<'v> Record<'v> {
    pub(crate) fn new(items: Vec<ArgItem<'v>>) -> Self {
        Self {
            items: items.into_boxed_slice(),
        }
    }

    pub(crate) fn from_args(vm: &Vm<'v>, args: Args<'v, '_>) -> Self {
        Self::new(
            args.map(|arg| match arg {
                Arg::Pos(mut value) => (None, value.take()),
                Arg::Key(key, mut value) => (Some(vm.sym_obj(key)), value.take()),
            })
            .collect(),
        )
    }

    /// Builds a record of symbol-keyed entries, such as the leftovers for a `**name` rest.
    ///
    /// This never checks for a GC trap, so the caller may hold `entries` unrooted.
    pub(crate) fn from_sym_entries(entries: Vec<(GcObj<'v, SymObj>, Value<'v>)>) -> Self {
        Self::new(
            entries
                .into_iter()
                .map(|(key, value)| (Some(key), value))
                .collect(),
        )
    }

    pub(crate) fn items(&self) -> &[ArgItem<'v>] {
        &self.items
    }

    /// The value for `key`: the positional item it numbers if it is an integer,
    /// or the latest instance of it if it is a symbol
    fn find(&self, strand: &mut Strand<'v, '_>, key: &Value<'v>) -> Option<&Value<'v>> {
        if let Some(sym) = key.as_sym(strand) {
            self.items
                .iter()
                .rev()
                .find(|(k, _)| k.as_ref().is_some_and(|k| k.tag == sym.tag()))
                .map(|(_, value)| value)
        } else {
            let index = usize::try_from(key.to_i64(strand).ok()?).ok()?;
            self.items
                .iter()
                .filter(|(k, _)| k.is_none())
                .nth(index)
                .map(|(_, value)| value)
        }
    }
}

/// Creates an empty record.
pub(crate) fn empty<'v>(strand: &mut Strand<'v, '_>) -> GcObj<'v, Record<'v>> {
    let vm = strand.vm();
    GcObj::new(
        vm.arena(),
        vm.builtin_types().record,
        Record::new(Vec::new()),
    )
}

/// The key of an item as a value: the symbol, or the position `int` counts
fn key_value<'v>(
    strand: &mut Strand<'v, '_>,
    key: &Option<GcObj<'v, SymObj>>,
    int: i64,
) -> Value<'v> {
    match key {
        Some(key) => Value::from_object(key.clone()),
        None => Value::from_i64(strand, int),
    }
}

// ── Unpacking ───────────────────────────────────────────────────────

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
    // Reported after any missing item
    let mut unexpected_key = None;
    let mut unexpected_pos = false;

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
                unexpected_key.get_or_insert(sym.tag);
                continue;
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
            unexpected_pos = true;
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

    if unexpected_pos {
        return Err(Error::unexpected_positional(strand, pos_count));
    }
    if let Some(tag) = unexpected_key {
        return Err(Error::unexpected_key(strand, unsafe { Sym::from_tag(tag) }));
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
        out.at(i).store(Value::from_object(GcObj::new(
            strand.vm().arena(),
            strand.builtin_types().record,
            Record::from_sym_entries(entries),
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

/// Unpacks `record` against `sig`. A `...name` rest captures an iterator over
/// the items left over.
pub(crate) fn unpack<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    record: GcObj<'v, Record<'v>>,
    sig: &sig::Unpack<'v, 'a>,
    mut out: Slots<'v, 'a>,
) -> Result<'v, 's, ()> {
    let borrow = record.borrow().ok_or_else(|| Error::concurrency(strand))?;
    let items = &borrow.items;
    let plan = unpack_plan(strand, items, &HashSet::new(), 0, sig)?;

    for action in &plan.actions {
        out.at(action.dest_slot)
            .store(items[action.source_index].1.dup())
    }

    fill_unpack_defaults(strand, sig, &mut out, plan.pos_matched, &plan.actions);
    store_split_rests(strand, sig, &mut out, items, &plan);

    if sig.variadic == Variadic::Capture {
        let skip = plan
            .actions
            .iter()
            .map(|action| action.source_index)
            .collect();
        let pos = first_visible_index(items, &skip, 0).unwrap_or(items.len());
        let positional_matched =
            i64::try_from(plan.pos_matched).map_err(|_| Error::overflow(strand))?;
        drop(borrow);
        strand.builtin_types().record_iter.create(
            strand,
            Iter::new(record, skip, pos, positional_matched),
            out.at(sig.len() - 1),
        );
    }

    Ok(())
}

// ── Iter ────────────────────────────────────────────────────────────

/// An iterator over a record's `(key, value)` pairs, which is also the rest a
/// `...name` captures from one. Unpacking it consumes the items it matches.
pub(crate) struct Iter<'v> {
    record: GcObj<'v, Record<'v>>,
    skip: HashSet<usize>,
    pos: usize,
    int: i64,
}

impl<'v> Iter<'v> {
    pub(crate) fn new(
        record: GcObj<'v, Record<'v>>,
        skip: HashSet<usize>,
        pos: usize,
        int: i64,
    ) -> Self {
        Self {
            record,
            skip,
            pos,
            int,
        }
    }

    /// The next item not yet consumed, and its index
    fn next_item(&self) -> Option<(usize, Option<GcObj<'v, SymObj>>, Value<'v>)> {
        let record = self.record.borrow()?;
        first_visible_index(&record.items, &self.skip, self.pos).map(|index| {
            let (key, value) = &record.items[index];
            (index, key.clone(), value.dup())
        })
    }
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.record.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Iter<'v> {
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
        crate::fmt!(strand, w, "<record iterator>")
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
        let record_obj = iter.record.clone();
        let record = record_obj
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let items = &record.items;
        let plan = unpack_plan(strand, items, &iter.skip, iter.pos, sig)?;

        for action in &plan.actions {
            iter.skip.insert(action.source_index);
            out.at(action.dest_slot)
                .store(items[action.source_index].1.dup());
        }
        fill_unpack_defaults(strand, sig, &mut out, plan.pos_matched, &plan.actions);
        // The rests take their items
        store_split_rests(strand, sig, &mut out, items, &plan);
        iter.skip.extend(plan.pos_rest.iter().chain(&plan.key_rest));

        iter.pos = first_visible_index(items, &iter.skip, iter.pos).unwrap_or(items.len());
        iter.int = iter
            .int
            .checked_add(
                i64::try_from(plan.pos_matched + plan.pos_rest.len())
                    .map_err(|_| Error::overflow(strand))?,
            )
            .ok_or_else(|| Error::overflow(strand))?;
        drop(record);
        drop(iter);

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
        let Some((index, key, value)) = iter.next_item() else {
            return Ok(false);
        };
        iter.pos = index + 1;
        let positional = key.is_none();
        let key = key_value(strand, &key, iter.int);
        iter.int += i64::from(positional);
        out.store(Value::from_object(tuple::tuple(strand, [key, value])));
        Ok(true)
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut iter = this.borrow_mut(strand)?;
        while let Some((index, key, mut value)) = iter.next_item() {
            if context == SpreadContext::Sequence {
                let positional = key.is_none();
                let key = key_value(strand, &key, iter.int);
                iter.int += i64::from(positional);
                let mut pair = Value::from_object(tuple::tuple(strand, [key, value]));
                sink.positional(strand, Slot::new(&mut pair))?;
            } else if let Some(key) = key {
                let key = unsafe { Sym::from_obj(&key) };
                sink.symbol(strand, key, Slot::new(&mut value))?;
            } else {
                sink.positional(strand, Slot::new(&mut value))?;
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
                let record = iter
                    .record
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                let len = i64::try_from(visible_len(&record.items, &iter.skip, iter.pos))
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

// ── Record protocol ─────────────────────────────────────────────────

impl<'v> Protocol<'v> for Record<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().record)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let items = &borrow.items;
        crate::fmt!(strand, w, "(")?;
        for (index, (key, value)) in items.iter().enumerate() {
            if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            if index > 0 {
                crate::fmt!(strand, w, ", ")?;
            }
            if let Some(key) = key {
                crate::fmt!(strand, w, "{}: ", key.name)?;
            }
            value.op_debug(strand, w)?;
        }
        // A lone positional item
        if let [(None, _)] = &items[..] {
            crate::fmt!(strand, w, ",")?;
        }
        crate::fmt!(strand, w, ")")
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let Ok(borrow) = this.borrow(strand) else {
            return true;
        };
        !borrow.items.is_empty()
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        sym::RECORD.hash(hasher);
        for (index, (key, value)) in borrow.items.iter().enumerate() {
            if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            key.as_ref().map(|key| key.tag).hash(hasher);
            value.op_hash(strand, hasher)?;
        }
        Ok(())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().record) else {
            return Ok(Value::FALSE);
        };
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        if left.items.len() != right.items.len() {
            return Ok(Value::FALSE);
        }
        for (index, ((lkey, lvalue), (rkey, rvalue))) in
            left.items.iter().zip(right.items.iter()).enumerate()
        {
            if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            // Positions line up when every key before them does
            if lkey.as_ref().map(|key| key.tag) != rkey.as_ref().map(|key| key.tag)
                || !lvalue.op_eq(strand, rvalue).to_bool(strand)
            {
                return Ok(Value::FALSE);
            }
        }
        Ok(Value::TRUE)
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().record) else {
            return Err(Error::not_supported(strand));
        };
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        let (mut lint, mut rint) = (0, 0);
        for (index, ((lkey, lvalue), (rkey, rvalue))) in
            left.items.iter().zip(right.items.iter()).enumerate()
        {
            if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            let lk = key_value(strand, lkey, lint);
            let rk = key_value(strand, rkey, rint);
            lint += i64::from(lkey.is_none());
            rint += i64::from(rkey.is_none());
            if lk.op_lt(strand, &rk)?.to_bool(strand) {
                return Ok(Value::TRUE);
            }
            if lvalue.op_lt(strand, rvalue)?.to_bool(strand) {
                return Ok(Value::TRUE);
            }
        }
        Ok(Value::from_bool(right.items.len() > left.items.len()))
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if !index.is_int(strand) && index.as_sym(strand).is_none() {
            return Err(Error::type_error(
                strand,
                "records only support symbol and integer keys",
            ));
        }
        let borrow = this.borrow(strand)?;
        match borrow.find(strand, index) {
            Some(value) => {
                Output::set(strand, out, value);
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_assign<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _key: Slot<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let len = this.borrow(strand)?.items.len();
                let len = i64::try_from(len).map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            sym::GET => {
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
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([key], [default, or_else]) =
                    unpack!(strand, args, 1, 0, default = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let found = this.borrow(strand)?.find(strand, &key).map(Value::dup);
                if let Some(value) = found {
                    out.store(value);
                    Ok(())
                } else if let Some(mut default) = default {
                    out.store(default.take());
                    Ok(())
                } else if let Some(or_else) = or_else {
                    call!(strand, or_else, out).await
                } else {
                    out.store(Value::NIL);
                    Ok(())
                }
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "record.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
                    .await
                    .expect("supported special method")
            }
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }

    fn op_set<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        _field: Sym<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.builtin_types().record_iter.create(
            strand,
            Iter::new(this.to_strong(), HashSet::new(), 0, 0),
            out,
        );
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let mut int = 0;
        for (key, value) in &borrow.items {
            let mut value = value.dup();
            if context == SpreadContext::Sequence {
                int += i64::from(key.is_none());
                let key = key_value(strand, key, int - i64::from(key.is_none()));
                let mut pair = Value::from_object(tuple::tuple(strand, [key, value]));
                sink.positional(strand, Slot::new(&mut pair))?;
            } else if let Some(key) = key {
                let key = unsafe { Sym::from_obj(key) };
                sink.symbol(strand, key, Slot::new(&mut value))?;
            } else {
                sink.positional(strand, Slot::new(&mut value))?;
            }
        }
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack(strand, this.to_strong(), sig, out)
    }
}

// ── Record Class ────────────────────────────────────────────────────

/// Collects a source's pairs for `Record src`, into an argument pack rooted in
/// a slot. A record holds only positional items and symbol keys.
struct RecordPairs<'b, 'v> {
    int: i64,
    pack: &'b Value<'v>,
}

impl<'v> RecordPairs<'_, 'v> {
    fn push<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Option<GcObj<'v, SymObj>>,
        value: Value<'v>,
    ) -> Result<'v, 's, ()> {
        let pack = self
            .pack
            .downcast_ref(strand.builtin_types().arg_pack)
            .expect("record source pack");
        pack.borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?
            .push((key, value));
        Ok(())
    }
}

impl<'v, 's> Spread<'v, 's> for RecordPairs<'_, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.push(strand, None, value.take())?;
        self.int = self
            .int
            .checked_add(1)
            .ok_or_else(|| Error::overflow(strand))?;
        Ok(())
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let key = strand.sym_obj(key);
        self.push(strand, Some(key), value.take())
    }

    // Pairs from an iterable all arrive here. An integer key is positional
    // while it counts up from 0, as when spreading a dict.
    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Slot<'v, '_>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        if let Some(sym) = key.as_sym(strand) {
            self.symbol(strand, sym, value)
        } else if key.to_i64(strand).ok() == Some(self.int) {
            self.positional(strand, value)
        } else {
            Err(Error::type_error(strand, "record keys must be symbols"))
        }
    }
}

pub(crate) struct Class;

unsafe impl Collect for Class {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Class {
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
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Record>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::GET),
                Method(sym::INDEX_METHOD),
                Method(sym::ITER_METHOD),
                Method(sym::UNPACK_METHOD),
                Method(sym::SPREAD_METHOD),
            ],
        })
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
                strand
                    .with_slots(async |strand, [mut native]| {
                        call!(strand, &strand.singletons().record, &mut native, items).await?;
                        self_val.op_fill(strand, &strand.singletons().record, native.take())?;
                        Ok(())
                    })
                    .await
            }
            _ => {
                let vm = strand.vm();
                type_mcall_fallback(strand, &vm.singletons().record, method, args, out).await
            }
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::INIT_METHOD
            | sym::LEN
            | sym::GET
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::LT_METHOD
            | sym::HASH_METHOD
            | sym::INDEX_METHOD
            | sym::ITER_METHOD
            | sym::UNPACK_METHOD
            | sym::SPREAD_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        strand
            .with_slots(async |strand, [mut pack]| {
                // Spreading can run arbitrary code, so the items collect in a
                // pack the GC can see
                strand.builtin_types().arg_pack.create(
                    strand,
                    ArgPack::new(Vec::new()),
                    Slot::reborrow(&mut pack),
                );
                let mut sink = RecordPairs {
                    int: 0,
                    pack: &pack,
                };
                items
                    .op_spread(strand, SpreadContext::Pairs, &mut sink)
                    .await?;
                let items = pack
                    .downcast_ref(strand.builtin_types().arg_pack)
                    .expect("record source pack")
                    .borrow_mut()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .take();
                strand
                    .builtin_types()
                    .record
                    .create(strand, Record::new(items), out);
                Ok(())
            })
            .await
    }
}
