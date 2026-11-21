use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    hash::{Hash, Hasher},
    mem,
    ops::ControlFlow,
};

use bitvec::{bitbox, boxed::BitBox};
use dolang_util::hashbrown::raw::{Bucket, RawTable};

use crate::{
    arg::{Arg, Args},
    bytecode::Rest,
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit, with_pin},
    object::protocol::{Unpack as ProtoUnpack, UnpackItem, members},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        Input, Output, Slot, TypeObject, Value,
        fmt::{Format, Spec},
    },
    vm::{AllocExt, Vm},
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
    fn leftover_key(&self, skip: &Skip) -> Option<&Value<'v>> {
        let mut skip = skip.clone();
        for (bucket, _) in self.index.iter().flatten() {
            let instance = skip.add(bucket);
            let entry = unsafe { bucket.as_ref() };
            if instance < entry.value.len() {
                return Some(&entry.key);
            }
        }
        None
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

    /// A copy with the same pairs in the same order, made without running any code, as
    /// [`Self::rehash`] moves them.
    ///
    /// # Safety
    /// The copies are loose: the caller must store the dict in an allocated object's
    /// payload with no allocation in between.
    unsafe fn cloned(&self) -> Self {
        let mut table = RawTable::with_capacity(self.table.len());
        let mut remap: Vec<Option<Bucket<Entry<'v>>>> = vec![None; self.table.buckets()];
        let mut index = Vec::with_capacity(self.total_pairs);
        for (old_bucket, subindex) in self.index.iter().flatten() {
            let old_index = unsafe { self.table.bucket_index(old_bucket) };
            let old = unsafe { old_bucket.as_ref() };
            // SAFETY (here and for the key): the caller's contract
            let value = unsafe { old.value.at(*subindex).dup() };
            let i = index.len();
            let (bucket, subindex) = match &remap[old_index] {
                // A later instance of a key already copied
                Some(bucket) => {
                    let bucket: Bucket<Entry<'v>> = Bucket::clone(bucket);
                    let entry = unsafe { bucket.as_mut() };
                    let subindex = match &mut entry.value {
                        EntryValue::Multi(items) => {
                            items.push((value, i));
                            items.len() - 1
                        }
                        single => {
                            let EntryValue::Single {
                                value: first,
                                index: first_index,
                            } = mem::replace(single, EntryValue::Multi(Vec::new()))
                            else {
                                unreachable!()
                            };
                            *single = EntryValue::Multi(vec![(first, first_index), (value, i)]);
                            1
                        }
                    };
                    (bucket, subindex)
                }
                None => {
                    let entry = Entry {
                        key: unsafe { old.key.dup() },
                        value: EntryValue::Single { value, index: i },
                        hash: old.hash,
                    };
                    let bucket = unsafe { table.insert_no_grow(old.hash, entry) };
                    remap[old_index] = Some(bucket.clone());
                    (bucket, 0)
                }
            };
            index.push(Some((bucket, subindex)));
        }
        Self {
            table,
            index,
            epoch: 0,
            total_pairs: self.total_pairs,
        }
    }

    fn find<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        hash: u64,
        key: &Value<'v>,
    ) -> Result<'v, 's, Option<Bucket<Entry<'v>>>> {
        super::table::find(&self.table, hash, |entry| key.eq(strand, &entry.key))
    }

    /// Inserts `value` under `key`, whose hash is `hv`, replacing every value already
    /// there if `unique`. The key and value stay in their slots, which root them, until the
    /// lookup, which can run code, is over.
    pub(crate) fn insert<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut key: Slot<'v, '_>,
        mut value: Slot<'v, '_>,
        hv: u64,
        unique: bool,
    ) -> Result<'v, 's, ()> {
        unsafe {
            let found = self.find(strand, hv, &key)?;
            let (key, value) = (key.take(strand), value.take(strand));
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

    /// Store a dict of `args`, which must all be key arguments, into `out`.
    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        // Hashing runs arbitrary code, so the pairs go straight into the allocated dict
        let mut dict = Slot::from_output(&mut out);
        strand
            .builtin_types()
            .dict
            .create(strand, Self::new(), Slot::reborrow(&mut dict));
        let mut sink = DictPairs { dict: &dict };
        for (index, arg) in args.enumerate() {
            if (index + 1) % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?;
            }
            match arg {
                Arg::Pos(value) => sink.positional(strand, value)?,
                Arg::Key(key, value) => sink.symbol(strand, key, value)?,
            }
        }
        Ok(())
    }

    /// Store a dict of `args` into `out`, keying the positional ones by their index among
    /// the positional arguments and the key ones by symbol.
    pub(crate) fn from_indexed_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let mut dict = Slot::from_output(&mut out);
        strand
            .builtin_types()
            .dict
            .create(strand, Self::new(), Slot::reborrow(&mut dict));
        let sink = DictPairs { dict: &dict };
        let mut position = 0i64;
        with_slots!(strand, |mut key| {
            for (index, arg) in args.enumerate() {
                if (index + 1) % crate::INTERRUPT_INTERVAL == 0 {
                    strand.check_trap_gc()?;
                }
                let value = match arg {
                    Arg::Pos(value) => {
                        strand.store(Slot::reborrow(&mut key), position);
                        position += 1;
                        value
                    }
                    Arg::Key(sym, value) => {
                        strand.store(Slot::reborrow(&mut key), sym);
                        value
                    }
                };
                sink.insert(strand, Slot::reborrow(&mut key), value)?;
            }
            Ok(())
        })
    }

    /// Store a dict of `args`, alternating keys and values, into `out`, expanding `...`
    /// spreads (keyed `ITER`) of pairs.
    pub(crate) async fn from_builtin_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let mut dict = Slot::from_output(&mut out);
        strand
            .builtin_types()
            .dict
            .create(strand, Self::new(), Slot::reborrow(&mut dict));
        let mut sink = DictPairs { dict: &dict };
        let mut counter = 1;
        loop {
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?
            }
            counter += 1;
            let key = match args.next() {
                Some(Arg::Pos(key)) => key,
                Some(Arg::Key(sym, expand)) if sym.tag() == sym::ITER => {
                    expand
                        .op_spread(strand, SpreadContext::Pairs, &mut sink)
                        .await?;
                    continue;
                }
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => return Ok(()),
            };
            let value = match args.next() {
                Some(Arg::Pos(value)) => value,
                Some(Arg::Key(sym, _)) => return Err(Error::unexpected_key(strand, sym)),
                None => return Err(Error::missing_positional(strand, counter)),
            };
            sink.insert(strand, key, value)?;
        }
    }
}

// ── Hashing ─────────────────────────────────────────────────────────

fn hasher<'v>() -> impl Fn(&Entry<'v>) -> u64 {
    |Entry { hash, .. }| *hash
}

/// Collects spread pairs into a dict, which holds only keyed entries. The dict object is
/// already allocated, so each pair is rooted as soon as it arrives, and is borrowed only
/// for each insert.
pub(crate) struct DictPairs<'b, 'v> {
    dict: &'b Value<'v>,
}

impl<'b, 'v> DictPairs<'b, 'v> {
    /// A sink into the dict in `dict`.
    pub(crate) fn new(dict: &'b Value<'v>) -> Self {
        Self { dict }
    }

    fn insert<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        mut key: Slot<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let dict = self
            .dict
            .downcast_ref(strand.builtin_types().dict)
            .expect("dict accumulator");
        with_pin!(strand, dict, |dict| {
            dict.borrow_mut()
                .ok_or_else(|| Error::concurrency(strand))?
                .insert(
                    strand,
                    Slot::reborrow(&mut key),
                    Slot::reborrow(&mut value),
                    hv,
                    false,
                )
        })
    }
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
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        with_slots!(strand, |mut key_slot| {
            strand.store(Slot::reborrow(&mut key_slot), key);
            self.insert(strand, key_slot, value)
        })
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Slot<'v, '_>,
        value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.insert(strand, key, value)
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
        strand.store(out, &strand.singletons().input_iter)
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
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _unpack: ProtoUnpack<'v, 'a>,
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
            strand.store(&mut key_slot, key);
            strand.store(&mut value_slot, value);
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
        f: impl FnOnce(&mut Strand<'v, 's>, &Dict<'v>) -> Result<'v, 's, R>,
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
            f(strand, &dict)
        })
    }

    /// The next value at or after `index`, borrowed from `dict`, and the index after it.
    fn next_value_from_index<'d>(
        dict: &'d Dict<'v>,
        index: usize,
    ) -> Option<(usize, &'d Value<'v>)> {
        let mut index = index;
        loop {
            let bucket = dict.index.get(index)?;
            index += 1;
            if let Some((bucket, subindex)) = bucket {
                // Safety: the bucket stays live while the dict is borrowed
                return Some((index, unsafe { bucket.as_ref() }.value.at(*subindex)));
            }
        }
    }

    /// Instance `index` of `bucket`'s values, borrowed from the `dict` that holds the bucket.
    fn next_value_from_bucket<'d>(
        _dict: &'d Dict<'v>,
        bucket: Option<Bucket<Entry<'v>>>,
        index: usize,
    ) -> Option<(usize, &'d Value<'v>)> {
        let bucket = bucket?;
        // Safety: the bucket stays live while the dict is borrowed
        let value = unsafe { bucket.as_ref() }.value.get(Some(index))?;
        Some((index + 1, value))
    }

    /// The next key at or after `index` not yet visited, borrowed from `dict`.
    fn next_key_from_index<'d>(
        dict: &'d Dict<'v>,
        index: usize,
        visited: &BitBox,
        pending: &mut Vec<usize>,
    ) -> Option<(usize, &'d Value<'v>)> {
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
            // Safety: the bucket stays live while the dict is borrowed
            return Some((index, &unsafe { bucket.as_ref() }.key));
        }
    }

    fn values_iter_op_next<'s>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Self::with_iter_inner(container, epoch, strand, |strand, dict| {
            if let Some((next_index, value)) = Self::next_value_from_index(dict, index.get()) {
                index.set(next_index);
                strand.store(&mut out, value);
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
        Self::with_iter_inner(container, epoch, strand, |strand, dict| {
            if let Some((next_index, value)) =
                Self::next_value_from_bucket(dict, bucket, index.get())
            {
                index.set(next_index);
                strand.store(&mut out, value);
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
        Self::with_iter_inner(container, epoch, strand, |strand, dict| {
            let mut visited = visited.borrow_mut();
            let mut pending = Vec::with_capacity(1);
            if let Some((next_index, key)) =
                Self::next_key_from_index(dict, index.get(), &visited, &mut pending)
            {
                for bucket_index in pending {
                    visited.set(bucket_index, true);
                }
                index.set(next_index);
                strand.store(&mut out, key);
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
        strand.store(out, &strand.singletons().input_iter)
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
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'s>(
        this: Recv<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
        unpack: ProtoUnpack<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
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
            unpack.cursor(
                strand,
                borrow.index.get(),
                |strand, i, slot| match Dict::next_value_from_index(&dict, *i) {
                    Some((next, value)) => {
                        strand.store(slot, value);
                        *i = next;
                        true
                    }
                    None => false,
                },
                |strand, next_index, slot| {
                    let embryo = strand.builtin_types().dict_values.embryo(strand);
                    // SAFETY: the reference moves straight into the allocated iterator
                    embryo.finish(
                        strand,
                        Values {
                            index: Cell::new(next_index),
                            epoch: borrow.epoch,
                            container: unsafe { borrow.container.dup() },
                        },
                        slot,
                    );
                    Ok(())
                },
            )
        })
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
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

impl<'v> Protocol<'v> for KeyValues<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        unpack: ProtoUnpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
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
            unpack.cursor(
                strand,
                borrow.index.get(),
                |strand, i, slot| match Dict::next_value_from_bucket(
                    &dict,
                    borrow.bucket.clone(),
                    *i,
                ) {
                    Some((next, value)) => {
                        strand.store(slot, value);
                        *i = next;
                        true
                    }
                    None => false,
                },
                |strand, next_index, slot| {
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
                        slot,
                    );
                    Ok(())
                },
            )
        })
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
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

impl<'v> Protocol<'v> for Keys<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().input_iter)
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
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'s>(
        this: Recv<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
        unpack: ProtoUnpack<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let visited = borrow.visited.borrow().clone();
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
            // The cursor: next index, buckets visited, and buckets to mark visited
            let cursor = (borrow.index.get(), visited, Vec::new());
            unpack.cursor(
                strand,
                cursor,
                |strand, (i, visited, pending), slot| match Dict::next_key_from_index(
                    &dict, *i, visited, pending,
                ) {
                    Some((next, key)) => {
                        strand.store(slot, key);
                        *i = next;
                        true
                    }
                    None => false,
                },
                |strand, (next_index, mut visited, pending), slot| {
                    for bucket_index in pending {
                        visited.set(bucket_index, true);
                    }
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
                        slot,
                    );
                    Ok(())
                },
            )
        })
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
        iter::unpack_iter_mcall(strand, &this, method, args, out).await
    }
}

// ── Unpack bookkeeping ──────────────────────────────────────────────

/// Counts, per key, the instances an unpack has consumed.
///
/// A key is identified by its entry, which stays put while the dict is unchanged, and every
/// use is guarded by the dict's epoch, so the counts hold no values and compare none.
#[derive(Clone, Default)]
struct Skip {
    // Entry address → instances consumed
    counts: HashMap<usize, usize>,
}

impl Skip {
    fn new() -> Self {
        Self::default()
    }

    /// Counts one more instance of `bucket`'s key, returning how many were counted before.
    fn add(&mut self, bucket: &Bucket<Entry<'_>>) -> usize {
        let count = self.counts.entry(bucket.as_ptr() as usize).or_insert(0);
        let prior = *count;
        *count += 1;
        prior
    }
}

/// Position of a lazy rest over a dict's leftover pairs, which follow
/// insertion order from `index`, skipping the instances of each key that
/// `skip` counts as consumed.
#[derive(Clone)]
struct UnpackState {
    index: usize,
    skip: Skip,
}

impl UnpackState {
    /// Returns the next leftover pair, advancing past it.
    fn next_pair<'v, 'd>(&mut self, dict: &'d Dict<'v>) -> Option<(&'d Value<'v>, &'d Value<'v>)> {
        while let Some(slot) = dict.index.get(self.index) {
            self.index += 1;
            let Some((bucket, _)) = slot else {
                continue;
            };
            let instance = self.skip.add(bucket);
            let entry = unsafe { bucket.as_ref() };
            if instance < entry.value.len() {
                return Some((&entry.key, entry.value.at(instance)));
            }
        }
        None
    }
}

// ── Unpack ──────────────────────────────────────────────────────────

/// Lazy rest over a dict's leftover pairs.
pub(crate) struct Unpack<'v> {
    state: UnpackState,
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
    state: UnpackState,
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
        unpack: ProtoUnpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        borrow.check_epoch(strand)?;
        let (rests, skip) = with_pin!(strand, borrow.dict, |pinned| {
            let dict = pinned.borrow().ok_or_else(|| Error::concurrency(strand))?;
            // Match against a copy, so a failure leaves the rest as it was
            let mut skip = borrow.state.skip.clone();
            let rests = dict.unpack_matched(strand, unpack, &mut skip)?;
            Ok((rests, skip))
        })?;
        let (epoch, resume) = (borrow.epoch, borrow.state.index);
        // SAFETY: the handle moves straight into the allocated rest
        let container = || unsafe { borrow.dict.dup() };
        rests.fill(strand, container, epoch, resume, skip);
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
        with_slots!(strand, |mut key, mut value| {
            loop {
                counter += 1;
                if counter.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let found = with_pin!(strand, container, |container| {
                    let dict = container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?;
                    if dict.epoch != borrow.epoch {
                        return Err(Error::concurrency(strand));
                    }
                    let pair = state.next_pair(&dict);
                    Ok(pair.map(|(k, v)| {
                        strand.store(&mut key, k);
                        strand.store(&mut value, v);
                    }))
                })?
                .is_some();
                if !found {
                    return Ok(());
                }
                Dict::spread_key_value(strand, &mut key, &mut value, sink)?;
            }
        })
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
    state: UnpackState,
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
        strand.store(out, &strand.singletons().input_iter)
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
        strand.store(out, &this);
        Ok(())
    }

    async fn op_unpack<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _unpack: ProtoUnpack<'v, 'a>,
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
            let Some((key, value)) = state.next_pair(&dict) else {
                return Ok(false);
            };
            tuple::create_pair(strand, key, value, out);
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
        strand.store(out, &strand.singletons().dict)
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
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let Some(other) = other.downcast_ref(strand.builtin_types().dict) else {
            return Err(Error::unsupported(strand, "`==`"));
        };
        let left = this.borrow(strand)?;
        with_pin!(strand, other, |other| {
            let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
            // Equal pair counts make both walks end together
            if left.total_pairs != right.total_pairs {
                strand.store(&mut out, Value::FALSE);
                return Ok(());
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
                    || !l.key.eq(strand, &r.key)?
                    || !l.value.at(*subl).eq(strand, r.value.at(*subr))?
                {
                    strand.store(&mut out, Value::FALSE);
                    return Ok(());
                }
            }
            strand.store(&mut out, Value::TRUE);
            Ok(())
        })
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
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
                        if !l.eq(strand, r)? {
                            let flag = l.lt(strand, r)?;
                            strand.store(&mut out, flag);
                            return Ok(());
                        }
                    }
                    i += 1;
                    j += 1;
                }
                strand.store(
                    &mut out,
                    Value::from_bool(right.table.len() > left.table.len()),
                );
                Ok(())
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
                strand.store(out, unsafe { pair.as_ref().value.latest() });
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
        borrow.insert(
            strand,
            Slot::reborrow(&mut key),
            Slot::reborrow(&mut value),
            hv,
            true,
        )?;
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
                let embryo = strand.builtin_types().dict_keys.embryo(strand);
                // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
                let keys = Keys {
                    index: Cell::new(0),
                    epoch,
                    visited: RefCell::new(bitbox![0; borrow.table.buckets()]),
                    container: unsafe { this.to_strong() },
                };
                embryo.finish(strand, keys, out);
                Ok(())
            }
            sym::VALUES => {
                unpack!(strand, args => ?key);
                let epoch = this.borrow(strand)?.epoch;
                if let Some(key) = key {
                    let mut hasher = strand.vm().hasher();
                    key.op_hash(strand, &mut hasher)?;
                    let hv = hasher.finish();
                    let bucket = this.borrow(strand)?.find(strand, hv, &key)?;
                    let embryo = strand.builtin_types().dict_key_values.embryo(strand);
                    // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
                    let values = KeyValues {
                        index: Cell::new(0),
                        epoch,
                        container: unsafe { this.to_strong() },
                        bucket,
                    };
                    embryo.finish(strand, values, out);
                } else {
                    let embryo = strand.builtin_types().dict_values.embryo(strand);
                    // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
                    let values = Values {
                        index: Cell::new(0),
                        epoch,
                        container: unsafe { this.to_strong() },
                    };
                    embryo.finish(strand, values, out);
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
                        strand.store(Slot::reborrow(&mut receiver), &this);
                        receiver.op_type(strand, Slot::reborrow(&mut ty));
                        call!(strand, &ty, &receiver, out)
                    });
                }
                let borrow = this.borrow(strand)?;
                let embryo = strand.builtin_types().dict.embryo(strand);
                // SAFETY: the dict is allocated; the copies move straight into it
                embryo.finish(strand, unsafe { borrow.cloned() }, out);
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
                strand.store(out, input);
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
        let epoch = this.borrow(strand)?.epoch;
        let embryo = strand.builtin_types().dict_iter.embryo(strand);
        // SAFETY: the iterator is allocated; the pinned receiver is copied straight into it
        let iter = Iter {
            index: Cell::new(0),
            dict: unsafe { this.to_strong() },
            epoch,
        };
        embryo.finish(strand, iter, out);
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
        unpack: ProtoUnpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        let mut skip = Skip::new();
        let rests = dict.unpack_matched(strand, unpack, &mut skip)?;
        let epoch = dict.epoch;
        drop(dict);
        // SAFETY: `fill` calls this after allocating each rest, whose payload the copy
        // of the pinned receiver becomes at once
        rests.fill(strand, || unsafe { this.to_strong() }, epoch, 0, skip);
        Ok(())
    }
}

/// The rests of an unpack of a dict, or of a rest of one, still to be filled.
struct Rests<'v, 'a> {
    /// A `...name` rest
    rest: Option<Slot<'v, 'a>>,
    /// A `*name` rest
    pos_rest: Option<Slot<'v, 'a>>,
    /// A `**name` rest
    key_rest: Option<Slot<'v, 'a>>,
}

impl<'v> Rests<'v, '_> {
    /// Stores the rests, whose leftover pairs start at `index` past what `skip`
    /// counts. A dict has no positional items, so a `*name` rest is empty. `dict` is
    /// called as for [`make_unpack`].
    fn fill(
        self,
        strand: &mut Strand<'v, '_>,
        dict: impl FnOnce() -> GcObj<'v, Dict<'v>>,
        epoch: u64,
        index: usize,
        skip: Skip,
    ) {
        if let Some(slot) = self.pos_rest {
            tuple::create(strand, [], slot);
        }
        if let Some(slot) = self.rest.or(self.key_rest) {
            make_unpack(strand, dict, epoch, UnpackState { index, skip }, slot);
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
    /// Unpacks the dict's pairs into the key parameters of `unpack`, adding the
    /// pairs taken to `skip`, and returns the rests still to be filled.
    fn unpack_matched<'a, 's>(
        &self,
        strand: &mut Strand<'v, 's>,
        unpack: ProtoUnpack<'v, 'a>,
        skip: &mut Skip,
    ) -> Result<'v, 's, Rests<'v, 'a>> {
        let key_rest = unpack.key_rest();
        let mut rests = Rests {
            rest: None,
            pos_rest: None,
            key_rest: None,
        };
        let mut position = 0;
        with_slots!(strand, |mut key_value| {
            for entry in unpack {
                match entry {
                    UnpackItem::Pos { slot, default } => {
                        match default {
                            Some(default) => strand.store(slot, default),
                            None => return Err(Error::missing_positional(strand, position)),
                        }
                        position += 1;
                    }
                    UnpackItem::SymKey { key, slot, default } => {
                        strand.store(Slot::reborrow(&mut key_value), key);
                        match (self.take_key(strand, &key_value, skip)?, default) {
                            (Some(value), _) => strand.store(slot, value),
                            (None, Some(default)) => strand.store(slot, default),
                            (None, None) => return Err(Error::missing_key(strand, key)),
                        }
                    }
                    UnpackItem::ConstKey { key, slot, default } => {
                        strand.store(Slot::reborrow(&mut key_value), key);
                        match (self.take_key(strand, &key_value, skip)?, default) {
                            (Some(value), _) => strand.store(slot, value),
                            (None, Some(default)) => strand.store(slot, default),
                            (None, None) => return Err(Error::missing_key(strand, key)),
                        }
                    }
                    UnpackItem::Rest { slot } => rests.rest = Some(slot),
                    UnpackItem::PosRest { slot } => rests.pos_rest = Some(slot),
                    UnpackItem::KeyRest { slot } => rests.key_rest = Some(slot),
                }
            }
            Ok::<_, Error<'v, 's>>(())
        })?;

        if key_rest == Rest::None
            && let Some(key) = self.leftover_key(skip)
        {
            return Err(Error::unexpected_key(strand, key));
        }
        Ok(rests)
    }

    /// Finds the value of the first pair under `key` that `skip` doesn't count yet,
    /// and counts it.
    fn take_key<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        key: &Value<'v>,
        skip: &mut Skip,
    ) -> Result<'v, 's, Option<&Value<'v>>> {
        let mut hasher = strand.vm().hasher();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        Ok(self.find(strand, hv, key)?.and_then(|bucket| {
            let instance = skip.add(&bucket);
            unsafe { bucket.as_ref() }.value.get(Some(instance))
        }))
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
                        // Safety: the bucket stays live while the dict is borrowed
                        let bucket = unsafe { bucket.as_ref() };
                        tuple::create_pair(strand, &bucket.key, bucket.value.at(*subindex), out);
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
        dict.insert(
            strand,
            Slot::reborrow(&mut key),
            Slot::reborrow(&mut value),
            hv,
            false,
        )?;
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
            strand.store(&mut out, value);
            Ok(())
        } else if let Some(mut default) = default {
            strand.store(&mut out, &mut default);
            Ok(())
        } else if let Some(else_key) = or_else {
            call!(strand, else_key, out)
        } else {
            strand.store(&mut out, Value::NIL);
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
                                    EntryValue::Single { value, .. } => {
                                        strand.store(&mut out, value)
                                    }
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
                                strand.store(&mut out, value);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        if let Some(mut default) = default {
            strand.store(&mut out, &mut default);
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
        strand.store(out, deleted);
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
                        EntryValue::Single { value, .. } => value.eq(strand, &expected_value)?,
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
                                if v.eq(strand, &expected_value)? {
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
        strand.store(&mut out, Value::from_bool(found));
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
        strand.store(out, value);
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => items);
        // Spreading runs arbitrary code, so the pairs go straight into the allocated dict,
        // which is borrowed only for each insert
        strand
            .builtin_types()
            .dict
            .create(strand, Dict::new(), Slot::reborrow(&mut out));
        items
            .op_spread(strand, SpreadContext::Pairs, &mut DictPairs { dict: &out })
            .await
    }

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
                    self_val.op_fill(
                        strand,
                        &strand.singletons().dict,
                        Slot::reborrow(&mut native),
                    )?;
                    Ok(())
                })
            }
            _ => type_mcall_fallback(strand, &strand.singletons().dict, method, args, out).await,
        }
    }
}
