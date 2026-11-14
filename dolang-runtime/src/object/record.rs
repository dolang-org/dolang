//! `Record`: a mutable sequence of positional and symbol-keyed items, the
//! mutable counterpart of [`Pack`](super::pack::Pack).
//!
//! Every item has a slot in an order index, which iteration, spreading,
//! unpacking and equality walk. Positional items are a separate lane: `pos`
//! holds their slots, so the k-th of them is positional item k. Each key maps
//! to the slots of its instances. Removing an item empties its slot, and the
//! index is compacted once empty slots outnumber live ones.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash},
    mem,
    ops::ControlFlow,
};

use crate::{
    arg::{Arg, Args},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::members,
    sig,
    strand::Strand,
    sym::{self, Sym, Tag},
    unpack,
    value::{Output, Slot, Slots, TypeObject, Value, fmt::Format},
    vm::Vm,
    with_slots,
};

use super::{
    BoundMethod, index, iter,
    pack::{self, ArgItem},
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        instance_mcall_fallback, is_special_mcall, type_mcall_fallback,
    },
    sym::SymObj,
    tuple,
};

/// The smallest order index worth compacting
const MIN_COMPACT: usize = 8;

// ── Record ──────────────────────────────────────────────────────────

pub(crate) struct Record<'v> {
    /// Every item in order; `None` is an emptied slot
    order: Vec<Option<ArgItem<'v>>>,
    /// The slots of the positional items, in order
    pos: Vec<usize>,
    /// The slots of each key's instances, in order. A key with no instances
    /// has no entry.
    keys: HashMap<Tag, Vec<usize>>,
    /// The number of keyed items
    keyed: usize,
    epoch: u64,
}

unsafe impl<'v> Collect for Record<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for (key, value) in self.order.iter().flatten() {
            if let Some(key) = key {
                key.accept(visit)?;
            }
            value.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.order = Vec::new();
        self.pos = Vec::new();
        self.keys = HashMap::new();
        self.keyed = 0;
    }
}

/// A record key: a position, as given, or a symbol
enum Key<'v> {
    Pos(i64),
    Sym(GcObj<'v, SymObj>),
}

impl<'v> Key<'v> {
    fn new<'s>(strand: &mut Strand<'v, 's>, key: &Value<'v>) -> Result<'v, 's, Self> {
        if let Some(sym) = key.as_sym(strand) {
            Ok(Self::Sym(strand.sym_obj(sym)))
        } else if key.is_int(strand) {
            // An integer too big for `i64` is no position
            Ok(Self::Pos(key.to_i64(strand).unwrap_or(i64::MAX)))
        } else {
            Err(Error::type_error(
                strand,
                "records only support symbol and integer keys",
            ))
        }
    }
}

/// An optional instance argument, as an index
fn instance<'v, 's>(
    strand: &mut Strand<'v, 's>,
    instance: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<i64>> {
    instance
        .map(|instance| instance.to_i64(strand).map_err(|_| Error::index(strand)))
        .transpose()
}

impl<'v> Record<'v> {
    pub(crate) fn new() -> Self {
        Self {
            order: Vec::new(),
            pos: Vec::new(),
            keys: HashMap::new(),
            keyed: 0,
            epoch: 0,
        }
    }

    pub(crate) fn from_items(items: Vec<ArgItem<'v>>) -> Self {
        let mut record = Self::new();
        for item in items {
            record.push_item(item);
        }
        record
    }

    /// The number of items
    pub(crate) fn len(&self) -> usize {
        self.pos.len() + self.keyed
    }

    /// Every slot of the order index, with `None` for an emptied one
    pub(crate) fn order(&self) -> &[Option<ArgItem<'v>>] {
        &self.order
    }

    /// The items, in order
    pub(crate) fn items(&self) -> impl Iterator<Item = &ArgItem<'v>> {
        self.order.iter().flatten()
    }

    fn snapshot(&self) -> Vec<ArgItem<'v>> {
        self.items()
            .map(|(key, value)| (key.clone(), value.dup()))
            .collect()
    }

    /// Appends an item.
    fn push_item(&mut self, item: ArgItem<'v>) {
        if self.order.len() >= MIN_COMPACT && self.order.len() >= 2 * self.len() {
            self.compact();
        }
        let slot = self.order.len();
        match &item.0 {
            None => self.pos.push(slot),
            Some(key) => {
                self.keys.entry(key.tag()).or_default().push(slot);
                self.keyed += 1;
            }
        }
        self.order.push(Some(item));
    }

    /// Drops the emptied slots, renumbering the rest.
    fn compact(&mut self) {
        self.order.retain(Option::is_some);
        self.pos.clear();
        for slots in self.keys.values_mut() {
            slots.clear();
        }
        for (slot, item) in self.order.iter().enumerate() {
            match &item.as_ref().expect("retained slot").0 {
                None => self.pos.push(slot),
                Some(key) => self.keys.get_mut(&key.tag()).expect("live key").push(slot),
            }
        }
    }

    fn value_mut(&mut self, slot: usize) -> &mut Value<'v> {
        &mut self.order[slot].as_mut().expect("live slot").1
    }

    /// The slot `key` selects: a position, or an instance of a key. Without an
    /// instance, a key selects its latest one. A position has only instance 0.
    fn slot(&self, key: &Key<'v>, instance: Option<i64>) -> Option<usize> {
        match key {
            Key::Pos(i) => {
                if instance.is_some_and(|instance| index::element(1, instance).is_none()) {
                    return None;
                }
                Some(self.pos[index::element(self.pos.len(), *i)?])
            }
            Key::Sym(key) => {
                let slots = self.keys.get(&key.tag())?;
                match instance {
                    None => slots.last().copied(),
                    Some(instance) => Some(slots[index::element(slots.len(), instance)?]),
                }
            }
        }
    }

    fn get(&self, key: &Key<'v>, instance: Option<i64>) -> Option<&Value<'v>> {
        let slot = self.slot(key, instance)?;
        self.order[slot].as_ref().map(|(_, value)| value)
    }

    /// Overwrites a position, or replaces every instance of a key, appending
    /// it if absent. False if the position is out of range.
    fn assign(&mut self, key: Key<'v>, value: Value<'v>) -> bool {
        match key {
            Key::Pos(i) => {
                let Some(slot) = self.slot(&Key::Pos(i), None) else {
                    return false;
                };
                *self.value_mut(slot) = value;
            }
            Key::Sym(key) => match self.keys.get_mut(&key.tag()) {
                Some(slots) => {
                    for &slot in &slots[1..] {
                        self.order[slot] = None;
                    }
                    self.keyed -= slots.len() - 1;
                    slots.truncate(1);
                    let slot = slots[0];
                    *self.value_mut(slot) = value;
                }
                None => self.push_item((Some(key), value)),
            },
        }
        self.epoch += 1;
        true
    }

    /// Appends an instance of a key, or inserts a positional item before
    /// position `i`, shifting the later ones. False if the position is out of
    /// range.
    fn insert(&mut self, key: Key<'v>, value: Value<'v>) -> bool {
        match key {
            Key::Sym(key) => self.push_item((Some(key), value)),
            Key::Pos(i) => {
                let Some(at) = index::position(self.pos.len(), i) else {
                    return false;
                };
                // The new item takes the last slot, then moves down to `at`,
                // each item it passes moving to the next position's slot
                self.push_item((None, value));
                for j in (at + 1..self.pos.len()).rev() {
                    let (lo, hi) = (self.pos[j - 1], self.pos[j]);
                    let (before, after) = self.order.split_at_mut(hi);
                    mem::swap(
                        &mut before[lo].as_mut().expect("live slot").1,
                        &mut after[0].as_mut().expect("live slot").1,
                    );
                }
            }
        }
        self.epoch += 1;
        true
    }

    /// Appends positional items.
    fn push(&mut self, values: impl IntoIterator<Item = Value<'v>>) {
        for value in values {
            self.push_item((None, value));
        }
        self.epoch += 1;
    }

    /// Removes a positional item, shifting the later ones, or an instance of a
    /// key, by default its first.
    fn remove(&mut self, key: &Key<'v>, instance: Option<i64>) -> Option<Value<'v>> {
        let slot = match key {
            Key::Pos(i) => {
                if instance.is_some_and(|instance| index::element(1, instance).is_none()) {
                    return None;
                }
                let at = index::element(self.pos.len(), *i)?;
                self.pos.remove(at)
            }
            Key::Sym(key) => {
                let slots = self.keys.get_mut(&key.tag())?;
                let at = index::element(slots.len(), instance.unwrap_or(0))?;
                let slot = slots.remove(at);
                if slots.is_empty() {
                    self.keys.remove(&key.tag());
                }
                self.keyed -= 1;
                slot
            }
        };
        self.epoch += 1;
        self.order[slot].take().map(|(_, value)| value)
    }

    /// Removes a positional item, or every instance of a key. False if there
    /// was none.
    fn delete(&mut self, key: &Key<'v>) -> bool {
        match key {
            Key::Pos(_) => self.remove(key, None).is_some(),
            Key::Sym(key) => {
                let Some(slots) = self.keys.remove(&key.tag()) else {
                    return false;
                };
                self.keyed -= slots.len();
                for slot in slots {
                    self.order[slot] = None;
                }
                self.epoch += 1;
                true
            }
        }
    }

    fn clear_items(&mut self) {
        self.order.clear();
        self.pos.clear();
        self.keys.clear();
        self.keyed = 0;
        self.epoch += 1;
    }

    /// The number of distinct keys, counting each position as one, or the
    /// number of instances of `key`
    fn count(&self, key: Option<&Key<'v>>) -> usize {
        match key {
            None => self.pos.len() + self.keys.len(),
            Some(key @ Key::Pos(_)) => usize::from(self.slot(key, None).is_some()),
            Some(Key::Sym(key)) => self.keys.get(&key.tag()).map_or(0, Vec::len),
        }
    }
}

/// A `Pack` holding the items of `this` as they are now, for unpacking.
fn snapshot<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: &Recv<'v, '_, Record<'v>>,
) -> Result<'v, 's, GcObj<'v, pack::PackObj<'v>>> {
    let items = this.borrow(strand)?.snapshot();
    Ok(GcObj::new_imm(
        strand.vm().arena(),
        strand.builtin_types().pack,
        pack::Pack::new(items),
    ))
}

/// Settles a lookup that found nothing: the default, the `else:` callback's
/// result, or `missing`.
async fn fallback<'v, 'a, 's>(
    strand: &'a mut Strand<'v, 's>,
    default: Option<Slot<'v, 'a>>,
    or_else: Option<Slot<'v, 'a>>,
    mut out: Slot<'v, 'a>,
    missing: impl FnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, ()>,
) -> Result<'v, 's, ()> {
    if let Some(mut default) = default {
        out.store(default.take());
        Ok(())
    } else if let Some(or_else) = or_else {
        call!(strand, or_else, out)
    } else {
        missing(strand)?;
        out.store(Value::NIL);
        Ok(())
    }
}

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
        // The borrow keeps the snapshot's values alive
        let borrow = this.borrow(strand)?;
        let items = borrow.snapshot();
        // With a positional item, the brace literal makes a record; otherwise,
        // it would make a dict
        let braces = !borrow.pos.is_empty();
        if !braces {
            crate::fmt!(strand, w, "Record")?;
        }
        pack::debug_items(strand, &items, braces, w)
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        let Ok(borrow) = this.borrow(strand) else {
            return Ok(true);
        };
        Ok(borrow.len() != 0)
    }

    /// Hashes the items in order, as equality compares them
    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        sym::RECORD.hash(hasher);
        for (index, (key, value)) in borrow.items().enumerate() {
            if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            key.as_ref().map(|key| key.tag()).hash(hasher);
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
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            if left.len() != right.len() {
                return Ok(Value::FALSE);
            }
            for (index, ((lkey, lvalue), (rkey, rvalue))) in
                left.items().zip(right.items()).enumerate()
            {
                if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                if lkey.as_ref().map(|key| key.tag()) != rkey.as_ref().map(|key| key.tag())
                    || !lvalue.op_eq(strand, rvalue)?.to_bool(strand)?
                {
                    return Ok(Value::FALSE);
                }
            }
            Ok(Value::TRUE)
        })
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key = Key::new(strand, index)?;
        let borrow = this.borrow(strand)?;
        match borrow.get(&key, None) {
            Some(value) => {
                Output::set(strand, out, value);
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let key = Key::new(strand, &key)?;
        if this.borrow_mut(strand)?.assign(key, value.take()) {
            Ok(())
        } else {
            Err(Error::index(strand))
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
                let len = this.borrow(strand)?.len();
                let len = i64::try_from(len).map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, len);
                Ok(())
            }
            sym::CLEAR
            | sym::INSERT
            | sym::PUSH
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::PAIRS
            | sym::KEYS
            | sym::VALUES
            | sym::COUNT
            | sym::COPY
            | sym::CONTAINS => {
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
        let default_key = Sym::well_known(sym::DEFAULT);
        let else_key = Sym::well_known(sym::ELSE);
        match method.tag() {
            sym::CLEAR => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                this.borrow_mut(strand)?.clear_items();
                Ok(())
            }
            sym::INSERT => {
                let ([key, mut value], []) = unpack!(strand, args, 2, 0)?;
                let key = Key::new(strand, &key)?;
                if this.borrow_mut(strand)?.insert(key, value.take()) {
                    Ok(())
                } else {
                    Err(Error::index(strand))
                }
            }
            sym::PUSH => {
                let mut values = Vec::new();
                for arg in args {
                    match arg {
                        Arg::Pos(mut value) => values.push(value.take()),
                        Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
                    }
                }
                // The argument slots no longer root the values, so nothing
                // may collect before they are stored
                this.borrow_mut(strand)?.push(values);
                Ok(())
            }
            sym::GET => {
                let ([key], [instance_arg, default, or_else]) =
                    unpack!(strand, args, 1, 1, default_key = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let key = Key::new(strand, &key)?;
                let instance = instance(strand, instance_arg)?;
                let found = this.borrow(strand)?.get(&key, instance).map(Value::dup);
                match found {
                    Some(value) => {
                        out.store(value);
                        Ok(())
                    }
                    None => fallback(strand, default, or_else, out, |_| Ok(())).await,
                }
            }
            sym::POP => {
                let ([key], [instance_arg, default, or_else]) =
                    unpack!(strand, args, 1, 1, default_key = None, else_key = None)?;
                if default.is_some() && or_else.is_some() {
                    return Err(Error::unexpected_key(strand, else_key));
                }
                let key = Key::new(strand, &key)?;
                let instance = instance(strand, instance_arg)?;
                let removed = this.borrow_mut(strand)?.remove(&key, instance);
                match removed {
                    Some(value) => {
                        out.store(value);
                        Ok(())
                    }
                    None => {
                        fallback(strand, default, or_else, out, |strand| {
                            Err(Error::index(strand))
                        })
                        .await
                    }
                }
            }
            sym::DELETE => {
                let ([key], []) = unpack!(strand, args, 1, 0)?;
                let key = Key::new(strand, &key)?;
                let deleted = this.borrow_mut(strand)?.delete(&key);
                Output::set(strand, out, deleted);
                Ok(())
            }
            sym::PAIRS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Iter::create(strand, &this, Mode::Pairs, out)
            }
            sym::KEYS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Iter::create(strand, &this, Mode::Keys, out)
            }
            sym::VALUES => {
                let ([], [key]) = unpack!(strand, args, 0, 1)?;
                let mode = match key.map(|key| Key::new(strand, &key)).transpose()? {
                    None => Mode::Values,
                    Some(Key::Sym(key)) => Mode::KeyValues(key.tag()),
                    Some(key @ Key::Pos(_)) => {
                        // A position has at most one value
                        let found = this.borrow(strand)?.get(&key, None).map(Value::dup);
                        return with_slots!(strand, |mut values| {
                            tuple::create(strand, found, &mut values);
                            values.op_iter(strand, out).await
                        });
                    }
                };
                Iter::create(strand, &this, mode, out)
            }
            sym::COUNT => {
                let ([], [key]) = unpack!(strand, args, 0, 1)?;
                let key = key.map(|key| Key::new(strand, &key)).transpose()?;
                let count = this.borrow(strand)?.count(key.as_ref());
                let count = i64::try_from(count).map_err(|_| Error::overflow(strand))?;
                Output::set(strand, out, count);
                Ok(())
            }
            sym::CONTAINS => {
                let ([key], [expected]) = unpack!(strand, args, 1, 1)?;
                let key = Key::new(strand, &key)?;
                let borrow = this.borrow(strand)?;
                let found = match (&key, expected) {
                    (_, None) => borrow.slot(&key, None).is_some(),
                    (Key::Pos(_), Some(expected)) => match borrow.get(&key, None) {
                        Some(value) => value.op_eq(strand, &expected)?.to_bool(strand)?,
                        None => false,
                    },
                    (Key::Sym(sym), Some(expected)) => {
                        let mut found = false;
                        for (i, &slot) in borrow
                            .keys
                            .get(&sym.tag())
                            .map_or(&[][..], Vec::as_slice)
                            .iter()
                            .enumerate()
                        {
                            if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                                strand.check_trap()?;
                            }
                            let value = &borrow.order[slot].as_ref().expect("live slot").1;
                            if value.op_eq(strand, &expected)?.to_bool(strand)? {
                                found = true;
                                break;
                            }
                        }
                        found
                    }
                };
                Output::set(strand, out, found);
                Ok(())
            }
            sym::COPY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if this.delegator().is_some() {
                    return with_slots!(strand, |mut receiver, mut ty| {
                        Output::set(strand, Slot::reborrow(&mut receiver), &this);
                        receiver.op_type(strand, Slot::reborrow(&mut ty));
                        call!(strand, &ty, &receiver, out)
                    });
                }
                let copy = Record::from_items(this.borrow(strand)?.snapshot());
                strand.builtin_types().record.create(strand, copy, out);
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "record.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
            }
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Iter::create(strand, &this, Mode::Pairs, out)
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        with_slots!(strand, |mut slot| {
            for (key, value) in borrow.items() {
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

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let snapshot = snapshot(strand, &this)?;
        pack::unpack(strand, snapshot, sig, out)
    }
}

// ── Iter ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Mode {
    /// `(key, value)` pairs, a position's key being its index
    Pairs,
    /// Each position, and each key once
    Keys,
    Values,
    /// The values of one key
    KeyValues(Tag),
}

/// An iterator over a record's items, in order. It fails once the record
/// changes.
pub(crate) struct Iter<'v> {
    record: GcObj<'v, Record<'v>>,
    epoch: u64,
    mode: Mode,
    /// The next slot
    slot: Cell<usize>,
    /// The positional items passed
    int: Cell<i64>,
    /// The keys yielded, for [`Mode::Keys`]
    seen: RefCell<HashSet<Tag>>,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.record.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Iter<'v> {
    fn create<'s>(
        strand: &mut Strand<'v, 's>,
        this: &Recv<'v, '_, Record<'v>>,
        mode: Mode,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let epoch = this.borrow(strand)?.epoch;
        let iter = Iter {
            record: this.to_strong(),
            epoch,
            mode,
            slot: Cell::new(0),
            int: Cell::new(0),
            seen: RefCell::new(HashSet::new()),
        };
        strand.builtin_types().record_iter.create(strand, iter, out);
        Ok(())
    }
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
        strand: &'a mut Strand<'v, 's>,
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
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _sig: &'a sig::Unpack<'v, 'a>,
        _out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::unsupported(strand, "unpacking"))
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let iter = this.borrow(strand)?;
        with_pin!(strand, iter.record, |record| {
            let record = record.borrow().ok_or_else(|| Error::concurrency(strand))?;
            if record.epoch != iter.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            while let Some(item) = record.order.get(iter.slot.get()) {
                iter.slot.update(|slot| slot + 1);
                let Some((key, value)) = item else {
                    continue;
                };
                let key = match key {
                    Some(key) => {
                        match iter.mode {
                            Mode::Keys if !iter.seen.borrow_mut().insert(key.tag()) => continue,
                            Mode::KeyValues(tag) if key.tag() != tag => continue,
                            _ => {}
                        }
                        Value::from_object(key.clone())
                    }
                    None => {
                        let int = iter.int.get();
                        iter.int.set(int + 1);
                        if let Mode::KeyValues(_) = iter.mode {
                            continue;
                        }
                        Value::from_i64(strand, int)
                    }
                };
                out.store(match iter.mode {
                    Mode::Pairs => {
                        Value::from_object(tuple::tuple(strand.vm(), [key, value.dup()]))
                    }
                    Mode::Keys => key,
                    Mode::Values | Mode::KeyValues(_) => value.dup(),
                });
                return Ok(true);
            }
            Ok(false)
        })
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

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        iter::spread_iter(strand, this, sink).await
    }
}

// ── Record Class ────────────────────────────────────────────────────

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
        crate::fmt!(strand, w, "<type std.Record>")
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
                Method(sym::HASH_METHOD),
                Getter(sym::LEN),
                Method(sym::CLEAR),
                Method(sym::INSERT),
                Method(sym::PUSH),
                Method(sym::GET),
                Method(sym::POP),
                Method(sym::DELETE),
                Method(sym::PAIRS),
                Method(sym::KEYS),
                Method(sym::VALUES),
                Method(sym::COUNT),
                Method(sym::COPY),
                Method(sym::CONTAINS),
                Method(sym::INDEX_METHOD),
                Method(sym::ASSIGN_METHOD),
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
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().record, items, &mut native)?;
                    self_val.op_fill(strand, &strand.singletons().record, native.take())?;
                    Ok(())
                })
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
            | sym::CLEAR
            | sym::INSERT
            | sym::PUSH
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::PAIRS
            | sym::KEYS
            | sym::VALUES
            | sym::COUNT
            | sym::COPY
            | sym::CONTAINS
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::HASH_METHOD
            | sym::INDEX_METHOD
            | sym::ASSIGN_METHOD
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([source], []) = unpack!(strand, args, 1, 0)?;
        let items = pack::source_items(strand, &source).await?;
        strand
            .builtin_types()
            .record
            .create(strand, Record::from_items(items), out);
        Ok(())
    }
}
