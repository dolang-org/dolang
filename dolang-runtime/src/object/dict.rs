use std::{
    cell::{Cell, RefCell},
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    ops::ControlFlow,
};

use bitvec::{bitbox, boxed::BitBox};
use dolang_util::hashbrown::raw::{Bucket, RawTable};

use crate::{
    arg::{Arg, Args},
    bytecode::{Rest, Variadic},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::protocol::members,
    sig::{self, UnpackKeyKind},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{
        Output, Slot, Slots, TypeObject, Value,
        fmt::{Format, Spec},
    },
    vm::Vm,
};

use super::{
    BoundMethod, index, iter,
    protocol::{
        GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, instance_mcall_fallback,
        is_special_mcall, type_mcall_fallback,
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
    const IMMUTABLE: bool = false;
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
        let mut hasher = DefaultHasher::new();
        index.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        Ok(self
            .table
            .find(hash, eq(strand, index))
            .and_then(|pair| unsafe {
                let pair = pair.as_ref();
                let instance = match instance {
                    Some(instance) => Some(index::element(pair.value.len(), instance)?),
                    None => None,
                };
                pair.value.get(instance)
            }))
    }

    /// Finds the key of the first pair, in insertion order, that an unpack
    /// left behind.
    ///
    /// A pair is accounted for if it is one of the integer keys below
    /// `int_limit` consumed positionally, or if `skip` already covers that
    /// many instances of its key. `skip` is copied rather than advanced,
    /// since the caller still needs it as it stands (to commit, or to hand to
    /// a capture).
    fn leftover_key<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        int_limit: i64,
        skip: &Skip<'v>,
    ) -> Result<'v, 's, Option<Value<'v>>> {
        let mut skip = skip.clone();
        for (bucket, subindex) in self.index.iter().flatten() {
            let bucket = unsafe { bucket.as_ref() };
            let key = &bucket.key;
            if *subindex == 0
                && let Some(int) = key.as_int(strand)
                && (0..i128::from(int_limit)).contains(&int)
            {
                continue;
            }
            if skip.add(strand, key, bucket.hash) >= bucket.value.len() {
                continue;
            }
            return Ok(Some(key.dup()));
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

    fn rehash(&mut self, strand: &mut Strand<'v, '_>, old_capacity: usize) -> usize {
        let capacity = 1.max(old_capacity * 2);
        let total_pairs = self.total_pairs;
        let mut this = Self {
            table: RawTable::with_capacity(capacity),
            index: Vec::new(),
            epoch: self.epoch + 1,
            total_pairs: 0,
        };
        for mut bucket in self.index.drain(..) {
            if let Some((bucket, subindex)) = bucket.take() {
                if let EntryValue::Single { .. } = unsafe { &bucket.as_ref().value } {
                    let (mut entry, _) = unsafe { self.table.remove(bucket) };
                    let (i, slot) = Self::next_index(&mut this.index, capacity);
                    match &mut entry.value {
                        EntryValue::Single { index, .. } => *index = i,
                        EntryValue::Multi(_) => unreachable!(),
                    };
                    *slot = Some((this.table.insert(entry.hash, entry, hasher()), 0));
                    this.total_pairs += 1;
                } else {
                    let bucket = unsafe { &bucket.as_ref() };
                    this.insert(
                        strand,
                        bucket.key.dup(),
                        bucket.value.at(subindex).dup(),
                        bucket.hash,
                        false,
                    )
                }
            }
        }
        debug_assert_eq!(this.total_pairs, total_pairs);
        mem::swap(self, &mut this);
        capacity
    }

    pub(crate) fn insert<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Value<'v>,
        value: Value<'v>,
        hv: u64,
        unique: bool,
    ) {
        unsafe {
            let mut cap = self.table.capacity();
            if self.table.len() == cap {
                cap = self.rehash(strand, cap)
            }
            match self
                .table
                .find_or_find_insert_index(hv, eq(strand, &key), hasher())
            {
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
    }

    pub(crate) fn from_args<'s>(
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut sink = DictPairs {
            int: 0,
            dict: &mut this,
        };

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

    pub(crate) async fn from_builtin_args<'s>(
        strand: &mut Strand<'v, 's>,
        mut args: Args<'v, '_>,
    ) -> Result<'v, 's, Self> {
        let mut this = Self::new();
        let mut counter = 1;
        let mut index = 0;

        let mut sink = DictPairs {
            int: 0,
            dict: &mut this,
        };

        loop {
            if counter % crate::INTERRUPT_INTERVAL == 0 {
                strand.check_trap_gc()?
            }
            counter += 1;
            let mut key = match args.next() {
                Some(Arg::Pos(key)) => key,
                Some(Arg::Key(sym, mut value)) if sym.tag() == sym::INT => {
                    let key = Value::from_i64(strand, index);
                    let mut hasher = DefaultHasher::new();
                    key.op_hash(strand, &mut hasher).unwrap();
                    let hv = hasher.finish();
                    sink.dict.insert(
                        strand,
                        Value::from_i64(strand, index),
                        value.take(),
                        hv,
                        false,
                    );
                    index += 1;
                    continue;
                }
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
            let mut hasher = DefaultHasher::new();
            key.op_hash(strand, &mut hasher)?;
            let hv = hasher.finish();
            sink.dict
                .insert(strand, key.take(), value.take(), hv, false)
        }

        Ok(this)
    }
}

// ── Hashing ─────────────────────────────────────────────────────────

fn hasher<'v>() -> impl Fn(&Entry<'v>) -> u64 {
    |Entry { hash, .. }| *hash
}

fn eq<'v, 's>(strand: &mut Strand<'v, 's>, needle: &Value<'v>) -> impl FnMut(&Entry<'v>) -> bool {
    |Entry { key, .. }| needle.eq(strand, key)
}

struct DictPairs<'b, 'v> {
    int: i64,
    dict: &'b mut Dict<'v>,
}

impl<'b, 'v, 's> Spread<'v, 's> for DictPairs<'b, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let key = Value::from_i64(strand, self.int);
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher).unwrap();
        let hv = hasher.finish();
        self.dict.insert(strand, key, value.take(), hv, false);
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
        let key = Value::from_object(strand.sym_obj(key));
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher).unwrap();
        let hv = hasher.finish();
        self.dict.insert(strand, key, value.take(), hv, false);
        Ok(())
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut key: Slot<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        self.dict
            .insert(strand, key.take(), value.take(), hv, false);
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
    const IMMUTABLE: bool = false;
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
        Err(Error::not_supported(strand))
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
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        Dict::iter_op_spread(
            &borrow.index,
            borrow.epoch,
            &borrow.dict,
            strand,
            context,
            sink,
        )
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
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

unsafe impl<'v> Collect for Keys<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

unsafe impl<'v> Collect for KeyValues<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Dict<'v> {
    /// Spreads one pair. It is positional if its key is the integer `next_pos`,
    /// which then counts up; `None` spreads every pair as keyed.
    fn spread_key_value<'s>(
        strand: &mut Strand<'v, 's>,
        next_pos: &mut Option<i64>,
        mut key: Value<'v>,
        mut value: Value<'v>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        if context == SpreadContext::Sequence {
            value = Value::from_object(tuple::tuple(strand, [key, value]));
            sink.positional(strand, Slot::new(&mut value))
        } else if let Some(pos) = *next_pos
            && key.to_i64(strand).ok() == Some(pos)
        {
            *next_pos = pos.checked_add(1);
            sink.positional(strand, Slot::new(&mut value))
        } else if let Some(sym) = key.as_sym(strand) {
            sink.symbol(strand, sym, Slot::new(&mut value))
        } else {
            sink.keyed(strand, Slot::new(&mut key), Slot::new(&mut value))
        }
    }

    fn with_iter_inner<'s, R>(
        container: &GcObj<'v, Dict<'v>>,
        epoch: u64,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&Dict<'v>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
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

    fn iter_unpack_values<'s>(
        strand: &mut Strand<'v, 's>,
        sig: &sig::Unpack<'v, '_>,
        out: &mut Slots<'v, '_>,
        mut index: usize,
        mut next: impl FnMut(usize) -> Option<(usize, Value<'v>)>,
    ) -> Result<'v, 's, usize> {
        let pos_count = sig.required + sig.optional.len();
        for i in 0..(pos_count + sig.keys.len()) {
            if i < pos_count {
                if let Some((next_index, value)) = next(index) {
                    out.at(i).store(value);
                    index = next_index;
                } else if i >= sig.required {
                    out.at(i).store(sig.optional[i - sig.required].dup());
                } else {
                    return Err(Error::missing_positional(strand, i));
                }
            } else {
                let key = &sig.keys[i - pos_count];
                if let Some(default) = &key.default {
                    out.at(i).store(default.dup());
                } else {
                    return Err(match &key.kind {
                        UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                        UnpackKeyKind::Const(value) => Error::missing_key(strand, value),
                    });
                }
            }
        }
        if sig.pos_rest() == Rest::None && next(index).is_some() {
            return Err(Error::unexpected_positional(strand, pos_count));
        }
        Ok(index)
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

    fn iter_op_spread<'s>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        strand: &mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let dict = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        if dict.epoch != epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        let mut next_pos = Some(0i64);
        loop {
            let Some(bucket) = dict.index.get(index.get()) else {
                return Ok(());
            };
            index.update(|i| i + 1);
            if let Some((bucket, subindex)) = bucket {
                let bucket = unsafe { bucket.as_ref() };
                Self::spread_key_value(
                    strand,
                    &mut next_pos,
                    bucket.key.dup(),
                    bucket.value.at(*subindex).dup(),
                    context,
                    sink,
                )?;
            }
        }
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
            let dict = borrow
                .container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != borrow.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            Dict::iter_unpack_values(strand, sig, &mut out, borrow.index.get(), |i| {
                Dict::next_value_from_index(&dict, i)
            })?
        };
        borrow.index.set(next_index);
        if let Some(i) = sig.pos_rest_slot() {
            out.at(i).store(Value::from_input(strand, &this))
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
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
        {
            let dict = borrow
                .container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != borrow.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
        }
        let next_index =
            Dict::iter_unpack_values(strand, sig, &mut out, borrow.index.get(), |i| {
                Dict::next_value_from_bucket(borrow.bucket.clone(), i)
            })?;
        borrow.index.set(next_index);
        if let Some(i) = sig.pos_rest_slot() {
            out.at(i).store(Value::from_input(strand, &this))
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
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
        let (next_index, pending) = {
            let dict = borrow
                .container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            if dict.epoch != borrow.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            let visited = borrow.visited.borrow();
            let mut pending = Vec::new();
            let next_index =
                Dict::iter_unpack_values(strand, sig, &mut out, borrow.index.get(), |i| {
                    Dict::next_key_from_index(&dict, i, &visited, &mut pending)
                })?;
            (next_index, pending)
        };
        borrow.index.set(next_index);
        {
            let mut visited = borrow.visited.borrow_mut();
            for bucket_index in pending {
                visited.set(bucket_index, true);
            }
        }
        if let Some(i) = sig.pos_rest_slot() {
            out.at(i).store(Value::from_input(strand, &this))
        }
        sig.fill_empty_key_rest(strand, &mut out);
        Ok(())
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

    fn add<'s>(&mut self, strand: &mut Strand<'v, 's>, value: &Value<'v>, hv: u64) -> usize {
        self.count += 1;
        unsafe {
            match self.table.find_or_find_insert_index(
                hv,
                |s| value.op_eq(strand, &s.value).to_bool(strand),
                |s| s.hash,
            ) {
                Ok(bucket) => {
                    let bucket = bucket.as_mut();
                    let count = bucket.count;
                    bucket.count += 1;
                    count
                }
                Err(index) => {
                    self.table.insert_at_index(
                        hv,
                        index,
                        Seen {
                            value: value.dup(),
                            hash: hv,
                            count: 1,
                        },
                    );
                    0
                }
            }
        }
    }

    fn take(&mut self) -> Self {
        mem::replace(self, Skip::new())
    }
}

/// How far [`Dict::unpack_matched`] got.
struct Matched {
    /// First integer key after those taken positionally, if any remain.
    start: Option<i64>,
    /// End of the run of leftover positional items from `start`.
    end: Option<i64>,
    /// First instances of integer keys below this were consumed beforehand.
    floor: i64,
}

impl Matched {
    /// Returns the floor for a `**name` rest: it takes the leftover
    /// positional items too, as keyed ones, unless a `*` rest takes them.
    fn key_floor(&self, sig: &sig::Unpack<'_, '_>) -> i64 {
        if sig.pos_rest() == Rest::None {
            self.start.unwrap_or(self.floor)
        } else {
            self.end.unwrap_or(self.floor)
        }
    }
}

fn offset<'v, 's>(strand: &mut Strand<'v, 's>, base: i64, by: usize) -> Result<'v, 's, i64> {
    i64::try_from(by)
        .ok()
        .and_then(|by| base.checked_add(by))
        .ok_or_else(|| Error::overflow(strand))
}

/// Returns which of `entry`'s values a walk over leftover pairs delivers when
/// it reaches the one at `subindex`, or `None` if that visit delivers nothing.
///
/// Values go out in order, skipping those consumed: the first instance of an
/// integer key below `floor` (taken positionally), then as many more as
/// `skip` counts.
fn leftover_instance<'v>(
    strand: &mut Strand<'v, '_>,
    entry: &Entry<'v>,
    subindex: usize,
    floor: i64,
    skip: &mut Skip<'v>,
) -> Option<usize> {
    let positional = entry
        .key
        .as_int(strand)
        .is_some_and(|int| (0..i128::from(floor)).contains(&int));
    if positional && subindex == 0 {
        return None;
    }
    let instance = skip.add(strand, &entry.key, entry.hash) + usize::from(positional);
    (instance < entry.value.len()).then_some(instance)
}

/// Position of a lazy rest over a keyed container's leftover pairs.
///
/// A rest delivers its positional items, the run of integer keys from the
/// next position, before its keyed ones, which follow insertion order.
enum UnpackState<'v> {
    /// Delivering the positional run from `int`, before resuming the keyed
    /// items at `resume`.
    Int {
        int: i64,
        resume: usize,
        skip: Skip<'v>,
    },
    /// Walking in insertion order while the integer keys met continue the
    /// positional run from `int`.
    Order {
        int: i64,
        index: usize,
        skip: Skip<'v>,
    },
    /// Delivering the keyed items from `index`. First instances of integer
    /// keys below `floor` were consumed positionally.
    Resume {
        index: usize,
        floor: i64,
        skip: Skip<'v>,
    },
}

impl<'v> UnpackState<'v> {
    /// Returns the next leftover pair, advancing past it.
    fn next_pair<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        dict: &Dict<'v>,
    ) -> Result<'v, 's, Option<(Value<'v>, Value<'v>)>> {
        loop {
            match self {
                UnpackState::Int { int, resume, skip } => {
                    let key = Value::from_i64(strand, *int);
                    if let Some(value) = dict.get(strand, &key, Some(0))? {
                        let value = value.dup();
                        *int = offset(strand, *int, 1)?;
                        return Ok(Some((key, value)));
                    }
                    *self = UnpackState::Resume {
                        index: *resume,
                        floor: *int,
                        skip: skip.take(),
                    };
                }
                UnpackState::Resume { index, floor, skip } => {
                    let Some(slot) = dict.index.get(*index) else {
                        return Ok(None);
                    };
                    *index += 1;
                    let Some((bucket, subindex)) = slot else {
                        continue;
                    };
                    let entry = unsafe { bucket.as_ref() };
                    if let Some(instance) =
                        leftover_instance(strand, entry, *subindex, *floor, skip)
                    {
                        return Ok(Some((entry.key.dup(), entry.value.at(instance).dup())));
                    }
                }
                UnpackState::Order { int, index, skip } => {
                    let Some(slot) = dict.index.get(*index) else {
                        return Ok(None);
                    };
                    let Some((bucket, subindex)) = slot else {
                        *index += 1;
                        continue;
                    };
                    let entry = unsafe { bucket.as_ref() };
                    if let Some(key) = entry.key.as_int(strand) {
                        if *subindex == 0 && key == i128::from(*int) {
                            *index += 1;
                            *int = offset(strand, *int, 1)?;
                            return Ok(Some((entry.key.dup(), entry.value.at(0).dup())));
                        }
                        // Out of sequence: finish the run, then resume here
                        *self = UnpackState::Int {
                            int: *int,
                            resume: *index,
                            skip: skip.take(),
                        };
                        continue;
                    }
                    *index += 1;
                    if let Some(instance) = leftover_instance(strand, entry, *subindex, *int, skip)
                    {
                        return Ok(Some((entry.key.dup(), entry.value.at(instance).dup())));
                    }
                }
            }
        }
    }
}

// ── Unpack ──────────────────────────────────────────────────────────

/// Lazy rest over a dict's leftover pairs.
pub(crate) struct Unpack<'v> {
    state: UnpackState<'v>,
    epoch: u64,
    dict: GcObj<'v, Dict<'v>>,
    /// Spreads every pair as keyed, for a `**name` rest.
    keyed: bool,
}

/// Creates the lazy rest over a dict's leftover pairs.
fn make_unpack<'v>(
    strand: &mut Strand<'v, '_>,
    dict: GcObj<'v, Dict<'v>>,
    epoch: u64,
    state: UnpackState<'v>,
    keyed: bool,
) -> Value<'v> {
    Value::from_object(GcObj::new(
        strand.arena(),
        strand.builtin_types().dict_unpack,
        Unpack {
            state,
            dict,
            epoch,
            keyed,
        },
    ))
}

unsafe impl<'v> Collect for Unpack<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.dict.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Unpack<'v> {
    fn check_epoch<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let dict = self
            .dict
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        if dict.epoch != self.epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        Ok(())
    }
}

impl<'v> Protocol<'v> for Unpack<'v> {
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
        crate::fmt!(strand, w, "<dict unpack iter>")
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
        borrow.check_epoch(strand)?;
        let container = borrow.dict.clone();
        let dict = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let (next, floor, resume, skip) = match &borrow.state {
            UnpackState::Int { int, resume, skip } => (Some(*int), *int, *resume, skip),
            UnpackState::Order { int, index, skip } => (Some(*int), *int, *index, skip),
            UnpackState::Resume { index, floor, skip } => (None, *floor, *index, skip),
        };

        // Match against a copy, so a failure leaves the rest as it was
        let mut skip = skip.clone();
        let matched = dict.unpack_matched(strand, sig, &mut out, next, floor, &mut skip)?;
        dict.store_pos_rest(strand, sig, &mut out, &matched)?;
        let key_rest = sig.key_rest_slot().map(|i| {
            let state = UnpackState::Resume {
                index: resume,
                floor: matched.key_floor(sig),
                skip: skip.clone(),
            };
            (i, state)
        });

        // Commit, consuming a positional run that a `*` rest took
        let next = if sig.variadic == Variadic::Capture || sig.pos_rest() == Rest::None {
            matched.start
        } else {
            matched.end
        };
        borrow.state = match next {
            Some(int) => UnpackState::Int { int, resume, skip },
            None => UnpackState::Resume {
                index: resume,
                floor,
                skip,
            },
        };
        let epoch = borrow.epoch;
        drop(dict);
        drop(borrow);

        if let Some((i, state)) = key_rest {
            let rest = make_unpack(strand, container, epoch, state, true);
            out.at(i).store(rest);
        } else if sig.variadic == Variadic::Capture {
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
        let container = borrow.dict.clone();
        let dict = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        if dict.epoch != borrow.epoch {
            return Err(Error::concurrency(strand));
        }
        match borrow.state.next_pair(strand, &dict)? {
            Some((key, value)) => {
                out.store(Value::from_object(tuple::tuple(strand, [key, value])));
                Ok(true)
            }
            None => Ok(false),
        }
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
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let container = borrow.dict.clone();
        if container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?
            .epoch
            != borrow.epoch
        {
            return Err(Error::concurrency(strand));
        }
        let mut next_pos = match &borrow.state {
            _ if borrow.keyed => None,
            UnpackState::Int { int, .. } | UnpackState::Order { int, .. } => Some(*int),
            UnpackState::Resume { .. } => None,
        };
        let mut counter = 0usize;
        loop {
            counter += 1;
            if counter.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            let pair = {
                let dict = container
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                borrow.state.next_pair(strand, &dict)?
            };
            let Some((key, value)) = pair else {
                return Ok(());
            };
            Dict::spread_key_value(strand, &mut next_pos, key, value, context, sink)?;
        }
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
        let mut next_int_key = Some(0);
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
                if let (Some(int_key), Some(expected)) =
                    (bucket.as_ref().key.to_i64(strand).ok(), next_int_key)
                {
                    if int_key == expected {
                        next_int_key = Some(expected + 1);
                        bucket.as_ref().value.at(subindex).op_debug(strand, w)?;
                        continue;
                    } else {
                        next_int_key = None;
                    }
                }
                if let Some(sym) = bucket.as_ref().key.downcast_ref(strand.builtin_types().sym) {
                    crate::fmt!(strand, w, "{}", sym.get().name)?;
                } else {
                    bucket.as_ref().key.op_debug(strand, w)?;
                }
                crate::fmt!(strand, w, ": ")?;
                bucket.as_ref().value.at(subindex).op_debug(strand, w)?;
            }
        }
        crate::fmt!(strand, w, "}}")
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let Ok(borrow) = this.borrow(strand) else {
            return true;
        };
        borrow.total_pairs != 0
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
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().dict) {
            other
        } else {
            return Ok(Value::FALSE);
        };
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        if left.table.len() != right.table.len() {
            return Ok(Value::from_bool(false));
        }
        unsafe {
            let mut i = 0usize;
            let mut j = 0usize;
            while i < left.index.len() {
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
                if l.hash != r.hash
                    || !l.key.op_eq(strand, &r.key).to_bool(strand)
                    || !l
                        .value
                        .at(subl)
                        .op_eq(strand, r.value.at(subr))
                        .to_bool(strand)
                {
                    return Ok(Value::FALSE);
                }
                i += 1;
                j += 1;
            }
        }
        Ok(Value::TRUE)
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let other = if let Some(other) = other.downcast_ref(strand.builtin_types().dict) {
            other
        } else {
            return Err(Error::not_supported(strand));
        };
        let left = this.borrow(strand)?;
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
                if l.key.op_lt(strand, &r.key)?.to_bool(strand) {
                    return Ok(Value::TRUE);
                }
                if l.value
                    .at(subl)
                    .op_lt(strand, r.value.at(subr))?
                    .to_bool(strand)
                {
                    return Ok(Value::TRUE);
                }
                i += 1;
                j += 1;
            }
            if right.table.len() > left.table.len() {
                return Ok(Value::TRUE);
            }
            Ok(Value::FALSE)
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut hasher = DefaultHasher::new();
        index.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let dict = this.borrow(strand)?;
        match dict.table.find(hv, eq(strand, index)) {
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
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let mut borrow = this.borrow_mut(strand)?;
        borrow.insert(strand, key.take(), value.take(), hv, true);
        borrow.epoch += 1;
        Ok(())
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CLEAR => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Self::mcall_clear(this, strand)
            }
            sym::INSERT => {
                let ([key, value], []) = unpack!(strand, args, 2, 0)?;
                Self::mcall_insert(this, strand, key, value)
            }
            sym::GET => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([key], [subindex, default, or_else]) =
                    unpack!(strand, args, 1, 1, default = None, else_key = None)?;
                Self::mcall_get(this, strand, key, subindex, default, or_else, out).await
            }
            sym::POP => {
                let default = Sym::well_known(sym::DEFAULT);
                let else_key = Sym::well_known(sym::ELSE);
                let ([key], [subindex, default, or_else]) =
                    unpack!(strand, args, 1, 1, default = None, else_key = None)?;
                Self::mcall_pop(this, strand, key, subindex, default, or_else, out).await
            }
            sym::DELETE => {
                let ([key], _) = unpack!(strand, args, 1, 0)?;
                Self::mcall_delete(this, strand, key, out)
            }
            sym::PAIRS => {
                let _ = unpack!(strand, args, 0, 0)?;
                Self::op_iter(this, strand, out).await
            }
            sym::KEYS => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
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
                let ([], [key]) = unpack!(strand, args, 0, 1)?;
                let epoch = this.borrow(strand)?.epoch;
                let dict = this.to_strong();
                let value = if let Some(key) = key {
                    let mut hasher = DefaultHasher::new();
                    key.op_hash(strand, &mut hasher)?;
                    let hv = hasher.finish();
                    let bucket = this.borrow(strand)?.table.find(hv, eq(strand, &key));
                    Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().dict_key_values,
                        KeyValues {
                            index: Cell::new(0),
                            epoch,
                            container: dict,
                            bucket,
                        },
                    ))
                } else {
                    Value::from_object(GcObj::new(
                        strand.arena(),
                        strand.builtin_types().dict_values,
                        Values {
                            index: Cell::new(0),
                            epoch,
                            container: dict,
                        },
                    ))
                };
                out.store(value);
                Ok(())
            }
            sym::COUNT => {
                let ([], [key]) = unpack!(strand, args, 0, 1)?;
                Self::mcall_count(this, strand, key, out)
            }
            sym::COPY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if this.delegator().is_some() {
                    return strand
                        .with_slots(async |strand, [mut receiver, mut ty]| {
                            Output::set(strand, Slot::reborrow(&mut receiver), &this);
                            receiver.op_type(strand, Slot::reborrow(&mut ty));
                            call!(strand, &ty, out, &receiver).await
                        })
                        .await;
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
                    );
                }
                strand.builtin_types().dict.create(strand, dict, out);
                Ok(())
            }
            sym::CONTAINS => {
                let ([key], [value]) = unpack!(strand, args, 1, 1)?;
                Self::mcall_contains(this, strand, key, value, out)
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "dict.len is a field, not a method",
            )),
            _ if is_special_mcall(method.tag()) => {
                instance_mcall_fallback(strand, &this, method, args, out)
                    .await
                    .expect("supported special method")
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
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        let mut next_pos = Some(0i64);
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
                Self::spread_key_value(
                    strand,
                    &mut next_pos,
                    bucket.key.dup(),
                    bucket.value.at(*subindex).dup(),
                    context,
                    sink,
                )?;
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
        let matched = dict.unpack_matched(strand, sig, &mut out, Some(0), 0, &mut skip)?;
        dict.store_pos_rest(strand, sig, &mut out, &matched)?;
        let epoch = dict.epoch;
        drop(dict);
        if let Some(i) = sig.key_rest_slot() {
            let state = UnpackState::Resume {
                index: 0,
                floor: matched.key_floor(sig),
                skip,
            };
            let rest = make_unpack(strand, this.to_strong(), epoch, state, true);
            out.at(i).store(rest);
        } else if sig.variadic == Variadic::Capture {
            let state = UnpackState::Order {
                int: matched.start.unwrap_or(i64::MAX),
                index: 0,
                skip,
            };
            let rest = make_unpack(strand, this.to_strong(), epoch, state, false);
            out.at(sig.len() - 1).store(rest);
        }
        Ok(())
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
        let mut next_int_key = Some(0);
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
                let implicit = if let (Some(int_key), Some(expected)) =
                    (bucket.key.to_i64(strand).ok(), next_int_key)
                {
                    if int_key == expected {
                        next_int_key = Some(expected + 1);
                        true
                    } else {
                        next_int_key = None;
                        false
                    }
                } else {
                    false
                };
                if !implicit {
                    if let Some(symbol) = bucket.key.downcast_ref(strand.builtin_types().sym) {
                        rendered.push_str(&symbol.get().name);
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
                }
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
    /// leftovers of a kind that no rest takes.
    ///
    /// The positional items are the first instances of the integer keys
    /// counting up from `next`, or none if it is `None`. Those of integer keys
    /// below `floor` are already consumed. `skip` counts the instances of
    /// each key consumed so far, and is advanced past those the key items
    /// take.
    fn unpack_matched<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        sig: &sig::Unpack<'v, '_>,
        out: &mut Slots<'v, '_>,
        next: Option<i64>,
        floor: i64,
        skip: &mut Skip<'v>,
    ) -> Result<'v, 's, Matched> {
        let pos_count = sig.required + sig.optional.len();
        let mut taken = 0usize;
        if let Some(next) = next {
            while taken < pos_count {
                let key = offset(strand, next, taken)?;
                let Some(value) = self.get(strand, &Value::from_i64(strand, key), Some(0))? else {
                    break;
                };
                out.at(taken).store(value.dup());
                taken += 1;
            }
        }
        if taken < sig.required {
            return Err(Error::missing_positional(strand, taken));
        }
        for i in taken..pos_count {
            out.at(i).store(sig.optional[i - sig.required].dup());
        }

        // The leftover positional items run from `start` to `end`
        let start = next.map(|next| offset(strand, next, taken)).transpose()?;
        let mut end = start;
        if let Some(end) = &mut end {
            while self
                .get(strand, &Value::from_i64(strand, *end), Some(0))?
                .is_some()
            {
                *end = offset(strand, *end, 1)?;
            }
        }

        for (i, key) in sig.keys.iter().enumerate() {
            let key_value = match &key.kind {
                UnpackKeyKind::Sym(sym) => Value::from_object(strand.sym_obj(*sym)),
                UnpackKeyKind::Const(value) => value.dup(),
            };

            let mut hasher = DefaultHasher::new();

            key_value.op_hash(strand, &mut hasher)?;

            let hv = hasher.finish();
            let seen = skip.add(strand, &key_value, hv);

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

        let matched = Matched { start, end, floor };
        let leftover_floor = match (sig.pos_rest(), sig.key_rest()) {
            (Rest::None, Rest::None) => {
                if start != end {
                    return Err(Error::unexpected_positional(strand, pos_count));
                }
                Some(start.unwrap_or(floor))
            }
            (_, Rest::None) => Some(end.unwrap_or(floor)),
            _ => None,
        };
        if let Some(leftover_floor) = leftover_floor
            && let Some(key) = self.leftover_key(strand, leftover_floor, skip)?
        {
            return Err(Error::unexpected_key(strand, &key));
        }
        Ok(matched)
    }

    /// Stores the leftover positional items in a `*name` rest, as a tuple.
    fn store_pos_rest<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        sig: &sig::Unpack<'v, '_>,
        out: &mut Slots<'v, '_>,
        matched: &Matched,
    ) -> Result<'v, 's, ()> {
        if sig.variadic == Variadic::Capture {
            return Ok(());
        }
        let Some(i) = sig.pos_rest_slot() else {
            return Ok(());
        };
        let mut values = Vec::new();
        if let (Some(start), Some(end)) = (matched.start, matched.end) {
            for key in start..end {
                let key = Value::from_i64(strand, key);
                if let Some(value) = self.get(strand, &key, Some(0))? {
                    values.push(value.dup());
                }
            }
        }
        out.at(i)
            .store(Value::from_object(tuple::tuple(strand, values)));
        Ok(())
    }

    fn iter_op_next<'a, 's>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, Dict<'v>>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
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
                    out.store(Value::from_object(tuple::tuple(strand, unsafe {
                        [
                            bucket.as_ref().key.dup(),
                            bucket.as_ref().value.at(*subindex).dup(),
                        ]
                    })));
                    Ok(true)
                } else {
                    continue;
                }
            } else {
                Ok(false)
            };
        }
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
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let mut dict = this.borrow_mut(strand)?;
        dict.insert(strand, key.take(), value.take(), hv, false);
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
            call!(strand, else_key, out).await
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
        let mut hasher = DefaultHasher::new();
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
            if let Some(bucket) = dict.table.find(hv, eq(strand, &key)) {
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
            call!(strand, else_key, out).await
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
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let mut dict = this.borrow_mut(strand)?;
        let mut deleted = false;
        if let Some(bucket) = dict.table.find(hv, eq(strand, &key)) {
            unsafe {
                dict.total_pairs -= bucket.as_ref().value.len();
                match &bucket.as_ref().value {
                    EntryValue::Single { index, .. } => {
                        *dict.index.get_unchecked_mut(*index) = None;
                    }
                    EntryValue::Multi(items) => {
                        for (_, index) in items.iter() {
                            *dict.index.get_unchecked_mut(*index) = None;
                        }
                    }
                }
                dict.table.erase(bucket)
            }
            dict.epoch += 1;
            deleted = true;
        }
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
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let dict = this.borrow(strand)?;
        let found = match dict.table.find(hv, eq(strand, &key)) {
            None => false,
            Some(bucket) => {
                if let Some(expected_value) = value {
                    let bucket_ref = unsafe { bucket.as_ref() };
                    match &bucket_ref.value {
                        EntryValue::Single { value, .. } => {
                            value.op_eq(strand, &expected_value).to_bool(strand)
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
                                if v.op_eq(strand, &expected_value).to_bool(strand) {
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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let dict = this.borrow(strand)?;
        let count = if let Some(key) = key {
            let mut hasher = DefaultHasher::new();
            key.op_hash(strand, &mut hasher)?;
            let hv = hasher.finish();
            dict.table
                .find(hv, eq(strand, &key))
                .map(|bucket| unsafe { bucket.as_ref().value.len() })
                .unwrap_or(0)
        } else {
            dict.table.len()
        };
        let value = i64::try_from(count).map_err(|_| Error::overflow(strand))?;
        out.store(Value::from_i64(strand, value));
        Ok(())
    }
}

// ── Dict Class ──────────────────────────────────────────────────

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
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([items], []) = unpack!(strand, args, 1, 0)?;
        let mut dict = Dict::new();

        // FIXME: `Dict` is not GC-scannable, but then again if it were it would also
        // be mutably borrowed, which would inhibit GC.  This needs a resolution.
        let mut sink = DictPairs {
            int: 0,
            dict: &mut dict,
        };
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
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
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
                let ([self_val, items], []) = unpack!(strand, args, 2, 0)?;
                strand
                    .with_slots(async |strand, [mut native]| {
                        call!(strand, &strand.singletons().dict, &mut native, items).await?;
                        self_val.op_fill(strand, &strand.singletons().dict, native.take())?;
                        Ok(())
                    })
                    .await
            }
            _ => type_mcall_fallback(strand, &strand.singletons().dict, method, args, out).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::ErrorKind,
        test_support::{args_from_slots, with_vm},
        value::Value,
    };

    use super::*;

    /// Populates `out` (which must be a GC-rooted slot, e.g. one from [`with_vm`]) with a
    /// freshly built `Dict` containing `pairs`. Values are never returned as bare,
    /// unrooted locals — every call site roots the dict for as long as it's needed by
    /// reading it back in place from the same slot (see `Slot::into_inner`), rather than
    /// duplicating it out into a separate Rust local.
    fn make_dict<'v, 's>(strand: &mut Strand<'v, 's>, pairs: &[(i64, i64)], out: Slot<'v, '_>) {
        let mut dict = Dict::new();
        for &(k, v) in pairs {
            let key = Value::from_i64(strand, k);
            let value = Value::from_i64(strand, v);
            let mut hasher = DefaultHasher::new();
            key.op_hash(strand, &mut hasher).unwrap();
            let hv = hasher.finish();
            dict.insert(strand, key, value, hv, false);
        }
        strand.builtin_types().dict.create(strand, dict, out);
    }

    fn total_pairs<'v>(strand: &mut Strand<'v, '_>, value: &Value<'v>) -> usize {
        strand
            .builtin_types()
            .dict
            .cast(value)
            .unwrap()
            .enter_sync(strand, |strand, recv| {
                recv.borrow(strand).unwrap().total_pairs
            })
    }

    #[test]
    fn dict_pairs_positional_overflow_errors() {
        with_vm(async |strand, [mut slot]| {
            let mut dict = Dict::new();
            let mut sink = DictPairs {
                int: i64::MAX,
                dict: &mut dict,
            };
            slot.store(Value::from_i64(strand, 0));
            let err = sink.positional(strand, slot).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Overflow);
        });
    }

    #[test]
    fn op_bool_treats_broken_borrow_as_true() {
        with_vm(async |strand, [mut slot]| {
            make_dict(strand, &[], Slot::reborrow(&mut slot));
            let value: &Value = &slot;
            let dict_type = strand.builtin_types().dict;
            dict_type
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv1| {
                    let _held = recv1.borrow_mut(strand).unwrap();
                    dict_type
                        .cast(value)
                        .unwrap()
                        .enter_sync(strand, |strand, recv2| {
                            assert!(Dict::op_bool(recv2, strand));
                        });
                });
        });
    }

    #[test]
    fn op_bool_false_when_empty_true_when_nonempty() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[], Slot::reborrow(&mut slot0));
            make_dict(strand, &[(1, 2)], Slot::reborrow(&mut slot1));
            let empty: &Value = &slot0;
            let nonempty: &Value = &slot1;
            let dict_type = strand.builtin_types().dict;
            dict_type
                .cast(empty)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(!Dict::op_bool(recv, strand));
                });
            dict_type
                .cast(nonempty)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    assert!(Dict::op_bool(recv, strand));
                });
        });
    }

    #[test]
    fn op_eq_and_op_lt_type_mismatch_are_asymmetric() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[], Slot::reborrow(&mut slot0));
            slot1.store(Value::from_i64(strand, 42));
            let value: &Value = &slot0;
            let other: &Value = &slot1;
            let dict_type = strand.builtin_types().dict;
            dict_type
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let eq = Dict::op_eq(recv, strand, other).unwrap();
                    assert!(!eq.to_bool(strand));
                });
            dict_type
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    match Dict::op_lt(recv, strand, other) {
                        Err(err) => assert_eq!(err.kind(), ErrorKind::Unsupported),
                        Ok(_) => panic!("expected op_lt to error on type mismatch"),
                    }
                });
        });
    }

    #[test]
    fn op_mcall_len_errors_as_field_not_method() {
        with_vm(async |strand, [mut slot0, slot1]| {
            make_dict(strand, &[(1, 2)], Slot::reborrow(&mut slot0));
            let value: &Value = &slot0;
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // `dict.len()` takes no arguments — a zero-length, separately
                    // rooted `Slots` backs the (empty) `Args`.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            match Dict::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::LEN),
                                args,
                                slot1,
                            )
                            .await
                            {
                                Err(err) => assert_eq!(err.kind(), ErrorKind::Type),
                                Ok(()) => panic!("expected dict.len() as a method call to error"),
                            }
                        })
                        .await;
                })
                .await;
        });
    }

    #[test]
    fn op_mcall_values_with_missing_key_yields_empty_key_values() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[(1, 2)], Slot::reborrow(&mut slot0));
            let value: &Value = &slot0;
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // `dict.values(key)` takes exactly the one positional key
                    // argument — a separately rooted, single-cell `Slots` backs it.
                    strand
                        .with_slots_dynamic(1, async |strand, mut arg_slots| {
                            arg_slots.at(0).store(Value::from_i64(strand, 999));
                            let sig = [None];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            Dict::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::VALUES),
                                args,
                                Slot::reborrow(&mut slot1),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                })
                .await;
            let out: &Value = &slot1;
            assert!(
                out.downcast_ref(strand.builtin_types().dict_key_values)
                    .is_some()
            );
        });
    }

    #[test]
    fn op_mcall_copy_is_independent_of_original() {
        with_vm(async |strand, [mut slot0, mut slot1]| {
            make_dict(strand, &[(1, 2), (3, 4)], Slot::reborrow(&mut slot0));
            let value: &Value = &slot0;
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    // `dict.copy()` takes no arguments — a zero-length, separately
                    // rooted `Slots` backs the (empty) `Args`.
                    strand
                        .with_slots_dynamic(0, async |strand, mut arg_slots| {
                            let sig: [Option<Sym>; 0] = [];
                            let args = args_from_slots(&mut arg_slots, &sig, 0);
                            Dict::op_mcall(
                                recv,
                                strand,
                                Sym::well_known(sym::COPY),
                                args,
                                Slot::reborrow(&mut slot1),
                            )
                            .await
                            .unwrap();
                        })
                        .await;
                })
                .await;

            let copy: &Value = &slot1;
            assert_eq!(total_pairs(strand, copy), 2);

            // Mutate the original after copying; the copy must be unaffected.
            strand
                .builtin_types()
                .dict
                .cast(value)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let key = Value::from_i64(strand, 5);
                    let val = Value::from_i64(strand, 6);
                    let mut hasher = DefaultHasher::new();
                    key.op_hash(strand, &mut hasher).unwrap();
                    let hv = hasher.finish();
                    recv.borrow_mut(strand)
                        .unwrap()
                        .insert(strand, key, val, hv, false);
                });

            assert_eq!(total_pairs(strand, copy), 2);
        });
    }
}
