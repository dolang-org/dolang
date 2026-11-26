use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash},
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use crate::{
    arg::{self, Arg},
    bytecode::Rest as RestKind,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::{Unpack, UnpackItem, members},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
    vm::{Alloc, AllocExt, Vm},
    with_slots,
};

use super::{
    BoundMethod,
    arg::ArgAcc,
    iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        type_mcall_fallback,
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

    /// Store a pack of `args` into `out`.
    pub(crate) fn from_args(
        strand: &mut Strand<'v, '_>,
        args: arg::Args<'v, '_>,
        out: impl Output<'v>,
    ) {
        let embryo = strand.builtin_types().pack.embryo(strand);
        // SAFETY: the pack is allocated; the items move straight into it
        let items = unsafe { pack_items(strand, args) };
        embryo.finish_imm(strand, Self::new(items), out);
    }

    /// Builds a `Pack` of symbol-keyed entries, such as the leftovers for a `**name` rest.
    ///
    /// Build it between allocating the pack's embryo and finishing it, moving or
    /// duplicating the entries out of rooted storage.
    pub(crate) fn from_sym_entries(
        entries: impl IntoIterator<Item = (GcObj<'v, SymObj>, Value<'v>)>,
    ) -> Self {
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

/// The items of `args`, in order, taken from their slots.
///
/// # Safety
/// The items are loose, so build them between allocating the target's embryo and
/// finishing it.
pub(crate) unsafe fn pack_items<'v>(
    alloc: &mut dyn Alloc<'v>,
    args: arg::Args<'v, '_>,
) -> Vec<ArgItem<'v>> {
    let vm = alloc.alloc_vm(crate::vm::private::Sealed);
    // SAFETY: the caller's contract
    args.map(|arg| unsafe {
        match arg {
            Arg::Pos(mut value) => (None, value.take(alloc)),
            Arg::Key(key, mut value) => (Some(vm.sym_obj(key)), value.take(alloc)),
        }
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
        value.debug(strand, w)?;
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
        value.hash(strand, hasher)?;
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
            || !lvalue.eq(strand, rvalue)?
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
    with_slots!(strand, |mut lk, mut rk| {
        for (index, ((lkey, lvalue), (rkey, rvalue))) in left.iter().zip(right.iter()).enumerate() {
            if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            set_key(strand, lkey, lint, Slot::reborrow(&mut lk));
            set_key(strand, rkey, rint, Slot::reborrow(&mut rk));
            lint += i64::from(lkey.is_none());
            rint += i64::from(rkey.is_none());
            for (l, r) in [(&*lk, &*rk), (lvalue, rvalue)] {
                if !l.eq(strand, r)? {
                    return l.lt(strand, r);
                }
            }
        }
        Ok(right.len() > left.len())
    })
}

/// Spreads `items` into `sink` as positional and symbol-keyed items.
pub(crate) fn spread_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    items: &[ArgItem<'v>],
    sink: &mut dyn Spread<'v, 's>,
) -> Result<'v, 's, ()> {
    with_slots!(strand, |mut slot| {
        for (key, value) in items {
            strand.store(&mut slot, value);
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

/// Places the key of an item into `out`: the symbol, or the position `int` counts
fn set_key<'v>(
    strand: &mut Strand<'v, '_>,
    key: &Option<GcObj<'v, SymObj>>,
    int: i64,
    out: Slot<'v, '_>,
) {
    match key {
        Some(key) => strand.store(out, key),
        None => strand.store(out, int),
    }
}

// ── Unpacking ───────────────────────────────────────────────────────

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

/// What an unpack of a pack's items matched, and the rests it still has to fill.
struct Matched<'v, 'a> {
    /// Items the parameters took
    taken: HashSet<usize>,
    /// Positional parameters filled from items
    pos_matched: usize,
    /// Leftover positional items
    pos_left: Vec<usize>,
    /// Leftover keyed items
    key_left: Vec<usize>,
    /// A `...name` rest
    rest: Option<Slot<'v, 'a>>,
    /// A `*name` rest
    pos_rest: Option<Slot<'v, 'a>>,
    /// A `**name` rest
    key_rest: Option<Slot<'v, 'a>>,
}

/// Unpacks the visible items of `items` from `start` into the parameters of
/// `unpack`, leaving its rests to the caller.
fn match_items<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    unpack: Unpack<'v, 'a>,
    items: &[ArgItem<'v>],
    skip: &HashSet<usize>,
    start: usize,
) -> Result<'v, 's, Matched<'v, 'a>> {
    let (pos_rest, key_rest) = (unpack.pos_rest(), unpack.key_rest());
    let visible = (start..items.len()).filter(|index| !skip.contains(index));
    let mut positional = visible.clone().filter(|&index| items[index].0.is_none());
    let mut matched = Matched {
        taken: HashSet::new(),
        pos_matched: 0,
        pos_left: Vec::new(),
        key_left: Vec::new(),
        rest: None,
        pos_rest: None,
        key_rest: None,
    };
    let mut position = 0;
    for entry in unpack {
        let (slot, found, default) = match entry {
            UnpackItem::Pos { slot, default } => {
                let found = positional.next();
                if found.is_some() {
                    matched.pos_matched += 1;
                } else if default.is_none() {
                    return Err(Error::missing_positional(strand, position));
                }
                position += 1;
                (slot, found, default)
            }
            UnpackItem::SymKey { key, slot, default } => {
                let found = visible.clone().find(|index| {
                    !matched.taken.contains(index)
                        && items[*index]
                            .0
                            .as_ref()
                            .is_some_and(|k| k.tag() == key.tag())
                });
                if found.is_none() && default.is_none() {
                    return Err(Error::missing_key(strand, key));
                }
                (slot, found, default)
            }
            // A pack's keys are all symbols
            UnpackItem::ConstKey { key, slot, default } => match default {
                Some(_) => (slot, None, default),
                None => return Err(Error::missing_key(strand, key)),
            },
            UnpackItem::Rest { slot } => {
                matched.rest = Some(slot);
                continue;
            }
            UnpackItem::PosRest { slot } => {
                matched.pos_rest = Some(slot);
                continue;
            }
            UnpackItem::KeyRest { slot } => {
                matched.key_rest = Some(slot);
                continue;
            }
        };
        match (found, default) {
            (Some(index), _) => {
                matched.taken.insert(index);
                strand.store(slot, &items[index].1);
            }
            (None, Some(default)) => strand.store(slot, default),
            (None, None) => unreachable!("missing items are reported above"),
        }
    }
    matched.pos_left = positional.collect();
    matched.key_left = visible
        .filter(|index| items[*index].0.is_some() && !matched.taken.contains(index))
        .collect();
    if pos_rest == RestKind::None && !matched.pos_left.is_empty() {
        return Err(Error::unexpected_positional(strand, position));
    }
    if key_rest == RestKind::None
        && let Some(&index) = matched.key_left.first()
    {
        // SAFETY: only keyed items are leftover keys
        let key = unsafe { items[index].0.as_ref().unwrap_unchecked() };
        return Err(Error::unexpected_key(strand, unsafe {
            Sym::from_tag(key.tag())
        }));
    }
    Ok(matched)
}

impl<'v, 'a> Matched<'v, 'a> {
    /// Stores the tuple of a `*name` rest and the `Pack` of a `**name` rest.
    fn store_split_rests(&mut self, strand: &mut Strand<'v, '_>, items: &[ArgItem<'v>]) {
        if let Some(slot) = self.pos_rest.take() {
            let embryo = tuple::embryo(strand, self.pos_left.len());
            // SAFETY: the tuple is allocated; the copies move straight into it
            let values = self
                .pos_left
                .iter()
                .map(|&index| unsafe { items[index].1.dup() });
            embryo.finish(strand, values, slot);
        }
        if let Some(slot) = self.key_rest.take() {
            let embryo = strand.builtin_types().pack.embryo(strand);
            // SAFETY: the entries move straight into the allocated pack
            let entries = self.key_left.iter().map(|&index| {
                let (key, value) = &items[index];
                unsafe { (key.as_ref().unwrap_unchecked().dup(), value.dup()) }
            });
            embryo.finish_imm(strand, Pack::from_sym_entries(entries), slot);
        }
    }
}

/// Unpacks `args` against `unpack`. A `...name` rest captures a view of
/// the items left over.
pub(crate) fn unpack<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: &GcObj<'v, PackObj<'v>>,
    unpack: Unpack<'v, '_>,
) -> Result<'v, 's, ()> {
    let items = args.items();
    let mut matched = match_items(strand, unpack, items, &HashSet::new(), 0)?;
    matched.store_split_rests(strand, items);
    if let Some(slot) = matched.rest {
        let pos = first_visible_index(items, &matched.taken, 0).unwrap_or(items.len());
        let positional_matched =
            i64::try_from(matched.pos_matched).map_err(|_| Error::overflow(strand))?;
        let embryo = strand.builtin_types().pack_rest.embryo(strand);
        // SAFETY: the rest is allocated; the copy of the rooted `args` moves straight
        // into it
        let args = unsafe { args.dup() };
        embryo.finish(
            strand,
            Rest::new(args, matched.taken, pos, positional_matched),
            slot,
        );
    }
    Ok(())
}

/// The key of an item gathered for a new pack: one an existing pack holds, or a new
/// symbol.
enum ItemKey<'v, 'a> {
    Held(&'a GcObj<'v, SymObj>),
    New(Sym<'v, 'a>),
}

/// Copy gathered items for a new pack's payload.
///
/// Call it only once the pack is allocated, and finish the pack with the result at
/// once: the copies are loose until then.
fn copy_items<'v>(
    vm: &Vm<'v>,
    items: Vec<(Option<ItemKey<'v, '_>>, &Value<'v>)>,
) -> Vec<ArgItem<'v>> {
    items
        .into_iter()
        // SAFETY: the caller has allocated the pack, which takes the copies at once;
        // the gathered references are rooted where they were found
        .map(|(key, value)| unsafe {
            let key = key.map(|key| match key {
                ItemKey::Held(key) => key.dup(),
                ItemKey::New(key) => vm.sym_obj(key),
            });
            (key, value.dup())
        })
        .collect()
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

    /// Advances past the next item, placing it into `out` as a `(key, value)` pair
    fn next_pair(&mut self, strand: &mut Strand<'v, '_>, out: impl Output<'v>) -> bool {
        let items = self.args.items();
        let Some(index) = first_visible_index(items, &self.skip, self.pos) else {
            return false;
        };
        let (key, value) = &items[index];
        match key {
            Some(key) => tuple::create_pair(strand, key, value, out),
            None => {
                tuple::create_pair(strand, self.int, value, out);
                self.int += 1;
            }
        }
        self.pos = index + 1;
        true
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "PackIter";

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

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = this.borrow(strand)?;
        // SAFETY: TODO(#1707): the cursor's reference is loose across the unpack
        let args = unsafe { iter.args.dup() };
        let cursor = Iter::new(args, iter.skip.clone(), iter.pos, iter.int);
        unpack.cursor(
            strand,
            cursor,
            |strand, cursor, slot| cursor.next_pair(strand, slot),
            |strand, cursor, slot| {
                strand
                    .builtin_types()
                    .pack_iter
                    .create(strand, cursor, slot);
                Ok(())
            },
        )
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        Ok(this.borrow_mut(strand)?.next_pair(strand, out))
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
                strand.store(out, len);
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
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
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

    /// The first item at or after `pos` not yet consumed, borrowed, and its index
    fn next_item(&self, pos: usize) -> Option<(usize, Option<&GcObj<'v, SymObj>>, &Value<'v>)> {
        let items = self.args.items();
        first_visible_index(items, &self.skip, pos).map(|index| {
            let (key, value) = &items[index];
            (index, key.as_ref(), value)
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "PackRest";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, TypeObject::Iterable)
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

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        // SAFETY: TODO(#1707): the child's reference is loose across the unpack
        let args = unsafe { borrow.args.dup() };
        let mut iter = Rest::new(args, borrow.skip.clone(), borrow.pos, borrow.int);
        let items = iter.args.items();
        let mut matched = match_items(strand, unpack, items, &iter.skip, iter.pos)?;
        iter.skip.extend(matched.taken.iter().copied());
        let mut consumed = matched.pos_matched;
        // The split rests take their items
        if matched.pos_rest.is_some() {
            iter.skip.extend(matched.pos_left.iter().copied());
            consumed += matched.pos_left.len();
        }
        if matched.key_rest.is_some() {
            iter.skip.extend(matched.key_left.iter().copied());
        }
        matched.store_split_rests(strand, items);
        iter.pos = first_visible_index(items, &iter.skip, iter.pos).unwrap_or(items.len());
        iter.int = iter
            .int
            .checked_add(i64::try_from(consumed).map_err(|_| Error::overflow(strand))?)
            .ok_or_else(|| Error::overflow(strand))?;
        if let Some(slot) = matched.rest {
            strand.builtin_types().pack_rest.create(strand, iter, slot);
        }
        Ok(())
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let embryo = strand.builtin_types().pack_iter.embryo(strand);
        // SAFETY: the reference moves straight into the allocated iterator
        let args = unsafe { borrow.args.dup() };
        let iter = Iter::new(args, borrow.skip.clone(), borrow.pos, borrow.int);
        embryo.finish(strand, iter, out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        // The borrow holds the rest, and so its immutable pack, across the sink
        let borrow = this.borrow(strand)?;
        let mut pos = borrow.pos;
        with_slots!(strand, |mut slot| {
            while let Some((index, key, value)) = borrow.next_item(pos) {
                strand.store(&mut slot, value);
                if let Some(key) = key {
                    let key = unsafe { Sym::from_obj(key) };
                    sink.symbol(strand, key, Slot::reborrow(&mut slot))?;
                } else {
                    sink.positional(strand, Slot::reborrow(&mut slot))?;
                }
                pos = index + 1;
            }
            Ok(())
        })
    }
}

// ── Pack protocol ─────────────────────────────────────────────────

impl<'v> Protocol<'v> for PackObj<'v> {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Pack";

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().pack)
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
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().pack) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.get();
        let right = other.items();
        let flag = eq_items(strand, &left.items, right)?;
        strand.store(&mut out, flag);
        Ok(())
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().pack) else {
            return Err(Error::unsupported(strand, "`<`"));
        };
        let left = this.get();
        let right = other.items();
        let flag = lt_items(strand, &left.items, right)?;
        strand.store(&mut out, flag);
        Ok(())
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
                strand.store(out, value);
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
                strand.store(out, len);
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
                unpack!(strand, args => key, ?:default, ?else_key: or_else);
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let borrow = this.get();
                if let Some(value) = find(strand, &borrow.items, &key) {
                    strand.store(&mut out, value);
                    Ok(())
                } else if let Some(mut default) = default {
                    strand.store(&mut out, &mut default);
                    Ok(())
                } else if let Some(or_else) = or_else {
                    call!(strand, or_else, out)
                } else {
                    strand.store(&mut out, Value::NIL);
                    Ok(())
                }
            }
            sym::WITH => {
                unpack!(strand, args => **updates);
                // The updates stay rooted in their argument slots and the items in this
                // pack, so the result is gathered as references and copied only once
                // the new pack is allocated, after the last interrupt check.
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
                    replacements[index].1 = Some(value.into_ref());
                }
                let borrow = this.get();
                let mut items = Vec::with_capacity(borrow.items.len());
                for (i, (key, value)) in borrow.items.iter().enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    if let Some(index) = key.as_ref().and_then(|key| indices.get(&key.tag())) {
                        if let Some(value) = replacements[*index].1.take() {
                            items.push((key.as_ref().map(ItemKey::Held), value));
                        }
                    } else {
                        items.push((key.as_ref().map(ItemKey::Held), value));
                    }
                }
                for (i, (key, value)) in replacements.into_iter().enumerate() {
                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    if let Some(value) = value {
                        items.push((Some(ItemKey::New(key)), value));
                    }
                }
                let embryo = strand.builtin_types().pack.embryo(strand);
                let items = copy_items(strand.vm(), items);
                embryo.finish_imm(strand, Pack::new(items), out);
                Ok(())
            }
            sym::WITHOUT => {
                unpack!(strand, args => *keys);
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
                        items.push((key.as_ref().map(ItemKey::Held), value));
                    }
                }
                let embryo = strand.builtin_types().pack.embryo(strand);
                let items = copy_items(strand.vm(), items);
                embryo.finish_imm(strand, Pack::new(items), out);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "pack.len is a field, not a method",
            )),
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
        let embryo = strand.builtin_types().pack_iter.embryo(strand);
        // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
        embryo.finish(
            strand,
            Iter::new(unsafe { this.to_strong() }, HashSet::new(), 0, 0),
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
        unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut args| {
            strand.store(&mut args, &this);
            let args = args
                .downcast_ref(strand.builtin_types().pack)
                .expect("pack");
            self::unpack(strand, args, unpack)
        })
    }
}

// ── Pack Class ────────────────────────────────────────────────────

/// Collects a source's pairs for `Pack src`, into an argument accumulator rooted in
/// a slot. A `Pack` holds only positional items and symbol keys.
struct PackPairs<'b, 'v> {
    pack: &'b Value<'v>,
}

impl<'v> PackPairs<'_, 'v> {
    /// Push the value in `value`, under `key` if it has one, onto the accumulator.
    ///
    /// The accumulator is borrowed first, so the item is made only once nothing is left
    /// that could fail or allocate.
    fn push<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Option<Sym<'v, '_>>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let pack = self
            .pack
            .downcast_ref(strand.builtin_types().arg_acc)
            .expect("source accumulator");
        with_pin!(strand, pack, |pack| {
            let mut pack = pack
                .borrow_mut()
                .ok_or_else(|| Error::concurrency(strand))?;
            // SAFETY: the symbol and the value move straight into the borrowed
            // accumulator
            let key = key.map(|key| unsafe { strand.sym_obj(key) });
            pack.push((key, unsafe { value.take(strand) }));
            Ok(())
        })
    }
}

impl<'v, 's> Spread<'v, 's> for PackPairs<'_, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.push(strand, None, value)
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.push(strand, Some(key), value)
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
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Pack";
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
                ..Value,
                Method(sym::INIT_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::LT_METHOD),
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
                unpack!(strand, args => self_val, items);
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().pack, items, &mut native)?;
                    self_val.op_fill(
                        strand,
                        &strand.singletons().pack,
                        Slot::reborrow(&mut native),
                    )?;
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
        unpack!(strand, args => source);
        with_slots!(strand, |mut acc| {
            source_items(strand, &source, Slot::reborrow(&mut acc)).await?;
            let embryo = strand.builtin_types().pack.embryo(strand);
            embryo.finish_imm(strand, Pack::new(take_source_items(strand, &acc)), out);
            Ok(())
        })
    }
}

/// Collects the items of `source` for `Pack src` into a fresh accumulator in `acc`.
///
/// Spreading can run arbitrary code, so the items collect in an object the GC can see.
/// Take them with [`take_source_items`].
pub(crate) async fn source_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    source: &Value<'v>,
    mut acc: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand.builtin_types().arg_acc.create(
        strand,
        ArgAcc::new(Vec::new()),
        Slot::reborrow(&mut acc),
    );
    let mut sink = PackPairs { pack: &acc };
    let _op = strand.start_op();
    source
        .op_spread(strand, SpreadContext::Pairs, &mut sink)
        .await
}

/// The items [`source_items`] collected into `acc`, taken.
///
/// The items are unrooted, so take them between allocating the target's embryo and
/// finishing it.
pub(crate) fn take_source_items<'v>(strand: &Strand<'v, '_>, acc: &Value<'v>) -> Vec<ArgItem<'v>> {
    let acc = acc
        .downcast_ref(strand.builtin_types().arg_acc)
        .expect("source accumulator");
    with_pin!(strand, acc, |acc| {
        acc.borrow_mut().expect("private accumulator").take()
    })
}
