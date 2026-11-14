use std::{
    cell::{Cell, RefCell},
    hash::{Hash, Hasher},
    ops::ControlFlow,
};

use bitvec::{bitbox, boxed::BitBox};
use dolang_util::hashbrown::raw::{Bucket, RawTable};

use crate::{
    arg::{Arg, Args},
    bytecode::{Rest, Variadic},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::members,
    sig::{self, UnpackKeyKind},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        Input, Output, Slot, Slots, TypeObject, Value,
        fmt::{Format, Spec},
    },
    vm::Vm,
    with_slots,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_get_fallback,
        instance_mcall_fallback, is_special_mcall, type_mcall_fallback,
    },
    tuple,
};

// ── Entries ─────────────────────────────────────────────────────────

pub(crate) enum EntryValue<'v> {
    Single { value: Value<'v>, index: usize },
    Multi(Vec<(Value<'v>, usize)>),
}

impl<'v> EntryValue<'v> {
    fn len(&self) -> usize {
        match self {
            EntryValue::Single { .. } => 1,
            EntryValue::Multi(items) => items.len(),
        }
    }

    fn get(&self, index: Option<usize>) -> Option<&Value<'v>> {
        match self {
            EntryValue::Single { value, .. } => {
                if index.unwrap_or(0) == 0 {
                    Some(value)
                } else {
                    None
                }
            }
            EntryValue::Multi(items) => items
                .get(index.unwrap_or(items.len().saturating_sub(1)))
                .map(|(v, _)| v),
        }
    }

    fn at(&self, index: usize) -> &Value<'v> {
        self.get(Some(index)).unwrap()
    }

    /// The value a lookup without an instance resolves to.
    ///
    /// The most recently inserted one, so a duplicate key overrides the values
    /// before it: last wins.
    fn latest(&self) -> &Value<'v> {
        self.get(None).expect("entry with no values")
    }
}

pub(crate) struct Entry<'v> {
    pub(crate) key: Value<'v>,
    pub(crate) value: EntryValue<'v>,
    hash: u64,
}

// ── Dict ────────────────────────────────────────────────────────────

pub(crate) struct Dict<'v> {
    table: RawTable<Entry<'v>>,
    pub(crate) index: Vec<Option<(Bucket<Entry<'v>>, usize)>>,
    epoch: u64,
    pub(crate) total_pairs: usize,
}

unsafe impl<'v> Collect for Dict<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        unsafe {
            for bucket in self.table.iter() {
                bucket.as_ref().key.accept(visit)?;
                match &bucket.as_ref().value {
                    EntryValue::Single { value, .. } => value.accept(visit)?,
                    EntryValue::Multi(values) => {
                        for (value, _) in values.iter() {
                            value.accept(visit)?
                        }
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.table.clear();
        self.total_pairs = 0;
    }
}

impl<'v> Dict<'v> {
    pub(crate) fn new() -> Self {
        Self {
            table: Default::default(),
            index: Default::default(),
            epoch: 0,
            total_pairs: 0,
        }
    }

    pub(crate) fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: &Value<'v>,
        instance: Option<i64>,
    ) -> Result<'v, 's, Option<&Value<'v>>> {
        let mut hasher = strand.vm().hasher();
        index.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        Ok(self.find(strand, hash, index)?.and_then(|pair| unsafe {
            let pair = pair.as_ref();
            let instance = match instance {
                Some(instance) => Some(index::element(pair.value.len(), instance)?),
                None => None,
            };
            pair.value.get(instance)
        }))
    }

    /// Finds the key of the first pair, in insertion order, that an unpack
    /// left behind: one with more instances than `skip` covers. `skip` is
    /// copied rather than advanced, since the caller still needs it as it
    /// stands (to commit, or to hand to a capture).
    fn leftover_key<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        skip: &Skip<'v>,
    ) -> Result<'v, 's, Option<Value<'v>>> {
        let mut skip = skip.clone();
        for (bucket, _) in self.index.iter().flatten() {
            let bucket = unsafe { bucket.as_ref() };
            if skip.add(strand, &bucket.key, bucket.hash)? >= bucket.value.len() {
                continue;
            }
            return Ok(Some(bucket.key.dup()));
        }
        Ok(None)
    }

    fn next_index<'a>(
        index: &'a mut Vec<Option<(Bucket<Entry<'v>>, usize)>>,
        capacity: usize,
    ) -> (usize, &'a mut Option<(Bucket<Entry<'v>>, usize)>) {
        let len = index.len();
        let i = if len >= 2 * capacity {
            let mut i = 0usize;
            index.retain(|e| {
                if let Some((bucket, subindex)) = e {
                    let bucket = unsafe { bucket.as_mut() };
                    match &mut bucket.value {
                        EntryValue::Single { index, .. } => {
                            assert_eq!(*subindex, 0);
                            *index = i;
                        }
                        EntryValue::Multi(items) => {
                            items[*subindex].1 = i;
                        }
                    }
                    i += 1;
                    true
                } else {
                    false
                }
            });
            i
        } else {
            index.len()
        };
        index.push(None);
        (i, unsafe { index.get_unchecked_mut(i) })
    }

    fn rehash(&mut self, old_capacity: usize) -> usize {
        let capacity = 1.max(old_capacity * 2);
        let mut table = RawTable::with_capacity(capacity);
        let mut remap = vec![None; self.table.buckets()];
        let mut index = Vec::with_capacity(self.total_pairs);
        for (old_bucket, subindex) in self.index.drain(..).flatten() {
            // The old allocation remains live until all bucket pointers have been remapped.
            let old_index = unsafe { self.table.bucket_index(&old_bucket) };
            let bucket = match &remap[old_index] {
                Some(bucket) => Bucket::clone(bucket),
                None => {
                    let (entry, _) = unsafe { self.table.remove(old_bucket) };
                    let bucket = unsafe { table.insert_no_grow(entry.hash, entry) };
                    remap[old_index] = Some(bucket.clone());
                    bucket
                }
            };
            let i = index.len();
            match unsafe { &mut bucket.as_mut().value } {
                EntryValue::Single { index, .. } => *index = i,
                EntryValue::Multi(items) => items[subindex].1 = i,
            }
            index.push(Some((bucket, subindex)));
        }
        self.table = table;
        self.index = index;
        self.epoch += 1;
        capacity
    }

    fn find<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        hash: u64,
        key: &Value<'v>,
    ) -> Result<'v, 's, Option<Bucket<Entry<'v>>>> {
        super::table::find(&self.table, hash, |entry| key.eq(strand, &entry.key))
    }

    pub(crate) fn insert<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Value<'v>,
        value: Value<'v>,
        hv: u64,
        unique: bool,
    ) -> Result<'v, 's, ()> {
        unsafe {
            let found = self.find(strand, hv, &key)?;
            let mut cap = self.table.capacity();
            let probe = match found {
                Some(bucket) => Ok(bucket),
                None => {
                    if self.table.len() == cap {
                        cap = self.rehash(cap);
                    }
                    self.table
                        .find_or_find_insert_index(hv, |_| false, hasher())
                }
            };
            match probe {
                Ok(bucket) => match &mut bucket.as_mut().value {
                    EntryValue::Single {
                        value: existing,
                        index,
                    } => {
                        if unique {
                            *existing = value
                        } else {
                            let (i, slot) = Self::next_index(&mut self.index, cap);
                            bucket.as_mut().value =
                                EntryValue::Multi(vec![(existing.take(), *index), (value, i)]);
                            *slot = Some((bucket, 1));
                            self.total_pairs += 1;
                        }
                    }
                    EntryValue::Multi(items) => {
                        if unique {
                            let (_, index) = items.remove(0);
                            for (_, index) in items.iter() {
                                *self.index.get_unchecked_mut(*index) = None;
                            }
                            self.total_pairs -= items.len();
                            bucket.as_mut().value = EntryValue::Single { value, index };
                        } else {
                            let (i, slot) = Self::next_index(&mut self.index, cap);
                            let subindex = items.len();
                            items.push((value, i));
                            *slot = Some((bucket, subindex));
                            self.total_pairs += 1;
                        }
                    }
                },
                Err(index) => {
                    let (i, slot) = Self::next_index(&mut self.index, cap);
                    *slot = Some((
                        self.table.insert_at_index(
                            hv,
                            index,
                            Entry {
                                key,
                                value: EntryValue::Single { value, index: i },
                                hash: hv,
                            },
                        ),
                        0,
                    ));
                    self.total_pairs += 1;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn delete<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        let Some(bucket) = self.find(strand, hash, key)? else {
            return Ok(false);
        };
        unsafe {
            self.total_pairs -= bucket.as_ref().value.len();
            match &bucket.as_ref().value {
                EntryValue::Single { index, .. } => {
                    *self.index.get_unchecked_mut(*index) = None;
                }
                EntryValue::Multi(items) => {
                    for (_, index) in items.iter() {
                        *self.index.get_unchecked_mut(*index) = None;
                    }
                }
            }
            self.table.erase(bucket);
        }
        self.epoch += 1;
        Ok(true)
    }

    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut sink = DictPairs { dict: &mut this };

        for (index, arg) in args.enumerate() {
            if (index + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?;
            }
            match arg {
                Arg::Pos(value) => sink.positional(strand, value)?,
                Arg::Key(key, value) => sink.symbol(strand, key, value)?,
            }
        }

        Ok(this)
    }

    /// Builds a dict from arguments, keying the positional ones by their
    /// index among the positional arguments and the key ones by symbol.
    pub(crate) fn from_indexed_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut position = 0i64;
        with_slots!(strand, |mut key| {
            for (index, arg) in args.enumerate() {
                if (index + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap_gc()?;
                }
                let mut value = match arg {
                    Arg::Pos(value) => {
                        Output::set(strand, Slot::reborrow(&mut key), position);
                        position += 1;
                        value
                    }
                    Arg::Key(sym, value) => {
                        Output::set(strand, Slot::reborrow(&mut key), sym);
                        value
                    }
                };
                let mut hasher = strand.vm().hasher();
                key.op_hash(strand, &mut hasher)?;
                this.insert(strand, key.take(), value.take(), hasher.finish(), false)?;
            }
            Ok(this)
        })
    }

    pub(crate) async fn from_builtin_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut counter = 1;

        let mut sink = DictPairs { dict: &mut this };

        loop {
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?
            }
            counter += 1;
            let mut key = match args.next() {
                Some(Arg::Pos(key)) => key,
                Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                    expand
                        .op_spread(strand, SpreadContext::Pairs, &mut sink)
                        .await?;
                    continue;
                }
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => break,
            };
            let mut value = match args.next() {
                Some(Arg::Pos(value)) => value,
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => return Err(Error::missing_positional(strand, counter)),
            };
            let mut hasher = strand.vm().hasher();
            key.op_hash(strand, &mut hasher)?;
            let hv = hasher.finish();
            sink.dict
                .insert(strand, key.take(), value.take(), hv, false)?
        }

        Ok(this)
    }
}

// ── Hashing ─────────────────────────────────────────────────────────

fn hasher<'v>() -> impl Fn(&Entry<'v>) -> u64 {
    |Entry { hash, .. }| *hash
}

/// Collects spread pairs into a dict, which holds only keyed entries
struct DictPairs<'b, 'v> {
    dict: &'b mut Dict<'v>,
}

impl<'b, 'v, 's> Spread<'v, 's> for DictPairs<'b, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        _value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Err(Error::unexpected_positional(strand, 0))
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut key_slot| {
            Output::set(strand, Slot::reborrow(&mut key_slot), key);
            let mut hasher = strand.vm().hasher();
            key_slot.op_hash(strand, &mut hasher).unwrap();
            let hv = hasher.finish();
            self.dict
                .insert(strand, key_slot.take(), value.take(), hv, false)
        })
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut key: Slot<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        self.dict
            .insert(strand, key.take(), value.take(), hv, false)?;
        Ok(())
    }
}

// ── Iter ────────────────────────────────────────────────────────────

pub(crate) struct Iter<'v> {
    index: Cell<usize>,
    epoch: u64,
    dict: GcObj<'v, Dict<'v>>,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.dict.accept(visit)
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
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<dict iterator>")
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        Dict::iter_op_next(&borrow.index, borrow.epoch, &borrow.dict, strand, out)
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
        super::iter::spread_iter(strand, this, sink).await
    }
}

// ── Keys, values ────────────────────────────────────────────────────

pub(crate) struct Values<'v> {
    index: Cell<usize>,
    epoch: u64,
    container: GcObj<'v, Dict<'v>>,
}

pub(crate) struct Keys<'v> {
    index: Cell<usize>,
    epoch: u64,
    container: GcObj<'v, Dict<'v>>,
    visited: RefCell<BitBox>,
}

pub(crate) struct KeyValues<'v> {
    index: Cell<usize>,
    epoch: u64,
    container: GcObj<'v, Dict<'v>>,
    bucket: Option<Bucket<Entry<'v>>>,
}

unsafe impl<'v> Collect for Values<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

unsafe impl<'v> Collect for Keys<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

unsafe impl<'v> Collect for KeyValues<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Dict<'v> {
    /// Spreads one pair, always keyed: a symbol key as a symbol, any other
    /// key as is
    fn spread_key_value<'s>(
        strand: &mut Strand<'v, 's>,
        key: impl Input<'v>,
        value: impl Input<'v>,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut key_slot, mut value_slot| {
            Output::set(strand, &mut key_slot, key);
            Output::set(strand, &mut value_slot, value);
            if let Some(sym) = key_slot.as_sym(strand) {
                sink.symbol(strand, sym, value_slot)
            } else {
                sink.keyed(strand, key_slot, value_slot)
            }
        })
    }

    fn with_iter_inner<'s, R>(
        container: &GcObj<'v, Dict<'v>>,
        epoch: u64,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&Dict<'v>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        with_pin!(strand, container, |container| {
            let dict = container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            f(&dict)
        })
    }

    fn next_value_from_index(dict: &Dict<'v>, index: usize) -> Option<(usize, Value<'v>)> {
        let mut index = index;
        loop {
            let bucket = dict.index.get(index)?;
            index += 1;
            if let Some((bucket, subindex)) = bucket {
                return Some((index, unsafe { bucket.as_ref().value.at(*subindex).dup() }));
            }
        }
    }

    fn next_value_from_bucket(
        bucket: Option<Bucket<Entry<'v>>>,
        index: usize,
    ) -> Option<(usize, Value<'v>)> {
        let bucket = bucket?;
        let value = unsafe { bucket.as_ref().value.get(Some(index))?.dup() };
        Some((index + 1, value))
    }

    fn next_key_from_index(
        dict: &Dict<'v>,
        index: usize,
        visited: &BitBox,
        pending: &mut Vec<usize>,
    ) -> Option<(usize, Value<'v>)> {
        let mut index = index;
        loop {
            let entry = dict.index.get(index)?;
            index += 1;
            let Some((bucket, _)) = entry else {
                continue;
            };
            let bucket_index = unsafe { dict.table.bucket_index(bucket) };
            if visited[bucket_index] || pending.contains(&bucket_index) {
                continue;
            }
            pending.push(bucket_index);
            return Some((index, unsafe { bucket.as_ref().key.dup() }));
        }
    }

    fn values_iter_op_next<'s>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Self::with_iter_inner(container, epoch, strand, |dict| {
            if let Some((next_index, value)) = Self::next_value_from_index(dict, index.get()) {
                index.set(next_index);
                out.store(value);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn key_values_iter_op_next<'s>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        bucket: Option<Bucket<Entry<'v>>>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Self::with_iter_inner(container, epoch, strand, |_inner| {
            if let Some((next_index, value)) = Self::next_value_from_bucket(bucket, index.get()) {
                index.set(next_index);
                out.store(value);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn keys_iter_op_next<'s>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        visited: &RefCell<BitBox>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Self::with_iter_inner(container, epoch, strand, |dict| {
            let mut visited = visited.borrow_mut();
            let mut pending = Vec::with_capacity(1);
            if let Some((next_index, key)) =
                Self::next_key_from_index(dict, index.get(), &visited, &mut pending)
            {
                for bucket_index in pending {
                    visited.set(bucket_index, true);
                }
                index.set(next_index);
                out.store(key);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

impl<'v> Protocol<'v> for Values<'v> {
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
        crate::fmt!(strand, w, "<values iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'s>(
        this: Recv<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
        sig: &sig::Unpack<'v, '_>,
        mut out: Slots<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let next_index = {
            with_pin!(strand, borrow.container, |container| {
                let dict = container
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                if dict.epoch != borrow.epoch {
                    return Err(Error::concurrency_msg(
                        strand,
                        "collection was modified during iteration",
                    ));
                }
                super::iter::unpack_cursor(
                    strand,
                    sig,
                    &mut out,
                    borrow.index.get(),
                    |_, i, mut out| {
                        Dict::next_value_from_index(&dict, i).map(|(next, value)| {
                            out.store(value);
                            next
                        })
                    },
                )
            })?
        };
        if let Some(i) = sig.pos_rest_slot() {
            let embryo = strand.builtin_types().dict_values.embryo(strand);
            // SAFETY: the reference moves straight into the allocated iterator
            embryo.finish(
                strand,
                Values {
                    index: Cell::new(next_index),
                    epoch: borrow.epoch,
                    container: unsafe { borrow.container.dup() },
                },
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
        super::iter::spread_iter(strand, this, sink).await
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        Dict::values_iter_op_next(&borrow.index, borrow.epoch, &borrow.container, strand, out)
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
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}

impl<'v> Protocol<'v> for KeyValues<'v> {
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
        crate::fmt!(strand, w, "<key values iterator>")
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
        let borrow = this.borrow(strand)?;
        let next_index = {
            with_pin!(strand, borrow.container, |container| {
                let dict = container
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                if dict.epoch != borrow.epoch {
                    return Err(Error::concurrency_msg(
                        strand,
                        "collection was modified during iteration",
                    ));
                }
                super::iter::unpack_cursor(
                    strand,
                    sig,
                    &mut out,
                    borrow.index.get(),
                    |_, i, mut out| {
                        Dict::next_value_from_bucket(borrow.bucket.clone(), i).map(
                            |(next, value)| {
                                out.store(value);
                                next
                            },
                        )
                    },
                )
            })?
        };
        if let Some(i) = sig.pos_rest_slot() {
            let embryo = strand.builtin_types().dict_key_values.embryo(strand);
            // SAFETY: the reference moves straight into the allocated iterator
            embryo.finish(
                strand,
                KeyValues {
                    index: Cell::new(next_index),
                    epoch: borrow.epoch,
                    container: unsafe { borrow.container.dup() },
                    bucket: borrow.bucket.clone(),
                },
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
        super::iter::spread_iter(strand, this, sink).await
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        Dict::key_values_iter_op_next(
            &borrow.index,
            borrow.epoch,
            &borrow.container,
            borrow.bucket.clone(),
            strand,
            out,
        )
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
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}

impl<'v> Protocol<'v> for Keys<'v> {
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
        crate::fmt!(strand, w, "<keys iterator>")
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_unpack<'s>(
        this: Recv<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
        sig: &sig::Unpack<'v, '_>,
        mut out: Slots<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let mut visited = borrow.visited.borrow().clone();
        let next_index = {
            with_pin!(strand, borrow.container, |container| {
                let dict = container
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                if dict.epoch != borrow.epoch {
                    return Err(Error::concurrency_msg(
                        strand,
                        "collection was modified during iteration",
                    ));
                }
                let mut pending = Vec::new();
                let next_index = super::iter::unpack_cursor(
                    strand,
                    sig,
                    &mut out,
                    borrow.index.get(),
                    |_, i, mut out| {
                        Dict::next_key_from_index(&dict, i, &visited, &mut pending).map(
                            |(next, key)| {
                                out.store(key);
                                next
                            },
                        )
                    },
                )?;
                for bucket_index in pending {
                    visited.set(bucket_index, true);
                }
                Ok(next_index)
            })?
        };
        if let Some(i) = sig.pos_rest_slot() {
            let embryo = strand.builtin_types().dict_keys.embryo(strand);
            // SAFETY: the reference moves straight into the allocated iterator
            embryo.finish(
                strand,
                Keys {
                    index: Cell::new(next_index),
                    epoch: borrow.epoch,
                    container: unsafe { borrow.container.dup() },
                    visited: RefCell::new(visited),
                },
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
        super::iter::spread_iter(strand, this, sink).await
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        Dict::keys_iter_op_next(
            &borrow.index,
            borrow.epoch,
            &borrow.container,
            &borrow.visited,
            strand,
            out,
        )
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
        args: crate::arg::Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}

// ── Unpack bookkeeping ──────────────────────────────────────────────

struct Seen<'v> {
    value: Value<'v>,
    hash: u64,
    count: usize,
}

impl<'v> Clone for Seen<'v> {
    fn clone(&self) -> Self {
        Seen {
            value: self.value.dup(),
            hash: self.hash,
            count: self.count,
        }
    }
}

struct Skip<'v> {
    table: RawTable<Seen<'v>>,
    count: usize,
}

impl<'v> Clone for Skip<'v> {
    fn clone(&self) -> Self {
        let mut new_table = RawTable::with_capacity(self.table.len());
        unsafe {
            for bucket in self.table.iter() {
                let seen = bucket.as_ref();
                new_table.insert_no_grow(seen.hash, seen.clone());
            }
        }
        Skip {
            table: new_table,
            count: self.count,
        }
    }
}

impl<'v> Skip<'v> {
    fn new() -> Self {
        Skip {
            table: RawTable::new(),
            count: 0,
        }
    }

    fn add<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
        hv: u64,
    ) -> Result<'v, 's, usize> {
        let found = super::table::find(&self.table, hv, |s| value.eq(strand, &s.value))?;
        let count = unsafe {
            match found {
                Some(bucket) => {
                    let bucket = bucket.as_mut();
                    let count = bucket.count;
                    bucket.count += 1;
                    count
                }
                None => {
                    self.table.insert(
                        hv,
                        Seen {
                            value: value.dup(),
                            hash: hv,
                            count: 1,
                        },
                        |s| s.hash,
                    );
                    0
                }
            }
        };
        self.count += 1;
        Ok(count)
    }
}

/// Position of a lazy rest over a dict's leftover pairs, which follow
/// insertion order from `index`, skipping the instances of each key that
/// `skip` counts as consumed.
#[derive(Clone)]
struct UnpackState<'v> {
    index: usize,
    skip: Skip<'v>,
}

impl<'v> UnpackState<'v> {
    /// Returns the next leftover pair, advancing past it.
    fn next_pair<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        dict: &Dict<'v>,
    ) -> Result<'v, 's, Option<(Value<'v>, Value<'v>)>> {
        while let Some(slot) = dict.index.get(self.index) {
            self.index += 1;
            let Some((bucket, _)) = slot else {
                continue;
            };
            let entry = unsafe { bucket.as_ref() };
            let instance = self.skip.add(strand, &entry.key, entry.hash)?;
            if instance < entry.value.len() {
                return Ok(Some((entry.key.dup(), entry.value.at(instance).dup())));
            }
        }
        Ok(None)
    }
}

// ── Unpack ──────────────────────────────────────────────────────────

/// Lazy rest over a dict's leftover pairs.
pub(crate) struct Unpack<'v> {
    state: UnpackState<'v>,
    epoch: u64,
    dict: GcObj<'v, Dict<'v>>,
}

/// Creates the lazy rest over a dict's leftover pairs.
/// Stores a dict rest.  `dict` runs after the rest is allocated, so the reference
/// it returns moves straight into the rest.
fn make_unpack<'v>(
    strand: &mut Strand<'v, '_>,
    dict: impl FnOnce() -> GcObj<'v, Dict<'v>>,
    epoch: u64,
    state: UnpackState<'v>,
    out: impl Output<'v>,
) {
    let embryo = strand.builtin_types().dict_unpack.embryo(strand);
    let dict = dict();
    embryo.finish(strand, Unpack { state, dict, epoch }, out);
}

unsafe impl<'v> Collect for Unpack<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.dict.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Unpack<'v> {
    fn check_epoch<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        with_pin!(strand, self.dict, |dict| {
            let dict = dict.borrow().ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != self.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            Ok(())
        })
    }
}

impl<'v> Protocol<'v> for Unpack<'v> {
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
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        super::iter::iterable_mcall(strand, &this, method, args, out).await
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<dict rest>")
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        borrow.check_epoch(strand)?;
        // SAFETY: TODO(#1707): as loose as the `Value::dup`s in the cloned `Skip`
        let container = unsafe { borrow.dict.dup() };
        let (epoch, resume, skip) = with_pin!(strand, container, |pinned| {
            let dict = pinned.borrow().ok_or_else(|| Error::concurrency(strand))?;
            let resume = borrow.state.index;

            // Match against a copy, so a failure leaves the rest as it was
            let mut skip = borrow.state.skip.clone();
            dict.unpack_matched(strand, sig, &mut out, &mut skip)?;
            Ok((borrow.epoch, resume, skip))
        })?;
        drop(borrow);
        dict_rests(strand, sig, &mut out, || container, epoch, resume, skip);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let container = &borrow.dict;
        let epoch = with_pin!(strand, container, |container| {
            Ok(container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?
                .epoch)
        })?;
        if epoch != borrow.epoch {
            return Err(Error::concurrency(strand));
        }
        let mut state = borrow.state.clone();
        let mut counter = 0usize;
        loop {
            counter += 1;
            if counter.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            let pair = {
                with_pin!(strand, container, |container| {
                    let dict = container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?;
                    if dict.epoch != borrow.epoch {
                        return Err(Error::concurrency(strand));
                    }
                    state.next_pair(strand, &dict)
                })?
            };
            let Some((key, value)) = pair else {
                return Ok(());
            };
            Dict::spread_key_value(strand, key, value, sink)?;
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let embryo = strand.builtin_types().dict_unpack_iter.embryo(strand);
        // SAFETY: the references move straight into the allocated iterator
        let iter = UnpackIter {
            state: borrow.state.clone(),
            epoch: borrow.epoch,
            dict: unsafe { borrow.dict.dup() },
        };
        embryo.finish(strand, iter, out);
        Ok(())
    }
}

// ── UnpackIter ──────────────────────────────────────────────────────

/// Iterator over a dict rest's `(key, value)` pairs.
pub(crate) struct UnpackIter<'v> {
    state: UnpackState<'v>,
    epoch: u64,
    dict: GcObj<'v, Dict<'v>>,
}

unsafe impl<'v> Collect for UnpackIter<'v> {
    const CYCLIC: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.dict.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for UnpackIter<'v> {
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
        crate::fmt!(strand, w, "<dict rest iterator>")
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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let UnpackIter { state, epoch, dict } = &mut *borrow;
        with_pin!(strand, dict, |dict_obj| {
            let dict = dict_obj
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != *epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            let Some((key, value)) = state.next_pair(strand, &dict)? else {
                return Ok(false);
            };
            tuple::create(strand, [key, value], out);
            Ok(true)
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
        super::iter::spread_iter(strand, this, sink).await
    }
}

// ── Protocol: Dict ──────────────────────────────────────────────────

impl<'v> Protocol<'v> for Dict<'v> {
    fn op_fmt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        spec: &Spec,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        use crate::value::fmt::{Fill, Kind, Pad};

        let kind = spec
            .kind
            .ok_or_else(|| crate::value::fmt::unresolved_kind(strand))?;
        if !kind.is_text() || spec.sign.is_some() || spec.fill == Fill::Zero {
            return Err(Error::type_error(strand, "unsupported dict format option"));
        }
        let mut pad = Pad::new(*spec, w);
        if spec.alt {
            Self::fmt_pretty(this, strand, kind, &mut pad)?;
        } else {
            match kind {
                Kind::Str => Self::op_display(this, strand, &mut pad)?,
                Kind::Dbg => Self::op_debug(this, strand, &mut pad)?,
                Kind::Verbatim => Self::op_verbatim(this, strand, &mut pad)?,
                _ => unreachable!(),
            }
        }
        pad.finish(strand)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().dict)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "{{")?;
        let mut index = 0usize;
        let mut count = 0usize;
        unsafe {
            while let Some(bucket) = borrow.index.get(index) {
                index += 1;
                let (bucket, subindex) = if let Some((bucket, subindex)) = bucket {
                    (bucket.clone(), *subindex)
                } else {
                    continue;
                };
                if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                if count > 0 {
                    crate::fmt!(strand, w, ", ")?;
                }
                count += 1;
                if let Some(sym) = bucket.as_ref().key.downcast_ref(strand.builtin_types().sym) {
                    crate::fmt!(strand, w, "{}", sym.name())?;
                } else {
                    bucket.as_ref().key.op_debug(strand, w)?;
                }
                crate::fmt!(strand, w, ": ")?;
                bucket.as_ref().value.at(subindex).op_debug(strand, w)?;
            }
        }
        crate::fmt!(strand, w, "}}")
    }

    fn op_bool<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, bool> {
        let Ok(borrow) = this.borrow(strand) else {
            return Ok(true);
        };
        Ok(borrow.total_pairs != 0)
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        sym::DICT.hash(hasher);
        unsafe {
            let mut i = 0usize;
            while i < borrow.index.len() {
                let (elem, subindex) = if let Some((elem, subindex)) = borrow.index.get_unchecked(i)
                {
                    (elem, *subindex)
                } else {
                    i += 1;
                    continue;
                };
                if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let elem = elem.as_ref();
                elem.hash.hash(hasher);
                elem.value.at(subindex).op_hash(strand, hasher)?;
                i += 1;
            }
        }
        Ok(())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().dict) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            // Equal pair counts make both walks end together
            if left.total_pairs != right.total_pairs {
                return Ok(Value::FALSE);
            }
            let pairs = left
                .index
                .iter()
                .flatten()
                .zip(right.index.iter().flatten());
            for (i, ((l, subl), (r, subr))) in pairs.enumerate() {
                if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let (l, r) = unsafe { (l.as_ref(), r.as_ref()) };
                if l.hash != r.hash
                    || !l.key.op_eq(strand, &r.key)?.to_bool(strand)?
                    || !l
                        .value
                        .at(*subl)
                        .op_eq(strand, r.value.at(*subr))?
                        .to_bool(strand)?
                {
                    return Ok(Value::FALSE);
                }
            }
            Ok(Value::TRUE)
        })
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().dict) {
            other
        } else {
            return Err(Error::unsupported(strand, "`<`"));
        };
        let left = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            unsafe {
                let mut i = 0usize;
                let mut j = 0usize;
                while i < left.index.len() && j < right.index.len() {
                    let l = if let Some(l) = left.index.get_unchecked(i) {
                        l
                    } else {
                        i += 1;
                        continue;
                    };
                    let r = if let Some(r) = right.index.get_unchecked(j) {
                        r
                    } else {
                        j += 1;
                        continue;
                    };

                    if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        strand.check_trap()?;
                    }
                    let (l, subl) = (l.0.as_ref(), l.1);
                    let (r, subr) = (r.0.as_ref(), r.1);
                    for (l, r) in [(&l.key, &r.key), (l.value.at(subl), r.value.at(subr))] {
                        if !l.op_eq(strand, r)?.to_bool(strand)? {
                            return Ok(Value::from_bool(l.op_lt(strand, r)?.to_bool(strand)?));
                        }
                    }
                    i += 1;
                    j += 1;
                }
                Ok(Value::from_bool(right.table.len() > left.table.len()))
            }
        })
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        index.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let dict = this.borrow(strand)?;
        match dict.find(strand, hv, index)? {
            Some(pair) => {
                Output::set(strand, out, unsafe { pair.as_ref().value.latest() });
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let mut borrow = this.borrow_mut(strand)?;
        borrow.insert(strand, key.take(), value.take(), hv, true)?;
        borrow.epoch += 1;
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CLEAR => {
                unpack!(strand, args =>);
                Self::mcall_clear(this, strand)
            }
            sym::INSERT => {
                unpack!(strand, args => key, value);
                Self::mcall_insert(this, strand, key, value)
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => key, ?subindex, ?:default, ?else_key: or_else);
                Self::mcall_get(this, strand, key, subindex, default, or_else, out).await
            }
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                unpack!(strand, args => key, ?subindex, ?:default, ?else_key: or_else);
                Self::mcall_pop(this, strand, key, subindex, default, or_else, out).await
            }
            sym::DELETE => {
                unpack!(strand, args => key);
                Self::mcall_delete(this, strand, key, out)
            }
            sym::PAIRS => {
                unpack!(strand, args =>);
                Self::op_iter(this, strand, out).await
            }
            sym::KEYS => {
                unpack!(strand, args =>);
                let borrow = this.borrow(strand)?;
                let epoch = borrow.epoch;
                let dict = this.to_strong();
                strand.builtin_types().dict_keys.create(
                    strand,
                    Keys {
                        index: Cell::new(0),
                        epoch,
                        visited: RefCell::new(bitbox![0; borrow.table.buckets()]),
                        container: dict,
                    },
                    out,
                );
                Ok(())
            }
            sym::VALUES => {
                unpack!(strand, args => ?key);
                let epoch = this.borrow(strand)?.epoch;
                let dict = this.to_strong();
                if let Some(key) = key {
                    let mut hasher = strand.vm().hasher();
                    key.op_hash(strand, &mut hasher)?;
                    let hv = hasher.finish();
                    let bucket = this.borrow(strand)?.find(strand, hv, &key)?;
                    strand.builtin_types().dict_key_values.create(
                        strand,
                        KeyValues {
                            index: Cell::new(0),
                            epoch,
                            container: dict,
                            bucket,
                        },
                        out,
                    );
                } else {
                    strand.builtin_types().dict_values.create(
                        strand,
                        Values {
                            index: Cell::new(0),
                            epoch,
                            container: dict,
                        },
                        out,
                    );
                }
                Ok(())
            }
            sym::COUNT => {
                unpack!(strand, args => ?key);
                Self::mcall_count(this, strand, key, out)
            }
            sym::COPY => {
                unpack!(strand, args =>);
                if this.delegator().is_some() {
                    return with_slots!(strand, |mut receiver, mut ty| {
                        Output::set(strand, Slot::reborrow(&mut receiver), &this);
                        receiver.op_type(strand, Slot::reborrow(&mut ty));
                        call!(strand, &ty, &receiver, out)
                    });
                }
                let borrow = this.borrow(strand)?;
                let mut dict = Dict::new();
                for entry in borrow.index.iter().flatten() {
                    let (bucket, subindex) = entry;
                    let bucket = unsafe { bucket.as_ref() };
                    dict.insert(
                        strand,
                        bucket.key.dup(),
                        bucket.value.at(*subindex).dup(),
                        bucket.hash,
                        false,
                    )?;
                }
                strand.builtin_types().dict.create(strand, dict, out);
                Ok(())
            }
            sym::CONTAINS => {
                unpack!(strand, args => key, ?value);
                Self::mcall_contains(this, strand, key, value, out)
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "dict.len is a field, not a method",
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
                let input = this.borrow(strand)?.total_pairs;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::CLEAR
            | sym::INSERT
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

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = Iter {
            index: Cell::new(0),
            dict: this.to_strong(),
            epoch: this.borrow(strand)?.epoch,
        };
        strand
            .vm()
            .builtin_types()
            .dict_iter
            .create(strand, iter, out);
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        unsafe {
            let mut i = 0usize;
            while i < dict.index.len() {
                let Some((bucket, subindex)) = dict.index.get_unchecked(i) else {
                    i += 1;
                    continue;
                };
                if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let bucket = bucket.as_ref();
                Self::spread_key_value(strand, &bucket.key, bucket.value.at(*subindex), sink)?;
                i += 1;
            }
        }
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        let mut skip = Skip::new();
        dict.unpack_matched(strand, sig, &mut out, &mut skip)?;
        let epoch = dict.epoch;
        drop(dict);
        dict_rests(strand, sig, &mut out, || this.to_strong(), epoch, 0, skip);
        Ok(())
    }
}

/// Stores the rests of an unpack of a dict, or of a rest of one, whose
/// leftover pairs start at `index` past what `skip` counts. A dict has no
/// positional items, so a `*name` rest is empty.  `dict` is called as for
/// [`make_unpack`].
fn dict_rests<'v>(
    strand: &mut Strand<'v, '_>,
    sig: &sig::Unpack<'v, '_>,
    out: &mut Slots<'v, '_>,
    dict: impl FnOnce() -> GcObj<'v, Dict<'v>>,
    epoch: u64,
    index: usize,
    skip: Skip<'v>,
) {
    let state = UnpackState { index, skip };
    match sig.variadic {
        Variadic::Capture => {
            make_unpack(strand, dict, epoch, state, out.at(sig.len() - 1));
        }
        _ => {
            if let Some(i) = sig.pos_rest_slot() {
                tuple::create(strand, [], out.at(i));
            }
            if let Some(i) = sig.key_rest_slot() {
                make_unpack(strand, dict, epoch, state, out.at(i));
            }
        }
    }
}

impl<'v> Dict<'v> {
    fn fmt_pretty<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        kind: crate::value::fmt::Kind,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        if dict.total_pairs == 0 {
            return crate::fmt!(strand, w, "{{}}");
        }
        crate::fmt!(strand, w, "{{\n")?;
        unsafe {
            for (index, entry) in dict.index.iter().enumerate() {
                let Some((bucket, subindex)) = entry else {
                    continue;
                };
                if (index + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let bucket = bucket.as_ref();
                let mut rendered = String::new();
                if let Some(symbol) = bucket.key.downcast_ref(strand.builtin_types().sym) {
                    rendered.push_str(symbol.name());
                } else {
                    bucket.key.fmt(
                        strand,
                        &Spec {
                            kind: Some(kind),
                            ..Default::default()
                        },
                        &mut rendered,
                    )?;
                }
                rendered.push_str(": ");
                let value = bucket.value.at(*subindex);
                value.fmt(
                    strand,
                    &Spec {
                        alt: value.downcast_ref(strand.builtin_types().array).is_some()
                            || value.downcast_ref(strand.builtin_types().dict).is_some(),
                        kind: Some(kind),
                        ..Default::default()
                    },
                    &mut rendered,
                )?;
                let mut indented = String::new();
                crate::value::fmt::push_indented(&mut indented, &rendered, 2);
                crate::fmt!(strand, w, "{indented},\n")?;
            }
        }
        crate::fmt!(strand, w, "}}")
    }

    /// Matches the positional and key items of `sig` against the pairs an
    /// unpack has not yet consumed, filling their slots, and checks for
    /// leftovers that no rest takes.
    ///
    /// A dict has no positional items, so a required one is missing and an
    /// optional one takes its default. `skip` counts the instances of each
    /// key consumed so far, and is advanced past those the key items take.
    fn unpack_matched<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        sig: &sig::Unpack<'v, '_>,
        out: &mut Slots<'v, '_>,
        skip: &mut Skip<'v>,
    ) -> Result<'v, 's, ()> {
        if sig.required > 0 {
            return Err(Error::missing_positional(strand, 0));
        }
        let pos_count = sig.optional.len();
        for (i, default) in sig.optional.iter().enumerate() {
            out.at(i).store(default.dup());
        }

        with_slots!(strand, |mut key_value| {
            for (i, key) in sig.keys.iter().enumerate() {
                Output::set(strand, Slot::reborrow(&mut key_value), &key.kind);

                let mut hasher = strand.vm().hasher();

                key_value.op_hash(strand, &mut hasher)?;

                let hv = hasher.finish();
                let seen = skip.add(strand, &key_value, hv)?;

                let instance = i64::try_from(seen).map_err(|_| Error::overflow(strand))?;
                if let Some(value) = self.get(strand, &key_value, Some(instance))? {
                    out.at(pos_count + i).store(value.dup())
                } else if let Some(default) = &key.default {
                    out.at(pos_count + i).store(default.dup())
                } else {
                    return Err(match &key.kind {
                        UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                        UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
                    });
                }
            }
            Ok::<_, Error<'v, 's>>(())
        })?;

        if sig.key_rest() == Rest::None
            && let Some(key) = self.leftover_key(strand, skip)?
        {
            return Err(Error::unexpected_key(strand, &key));
        }
        Ok(())
    }

    fn iter_op_next<'a, 's>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        with_pin!(strand, container, |container| {
            let dict = container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            loop {
                break if let Some(bucket) = dict.index.get(index.get()) {
                    index.update(|i| i + 1);
                    if let Some((bucket, subindex)) = bucket {
                        tuple::create(
                            strand,
                            unsafe {
                                [
                                    bucket.as_ref().key.dup(),
                                    bucket.as_ref().value.at(*subindex).dup(),
                                ]
                            },
                            out,
                        );
                        Ok(true)
                    } else {
                        continue;
                    }
                } else {
                    Ok(false)
                };
            }
        })
    }

    fn mcall_clear<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut dict = this.borrow_mut(strand)?;
        dict.table.clear();
        dict.index.clear();
        dict.total_pairs = 0;
        dict.epoch += 1;
        Ok(())
    }

    fn mcall_insert<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let mut dict = this.borrow_mut(strand)?;
        dict.insert(strand, key.take(), value.take(), hv, false)?;
        dict.epoch += 1;
        Ok(())
    }

    async fn mcall_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        subindex: Option<Slot<'v, 'a>>,
        default: Option<Slot<'v, 'a>>,
        or_else: Option<Slot<'v, 'a>>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let else_key = Sym::well_known(sym::ELSE);
        if default.is_some() && or_else.is_some() {
            return Err(Error::unexpected_key(strand, else_key));
        }
        let subindex = subindex
            .map(|s| s.to_i64(strand).map_err(|_| Error::index(strand)))
            .transpose()?;
        if let Some(value) = this.borrow(strand)?.get(strand, &key, subindex)? {
            out.store(value.dup());
            Ok(())
        } else if let Some(mut default) = default {
            out.store(default.take());
            Ok(())
        } else if let Some(else_key) = or_else {
            call!(strand, else_key, out)
        } else {
            out.store(Value::NIL);
            Ok(())
        }
    }

    async fn mcall_pop<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        subindex: Option<Slot<'v, 'a>>,
        default: Option<Slot<'v, 'a>>,
        or_else: Option<Slot<'v, 'a>>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let else_key = Sym::well_known(sym::ELSE);
        if default.is_some() && or_else.is_some() {
            return Err(Error::unexpected_key(strand, else_key));
        }
        let subindex = subindex
            .map(|s| s.to_i64(strand).map_err(|_| Error::index(strand)))
            .transpose()?;
        {
            let mut dict = this.borrow_mut(strand)?;
            if let Some(bucket) = dict.find(strand, hv, &key)? {
                unsafe {
                    match &mut bucket.as_mut().value {
                        EntryValue::Single { index, .. } => {
                            let subindex = match subindex {
                                Some(subindex) => index::element(1, subindex),
                                None => Some(0),
                            };
                            if subindex == Some(0) {
                                dict.total_pairs -= 1;
                                *dict.index.get_unchecked_mut(*index) = None;
                                let bucket = dict.table.remove(bucket).0;
                                dict.epoch += 1;
                                match bucket.value {
                                    EntryValue::Single { value, .. } => out.store(value),
                                    EntryValue::Multi(_) => unreachable!(),
                                };
                                return Ok(());
                            }
                        }
                        EntryValue::Multi(items) => {
                            // Without an explicit instance, pop the first value
                            // for the key, so repeated pops drain a multi-value
                            // key in insertion order.
                            let subindex = match subindex {
                                Some(subindex) => index::element(items.len(), subindex),
                                None => Some(0),
                            };
                            if let Some(subindex) = subindex {
                                dict.total_pairs -= 1;
                                let (value, index) = items.remove(subindex);
                                *dict.index.get_unchecked_mut(index) = None;
                                if items.is_empty() {
                                    dict.table.remove(bucket);
                                } else {
                                    for i in subindex..items.len() {
                                        let (_, idx) = items.get_unchecked(i);
                                        dict.index.get_unchecked_mut(*idx).as_mut().unwrap().1 = i;
                                    }
                                }
                                dict.epoch += 1;
                                out.store(value);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        if let Some(mut default) = default {
            out.store(default.take());
            Ok(())
        } else if let Some(else_key) = or_else {
            call!(strand, else_key, out)
        } else {
            Err(Error::index(strand))
        }
    }

    fn mcall_delete<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut dict = this.borrow_mut(strand)?;
        let deleted = dict.delete(strand, &key)?;
        Output::set(strand, out, deleted);
        Ok(())
    }

    fn mcall_contains<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Option<Slot<'v, 'a>>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let dict = this.borrow(strand)?;
        let found = match dict.find(strand, hv, &key)? {
            None => false,
            Some(bucket) => {
                if let Some(expected_value) = value {
                    let bucket_ref = unsafe { bucket.as_ref() };
                    match &bucket_ref.value {
                        EntryValue::Single { value, .. } => {
                            value.op_eq(strand, &expected_value)?.to_bool(strand)?
                        }
                        EntryValue::Multi(_) => {
                            let mut found = false;
                            let bucket_ref = unsafe { bucket.as_ref() };
                            let items = match &bucket_ref.value {
                                EntryValue::Multi(items) => items,
                                _ => unreachable!(),
                            };
                            for (i, (v, _)) in items.iter().enumerate() {
                                if (i + 1) % crate::INTERRUPT_INTERVAL == 0 {
                                    strand.check_trap()?;
                                }
                                if v.op_eq(strand, &expected_value)?.to_bool(strand)? {
                                    found = true;
                                    break;
                                }
                            }
                            found
                        }
                    }
                } else {
                    true
                }
            }
        };
        out.store(Value::from_bool(found));
        Ok(())
    }

    fn mcall_count<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        key: Option<Slot<'v, 'a>>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        let count = if let Some(key) = key {
            let mut hasher = strand.vm().hasher();
            key.op_hash(strand, &mut hasher)?;
            let hv = hasher.finish();
            dict.find(strand, hv, &key)?
                .map(|bucket| unsafe { bucket.as_ref().value.len() })
                .unwrap_or(0)
        } else {
            dict.table.len()
        };
        let value = i64::try_from(count).map_err(|_| Error::overflow(strand))?;
        Output::set(strand, out, value);
        Ok(())
    }
}

// ── Dict Class ──────────────────────────────────────────────────

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
        unpack!(strand, args => items);
        let mut dict = Dict::new();

        // FIXME: `Dict` is not GC-scannable, but then again if it were it would also
        // be mutably borrowed, which would inhibit GC.  This needs a resolution.
        let mut sink = DictPairs { dict: &mut dict };
        items
            .op_spread(strand, SpreadContext::Pairs, &mut sink)
            .await?;
        strand.builtin_types().dict.create(strand, dict, out);
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
        crate::fmt!(strand, w, "<type std.Dict>")
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
                Method(sym::CLEAR),
                Method(sym::INSERT),
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
            | sym::CLEAR
            | sym::COUNT
            | sym::INSERT
            | sym::GET
            | sym::POP
            | sym::DELETE
            | sym::PAIRS
            | sym::KEYS
            | sym::VALUES
            | sym::COPY
            | sym::CONTAINS
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

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                unpack!(strand, args => self_val, items);
                with_slots!(strand, |mut native| {
                    call!(strand, &strand.singletons().dict, items, &mut native)?;
                    self_val.op_fill(strand, &strand.singletons().dict, native.take())?;
                    Ok(())
                })
            }
            _ => type_mcall_fallback(strand, &strand.singletons().dict, method, args, out).await,
        }
    }
}
