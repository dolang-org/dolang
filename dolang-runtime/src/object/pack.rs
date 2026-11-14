use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::{self, Arg},
    bytecode::{Rest as RestKind, Variadic},
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
    with_slots,
};

use super::{
    BoundMethod,
    arg::ArgAcc,
    iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        instance_mcall_fallback, is_special_mcall, type_mcall_fallback,
    },
    sym::SymObj,
    tuple,
};

/// An item of a `Pack` or an argument accumulator: its symbol key, or `None` for a
/// positional item
pub(crate) type ArgItem<'v> = (Option<GcObj<'v, SymObj>>, Value<'v>);

// ── Pack ──────────────────────────────────────────────────────────

/// An immutable sequence of positional and symbol-keyed items, in the order
/// they were given
pub(crate) struct Pack<'v> {
    items: Box<[ArgItem<'v>]>,
}

unsafe impl<'v> crate::gc::ImmState for Pack<'v> {
    const CYCLIC: bool = true;
}

impl<'v> crate::gc::Annex for Pack<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in &self.items {
            if let Some(key) = key {
                key.accept(visit)?;
            }
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }
}

pub(crate) type PackObj<'v> = crate::gc::Imm<Pack<'v>>;

impl<'v> GcObj<'v, PackObj<'v>> {
    /// The pack's items, which live out of line.
    pub(crate) fn items(&self) -> &[ArgItem<'v>] {
        self.project(|p| &p.get().items)
    }
}

impl<'v> Pack<'v> {
    pub(crate) fn new(items: Vec<ArgItem<'v>>) -> Self {
        Self {
            items: items.into_boxed_slice(),
        }
    }

    pub(crate) fn from_args(vm: &Vm<'v>, args: arg::Args<'v, '_>) -> Self {
        Self::new(pack_items(vm, args))
    }

    /// Builds a `Pack` of symbol-keyed entries, such as the leftovers for a `**name` rest.
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
}

/// The items of `args`, in order
pub(crate) fn pack_items<'v>(vm: &Vm<'v>, args: arg::Args<'v, '_>) -> Vec<ArgItem<'v>> {
    args.map(|arg| match arg {
        Arg::Pos(mut value) => (None, value.take()),
        Arg::Key(key, mut value) => (Some(vm.sym_obj(key)), value.take()),
    })
    .collect()
}

/// The index in `items` for `key`: the positional item it numbers if it is an
/// integer, counting from the end if it is negative, or the latest instance of
/// it if it is a symbol
pub(crate) fn find_index<'v>(
    strand: &mut Strand<'v, '_>,
    items: &[ArgItem<'v>],
    key: &Value<'v>,
) -> Option<usize> {
    if let Some(sym) = key.as_sym(strand) {
        return items
            .iter()
            .rposition(|(k, _)| k.as_ref().is_some_and(|k| k.tag() == sym.tag()));
    }
    let index = key.to_i64(strand).ok()?;
    let mut positions = items
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| k.is_none())
        .map(|(i, _)| i);
    if index >= 0 {
        positions.nth(usize::try_from(index).ok()?)
    } else {
        let back = usize::try_from(index.unsigned_abs() - 1).ok()?;
        positions.rev().nth(back)
    }
}

/// The value in `items` for `key`, as [`find_index`] finds it
pub(crate) fn find<'v, 'b>(
    strand: &mut Strand<'v, '_>,
    items: &'b [ArgItem<'v>],
    key: &Value<'v>,
) -> Option<&'b Value<'v>> {
    find_index(strand, items, key).map(|index| &items[index].1)
}

/// Writes `items` as the inside of an args literal.
/// Writes `items` as the arguments of a call, or with `braces`, as the items of
/// a brace literal
pub(crate) fn debug_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &[ArgItem<'v>],
    braces: bool,
    w: &mut dyn Format<'v>,
) -> Result<'v, 's, ()> {
    crate::fmt!(strand, w, "{}", if braces { "{" } else { "(" })?;
    for (index, (key, value)) in items.iter().enumerate() {
        if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
            strand.check_trap()?;
        }
        if index > 0 {
            crate::fmt!(strand, w, ", ")?;
        }
        if let Some(key) = key {
            crate::fmt!(strand, w, "{}: ", key.name())?;
        }
        value.op_debug(strand, w)?;
    }
    if braces {
        return crate::fmt!(strand, w, "}}");
    }
    // A lone positional item
    if let [(None, _)] = items {
        crate::fmt!(strand, w, ",")?;
    }
    crate::fmt!(strand, w, ")")
}

pub(crate) fn hash_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &[ArgItem<'v>],
    hasher: &mut DefaultHasher,
) -> Result<'v, 's, ()> {
    for (index, (key, value)) in items.iter().enumerate() {
        if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
            strand.check_trap()?;
        }
        key.as_ref().map(|key| key.tag()).hash(hasher);
        value.op_hash(strand, hasher)?;
    }
    Ok(())
}

pub(crate) fn eq_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: &[ArgItem<'v>],
    right: &[ArgItem<'v>],
) -> Result<'v, 's, bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (index, ((lkey, lvalue), (rkey, rvalue))) in left.iter().zip(right.iter()).enumerate() {
        if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
            strand.check_trap()?;
        }
        // Positions line up when every key before them does
        if lkey.as_ref().map(|key| key.tag()) != rkey.as_ref().map(|key| key.tag())
            || !lvalue.op_eq(strand, rvalue)?.to_bool(strand)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn lt_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: &[ArgItem<'v>],
    right: &[ArgItem<'v>],
) -> Result<'v, 's, bool> {
    let (mut lint, mut rint) = (0, 0);
    for (index, ((lkey, lvalue), (rkey, rvalue))) in left.iter().zip(right.iter()).enumerate() {
        if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
            strand.check_trap()?;
        }
        let lk = key_value(strand, lkey, lint);
        let rk = key_value(strand, rkey, rint);
        lint += i64::from(lkey.is_none());
        rint += i64::from(rkey.is_none());
        for (l, r) in [(&lk, &rk), (lvalue, rvalue)] {
            if !l.op_eq(strand, r)?.to_bool(strand)? {
                return l.op_lt(strand, r)?.to_bool(strand);
            }
        }
    }
    Ok(right.len() > left.len())
}

/// Spreads `items` into `sink` as positional and symbol-keyed items.
pub(crate) fn spread_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &[ArgItem<'v>],
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut slot]| {
        for (key, value) in items {
            Output::set(strand, &mut slot, value);
            if let Some(key) = key {
                let key = unsafe { Sym::from_obj(key) };
                sink.symbol(strand, key, Slot::reborrow(&mut slot))?;
            } else {
                sink.positional(strand, Slot::reborrow(&mut slot))?;
            }
        }
        Ok(())
    })
}

/// Creates an empty `Pack`.
pub(crate) fn empty<'v>(strand: &mut Strand<'v, '_>) -> GcObj<'v, PackObj<'v>> {
    let vm = strand.vm();
    GcObj::new_imm(vm.arena(), vm.builtin_types().pack, Pack::new(Vec::new()))
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
    let collect_pos = split && sig.pos_rest() == RestKind::Capture;
    let collect_keys = split && sig.key_rest() == RestKind::Capture;
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
            && sig.pos_rest() != RestKind::None
            && sig.key_rest() != RestKind::None
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
                    && wanted_sym.tag() == sym.tag()
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
            if sig.key_rest() == RestKind::None {
                unexpected_key.get_or_insert(sym.tag());
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
        } else if sig.pos_rest() == RestKind::None {
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

/// Stores the tuple of a `*name` rest and the `Pack` of a `**name` rest.
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
        tuple::create(strand, values, out.at(i));
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
        strand
            .builtin_types()
            .pack
            .create_imm(strand, Pack::from_sym_entries(entries), out.at(i));
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

/// Unpacks `args` against `sig`. A `...name` rest captures a view of
/// the items left over.
pub(crate) fn unpack<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: GcObj<'v, PackObj<'v>>,
    sig: &sig::Unpack<'v, 'a>,
    mut out: Slots<'v, 'a>,
) -> Result<'v, 's, ()> {
    let items = args.items();
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
        strand.builtin_types().pack_rest.create(
            strand,
            Rest::new(args, skip, pos, positional_matched),
            out.at(sig.len() - 1),
        );
    }

    Ok(())
}

// ── Iter ────────────────────────────────────────────────────────────

/// An iterator over a `Pack`'s `(key, value)` pairs.
pub(crate) struct Iter<'v> {
    args: GcObj<'v, PackObj<'v>>,
    skip: HashSet<usize>,
    pos: usize,
    int: i64,
}

impl<'v> Iter<'v> {
    pub(crate) fn new(
        args: GcObj<'v, PackObj<'v>>,
        skip: HashSet<usize>,
        pos: usize,
        int: i64,
    ) -> Self {
        Self {
            args,
            skip,
            pos,
            int,
        }
    }

    /// The next item not yet consumed, and its index
    fn next_item(&self) -> Option<(usize, Option<GcObj<'v, SymObj>>, Value<'v>)> {
        let items = self.args.items();
        first_visible_index(items, &self.skip, self.pos).map(|index| {
            let (key, value) = &items[index];
            (index, key.clone(), value.dup())
        })
    }

    /// Advances past the next item, returning it as a `(key, value)` pair
    fn next_pair(&mut self, strand: &mut Strand<'v, '_>) -> Option<Value<'v>> {
        let (index, key, value) = self.next_item()?;
        self.pos = index + 1;
        let positional = key.is_none();
        let key = key_value(strand, &key, self.int);
        self.int += i64::from(positional);
        Some(Value::from_object(tuple::tuple(strand, [key, value])))
    }
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.args.accept(visit)
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
        crate::fmt!(strand, w, "<pack iterator>")
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
        let iter = this.borrow(strand)?;
        let mut cursor = Iter::new(iter.args.clone(), iter.skip.clone(), iter.pos, iter.int);
        super::iter::unpack_cursor(strand, sig, &mut out, iter.pos, |strand, pos| {
            cursor.pos = pos;
            let pair = cursor.next_pair(strand)?;
            Some((cursor.pos, pair))
        })?;
        if let Some(i) = sig.pos_rest_slot() {
            strand
                .builtin_types()
                .pack_iter
                .create(strand, cursor, out.at(i));
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let Some(pair) = this.borrow_mut(strand)?.next_pair(strand) else {
            return Ok(false);
        };
        out.store(pair);
        Ok(true)
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        super::iter::spread_iter(strand, this, sink).await
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
                let len = i64::try_from(visible_len(iter.args.items(), &iter.skip, iter.pos))
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
        args: arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}

/// Read-only view of the unmatched items of a `Pack`.
pub(crate) struct Rest<'v> {
    args: GcObj<'v, PackObj<'v>>,
    skip: HashSet<usize>,
    pos: usize,
    int: i64,
}

impl<'v> Rest<'v> {
    pub(crate) fn new(
        args: GcObj<'v, PackObj<'v>>,
        skip: HashSet<usize>,
        pos: usize,
        int: i64,
    ) -> Self {
        Self {
            args,
            skip,
            pos,
            int,
        }
    }

    /// The next item not yet consumed, and its index
    fn next_item(&self) -> Option<(usize, Option<GcObj<'v, SymObj>>, Value<'v>)> {
        let items = self.args.items();
        first_visible_index(items, &self.skip, self.pos).map(|index| {
            let (key, value) = &items[index];
            (index, key.clone(), value.dup())
        })
    }
}

unsafe impl<'v> Collect for Rest<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.args.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Rest<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, TypeObject::Iterable)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_mcall(strand, &this, method, args, out).await
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<pack rest>")
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let mut iter = Rest::new(
            borrow.args.clone(),
            borrow.skip.clone(),
            borrow.pos,
            borrow.int,
        );
        let record_obj = iter.args.clone();
        let items = record_obj.items();
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
        let child = iter;

        if sig.variadic == Variadic::Capture {
            strand
                .builtin_types()
                .pack_rest
                .create(strand, child, out.at(sig.len() - 1));
        }

        Ok(())
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let iter = Iter::new(
            borrow.args.clone(),
            borrow.skip.clone(),
            borrow.pos,
            borrow.int,
        );
        strand.builtin_types().pack_iter.create(strand, iter, out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let mut iter = Rest::new(
            borrow.args.clone(),
            borrow.skip.clone(),
            borrow.pos,
            borrow.int,
        );
        strand.with_slots_sync(|strand, [mut slot]| {
            while let Some((index, key, value)) = iter.next_item() {
                Output::set(strand, &mut slot, value);
                if let Some(key) = key {
                    let key = unsafe { Sym::from_obj(&key) };
                    sink.symbol(strand, key, Slot::reborrow(&mut slot))?;
                } else {
                    sink.positional(strand, Slot::reborrow(&mut slot))?;
                }
                iter.pos = index + 1;
            }
            Ok(())
        })
    }
}

// ── Pack protocol ─────────────────────────────────────────────────

impl<'v> Protocol<'v> for PackObj<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().pack)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        debug_items(strand, &borrow.items, false, w)
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        Ok(!this.get().items.is_empty())
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        sym::PACK.hash(hasher);
        hash_items(strand, &borrow.items, hasher)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().pack) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.get();
        let right = other.items();
        Ok(Value::from_bool(eq_items(strand, &left.items, right)?))
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().pack) else {
            return Err(Error::unsupported(strand, "`<`"));
        };
        let left = this.get();
        let right = other.items();
        Ok(Value::from_bool(lt_items(strand, &left.items, right)?))
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
                "a pack only supports symbol and integer keys",
            ));
        }
        let borrow = this.get();
        match find(strand, &borrow.items, index) {
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
                let len = this.get().items.len();
                let len = i64::try_from(len).map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            sym::GET | sym::WITH | sym::WITHOUT => {
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
        args: arg::Args<'v, 'a>,
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
                let borrow = this.get();
                let found = find(strand, &borrow.items, &key).map(Value::dup);
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
            sym::WITH => {
                let ([], [], updates) = unpack!(strand, args, 0, 0, **)?;
                // Keep argument slots intact so updates remain rooted. This path
                // checks interrupts but never collects while building the result.
                let mut replacements = Vec::new();
                let mut indices = HashMap::new();
                for (i, (key, value)) in updates.enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    let index = *indices.entry(key.tag()).or_insert_with(|| {
                        replacements.push((key, None));
                        replacements.len() - 1
                    });
                    replacements[index].1 = Some(value.dup());
                }
                let borrow = this.get();
                let mut items = Vec::with_capacity(borrow.items.len());
                for (i, (key, value)) in borrow.items.iter().enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    if let Some(index) = key.as_ref().and_then(|key| indices.get(&key.tag())) {
                        if let Some(value) = replacements[*index].1.take() {
                            items.push((key.clone(), value));
                        }
                    } else {
                        items.push((key.clone(), value.dup()));
                    }
                }
                for (i, (key, value)) in replacements.into_iter().enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    if let Some(value) = value {
                        items.push((Some(strand.sym_obj(key)), value));
                    }
                }
                strand
                    .builtin_types()
                    .pack
                    .create_imm(strand, Pack::new(items), out);
                Ok(())
            }
            sym::WITHOUT => {
                let ([], [], keys) = unpack!(strand, args, 0, 0, *)?;
                let mut removed = HashSet::new();
                for (i, key) in keys.enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    let key = key
                        .as_sym(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected `Sym`"))?;
                    removed.insert(key.tag());
                }
                let borrow = this.get();
                let mut items = Vec::with_capacity(borrow.items.len());
                for (i, (key, value)) in borrow.items.iter().enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    if !key.as_ref().is_some_and(|key| removed.contains(&key.tag())) {
                        items.push((key.clone(), value.dup()));
                    }
                }
                strand
                    .builtin_types()
                    .pack
                    .create_imm(strand, Pack::new(items), out);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "pack.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
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
        strand.builtin_types().pack_iter.create(
            strand,
            Iter::new(this.to_strong(), HashSet::new(), 0, 0),
            out,
        );
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        spread_items(strand, &borrow.items, sink)
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

// ── Pack Class ────────────────────────────────────────────────────

/// Collects a source's pairs for `Pack src`, into an argument accumulator rooted in
/// a slot. A `Pack` holds only positional items and symbol keys.
struct PackPairs<'b, 'v> {
    pack: &'b Value<'v>,
}

impl<'v> PackPairs<'_, 'v> {
    fn push<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Option<GcObj<'v, SymObj>>,
        value: Value<'v>,
    ) -> Result<'v, 's, ()> {
        let pack = self
            .pack
            .downcast_ref(strand.builtin_types().arg_acc)
            .expect("source accumulator");
        pack.pin_sync(strand, |strand, pack| {
            pack.borrow_mut()
                .ok_or_else(|| Error::concurrency(strand))?
                .push((key, value));
            Ok(())
        })
    }
}

impl<'v, 's> Spread<'v, 's> for PackPairs<'_, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.push(strand, None, value.take())
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

    // Pairs from an iterable all arrive here. Only a symbol key fits, since
    // an integer key is a key, never a position.
    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Slot<'v, '_>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match key.as_sym(strand) {
            Some(sym) => self.symbol(strand, sym, value),
            None => Err(Error::unexpected_key(strand, &key)),
        }
    }
}

pub(crate) struct Class;

unsafe impl Collect for Class {
    const CYCLIC: bool = false;
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
        crate::fmt!(strand, w, "<type std.Pack>")
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
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::LT_METHOD),
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::GET),
                Method(sym::WITH),
                Method(sym::WITHOUT),
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
        args: arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val, items], []) = unpack!(strand, args, 2, 0)?;
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().pack, &mut native, items).await?;
                    self_val.op_fill(strand, &strand.singletons().pack, native.take())?;
                    Ok(())
                })
            }
            _ => {
                let vm = strand.vm();
                type_mcall_fallback(strand, &vm.singletons().pack, method, args, out).await
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
            | sym::WITH
            | sym::WITHOUT
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
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([source], []) = unpack!(strand, args, 1, 0)?;
        let items = source_items(strand, &source).await?;
        strand
            .builtin_types()
            .pack
            .create_imm(strand, Pack::new(items), out);
        Ok(())
    }
}

/// Collects the items of `source` for `Pack src`.
///
/// The items are unrooted, so the caller must store them before anything can
/// collect.
pub(crate) async fn source_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    source: &Value<'v>,
) -> Result<'v, 's, Vec<ArgItem<'v>>> {
    with_slots!(strand, |mut pack| {
        // Spreading can run arbitrary code, so the items collect in a
        // pack the GC can see
        strand.builtin_types().arg_acc.create(
            strand,
            ArgAcc::new(Vec::new()),
            Slot::reborrow(&mut pack),
        );
        let mut sink = PackPairs { pack: &pack };
        source
            .op_spread(strand, SpreadContext::Pairs, &mut sink)
            .await?;
        pack.downcast_ref(strand.builtin_types().arg_acc)
            .expect("source accumulator")
            .pin_sync(strand, |strand, pack| {
                Ok(pack
                    .borrow_mut()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .take())
            })
    })
}
