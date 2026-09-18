use std::{
    cell::{Cell, RefCell},
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    ops::ControlFlow,
};

use crate::value::fmt::{Format, Spec};

use bitvec::boxed::BitBox;

use crate::{
    bytecode::{Rest, Variadic},
    call,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    sig::{self, UnpackKeyKind},
    strand::Strand,
    sym::{self, Sym, Tag},
    value::{Output, Slot, Slots, Value},
};

use super::protocol::{GcObj, GcObjBorrow, Protocol, Recv, Spread, SpreadContext};
use super::tuple;
use super::{index, iter};

use dolang_util::hashbrown::raw::{Bucket, RawTable};

// ── Shared entry types ──────────────────────────────────────────────

pub(crate) enum EntryValue<'v> {
    Single { value: Value<'v>, index: usize },
    Multi(Vec<(Value<'v>, usize)>),
}

impl<'v> EntryValue<'v> {
    pub(crate) fn len(&self) -> usize {
        match self {
            EntryValue::Single { .. } => 1,
            EntryValue::Multi(items) => items.len(),
        }
    }

    pub(crate) fn get(&self, index: Option<usize>) -> Option<&Value<'v>> {
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

    pub(crate) fn at(&self, index: usize) -> &Value<'v> {
        self.get(Some(index)).unwrap()
    }

    /// The value a lookup without an instance resolves to.
    ///
    /// The most recently inserted one, so a duplicate key overrides the values
    /// before it: last wins.
    pub(crate) fn latest(&self) -> &Value<'v> {
        self.get(None).expect("entry with no values")
    }
}

pub(crate) struct Entry<'v> {
    pub(crate) key: Value<'v>,
    pub(crate) value: EntryValue<'v>,
    pub(crate) hash: u64,
}

// ── Common inner data structure ─────────────────────────────────────

pub(crate) struct Inner<'v> {
    pub(crate) inner: RawTable<Entry<'v>>,
    pub(crate) index: Vec<Option<(Bucket<Entry<'v>>, usize)>>,
    pub(crate) epoch: u64,
    pub(crate) total_pairs: usize,
}

impl<'v> Inner<'v> {
    pub(crate) fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        unsafe {
            for bucket in self.inner.iter() {
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

    pub(crate) fn clear(&mut self) {
        self.inner.clear();
        self.total_pairs = 0;
    }
}

impl<'v> Inner<'v> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Default::default(),
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
        let hash = hash(strand, index)?;
        Ok(self
            .inner
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
    pub(crate) fn leftover_key<'s>(
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

    pub(crate) fn next_index<'a>(
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

    pub(crate) fn rehash(&mut self, strand: &mut Strand<'v, '_>, old_capacity: usize) -> usize {
        let capacity = 1.max(old_capacity * 2);
        let total_pairs = self.total_pairs;
        let mut this = Self {
            inner: RawTable::with_capacity(capacity),
            index: Vec::new(),
            epoch: self.epoch + 1,
            total_pairs: 0,
        };
        for mut bucket in self.index.drain(..) {
            if let Some((bucket, subindex)) = bucket.take() {
                if let EntryValue::Single { .. } = unsafe { &bucket.as_ref().value } {
                    let (mut entry, _) = unsafe { self.inner.remove(bucket) };
                    let (i, slot) = Self::next_index(&mut this.index, capacity);
                    match &mut entry.value {
                        EntryValue::Single { index, .. } => *index = i,
                        EntryValue::Multi(_) => unreachable!(),
                    };
                    *slot = Some((this.inner.insert(entry.hash, entry, hasher()), 0));
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
            let mut cap = self.inner.capacity();
            if self.inner.len() == cap {
                cap = self.rehash(strand, cap)
            }
            match self
                .inner
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
                        self.inner.insert_at_index(
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
}

// ── Free functions (private to crate) ────────────────────────────────

pub(crate) fn hash<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, u64> {
    let mut hasher = DefaultHasher::new();
    value.op_hash(strand, &mut hasher)?;
    Ok(hasher.finish())
}

pub(crate) fn hasher<'v>() -> impl Fn(&Entry<'v>) -> u64 {
    |Entry { hash, .. }| *hash
}

pub(crate) fn eq<'v, 's>(
    strand: &mut Strand<'v, 's>,
    needle: &Value<'v>,
) -> impl FnMut(&Entry<'v>) -> bool {
    |Entry { key, .. }| needle.eq(strand, key)
}

pub(crate) struct Values<'v, T: AsRef<Inner<'v>> + Collect + 'v> {
    pub(crate) index: Cell<usize>,
    pub(crate) epoch: u64,
    pub(crate) container: GcObj<'v, T>,
}

pub(crate) struct Keys<'v, T: AsRef<Inner<'v>> + Collect + 'v> {
    pub(crate) index: Cell<usize>,
    pub(crate) epoch: u64,
    pub(crate) container: GcObj<'v, T>,
    pub(crate) visited: RefCell<BitBox>,
}

pub(crate) struct KeyValues<'v, T: AsRef<Inner<'v>> + Collect + 'v> {
    pub(crate) index: Cell<usize>,
    pub(crate) epoch: u64,
    pub(crate) container: GcObj<'v, T>,
    pub(crate) bucket: Option<Bucket<Entry<'v>>>,
}

unsafe impl<'v, T: AsRef<Inner<'v>> + Collect + 'v> Collect for Values<'v, T> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

unsafe impl<'v, T: AsRef<Inner<'v>> + Collect + 'v> Collect for Keys<'v, T> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

unsafe impl<'v, T: AsRef<Inner<'v>> + Collect + 'v> Collect for KeyValues<'v, T> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.container.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Inner<'v> {
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

    fn with_iter_inner<'s, T: AsRef<Inner<'v>> + Collect + 'v, R>(
        container: &GcObj<'v, T>,
        epoch: u64,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&Inner<'v>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let container_borrow = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let inner: &Inner<'v> = (*container_borrow).as_ref();
        if inner.epoch != epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        f(inner)
    }

    fn next_value_from_index(inner: &Inner<'v>, index: usize) -> Option<(usize, Value<'v>)> {
        let mut index = index;
        loop {
            let bucket = inner.index.get(index)?;
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
        inner: &Inner<'v>,
        index: usize,
        visited: &BitBox,
        pending: &mut Vec<usize>,
    ) -> Option<(usize, Value<'v>)> {
        let mut index = index;
        loop {
            let entry = inner.index.get(index)?;
            index += 1;
            let Some((bucket, _)) = entry else {
                continue;
            };
            let bucket_index = unsafe { inner.inner.bucket_index(bucket) };
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

    pub(crate) fn values_iter_op_next<'s, T: AsRef<Inner<'v>> + Collect + 'v>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, T>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Self::with_iter_inner(container, epoch, strand, |inner| {
            if let Some((next_index, value)) = Self::next_value_from_index(inner, index.get()) {
                index.set(next_index);
                out.store(value);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    pub(crate) fn key_values_iter_op_next<'s, T: AsRef<Inner<'v>> + Collect + 'v>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, T>,
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

    pub(crate) fn keys_iter_op_next<'s, T: AsRef<Inner<'v>> + Collect + 'v>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, T>,
        visited: &RefCell<BitBox>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        Self::with_iter_inner(container, epoch, strand, |inner| {
            let mut visited = visited.borrow_mut();
            let mut pending = Vec::with_capacity(1);
            if let Some((next_index, key)) =
                Self::next_key_from_index(inner, index.get(), &visited, &mut pending)
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

    pub(crate) fn iter_op_spread<'s, T: AsRef<Inner<'v>> + Collect + 'v>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, T>,
        strand: &mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let container_borrow = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let inner: &Inner<'v> = (*container_borrow).as_ref();
        if inner.epoch != epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        let mut next_pos = Some(0i64);
        loop {
            let Some(bucket) = inner.index.get(index.get()) else {
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

impl<'v, T: AsRef<Inner<'v>> + Collect + 'v> Protocol<'v> for Values<'v, T> {
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
            let container_borrow = borrow
                .container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            let inner: &Inner<'v> = (*container_borrow).as_ref();
            if inner.epoch != borrow.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            Inner::iter_unpack_values(strand, sig, &mut out, borrow.index.get(), |i| {
                Inner::next_value_from_index(inner, i)
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
        Inner::values_iter_op_next(&borrow.index, borrow.epoch, &borrow.container, strand, out)
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

impl<'v, T: AsRef<Inner<'v>> + Collect + 'v> Protocol<'v> for KeyValues<'v, T> {
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
            let container_borrow = borrow
                .container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            let inner: &Inner<'v> = (*container_borrow).as_ref();
            if inner.epoch != borrow.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
        }
        let next_index =
            Inner::iter_unpack_values(strand, sig, &mut out, borrow.index.get(), |i| {
                Inner::next_value_from_bucket(borrow.bucket.clone(), i)
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
        Inner::key_values_iter_op_next(
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

impl<'v, T: AsRef<Inner<'v>> + Collect + 'v> Protocol<'v> for Keys<'v, T> {
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
            let container_borrow = borrow
                .container
                .borrow()
                .ok_or_else(|| Error::concurrency(strand))?;
            let inner: &Inner<'v> = (*container_borrow).as_ref();
            if inner.epoch != borrow.epoch {
                return Err(Error::concurrency_msg(
                    strand,
                    "collection was modified during iteration",
                ));
            }
            let visited = borrow.visited.borrow();
            let mut pending = Vec::new();
            let next_index =
                Inner::iter_unpack_values(strand, sig, &mut out, borrow.index.get(), |i| {
                    Inner::next_key_from_index(inner, i, &visited, &mut pending)
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
        Inner::keys_iter_op_next(
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

impl<'v> Inner<'v> {
    pub(crate) fn op_format_pretty<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        kind: crate::value::fmt::Kind,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner = (*borrow).as_ref();
        if inner.total_pairs == 0 {
            return crate::fmt!(strand, w, "{{}}");
        }
        crate::fmt!(strand, w, "{{\n")?;
        let mut next_int_key = Some(0);
        unsafe {
            for (index, entry) in inner.index.iter().enumerate() {
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

    pub(crate) fn op_debug<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
        open: &str,
        close: &str,
        separator: &str,
        // Written before `close` when the only entry is positional
        singleton: &str,
    ) -> Result<'v, 's, ()> {
        let this_borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "{open}")?;
        let mut index = 0usize;
        let mut next_int_key = Some(0);
        let mut count = 0usize;
        unsafe {
            while let Some(bucket) = (*this_borrow).as_ref().index.get(index) {
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
                    crate::fmt!(strand, w, "{separator}")?;
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
        // A lone positional entry
        if next_int_key == Some(1) && count == 1 {
            crate::fmt!(strand, w, "{singleton}")?;
        }
        crate::fmt!(strand, w, "{close}")
    }

    pub(crate) fn op_hash<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
        sym_tag: Tag,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        sym_tag.hash(hasher);
        unsafe {
            let mut i = 0usize;
            while i < (*borrow).as_ref().index.len() {
                let (elem, subindex) =
                    if let Some((elem, subindex)) = (*borrow).as_ref().index.get_unchecked(i) {
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

    pub(crate) fn op_eq<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        other: &GcObjBorrow<'v, '_, T>,
    ) -> Result<'v, 's, Value<'v>> {
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        if (*left).as_ref().inner.len() != (*right).as_ref().inner.len() {
            return Ok(Value::from_bool(false));
        }
        unsafe {
            let mut i = 0usize;
            let mut j = 0usize;
            while i < (*left).as_ref().index.len() {
                let l = if let Some(l) = (*left).as_ref().index.get_unchecked(i) {
                    l
                } else {
                    i += 1;
                    continue;
                };
                let r = if let Some(r) = (*right).as_ref().index.get_unchecked(j) {
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

    pub(crate) fn op_lt<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        other: &GcObjBorrow<'v, '_, T>,
    ) -> Result<'v, 's, Value<'v>> {
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        unsafe {
            let mut i = 0usize;
            let mut j = 0usize;
            while i < (*left).as_ref().index.len() && j < (*right).as_ref().index.len() {
                let l = if let Some(l) = (*left).as_ref().index.get_unchecked(i) {
                    l
                } else {
                    i += 1;
                    continue;
                };
                let r = if let Some(r) = (*right).as_ref().index.get_unchecked(j) {
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
            if (*right).as_ref().inner.len() > (*left).as_ref().inner.len() {
                return Ok(Value::TRUE);
            }
            Ok(Value::FALSE)
        }
    }

    pub(crate) fn op_index<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let hv = hash(strand, index)?;
        let borrow = this.borrow(strand)?;
        let inner: &Inner<'v> = (*borrow).as_ref();
        match inner.inner.find(hv, eq(strand, index)) {
            Some(pair) => {
                Output::set(strand, out, unsafe { pair.as_ref().value.latest() });
                Ok(())
            }
            None => Err(Error::index(strand)),
        }
    }

    pub(crate) fn op_assign<'a, 's, T: AsRef<Inner<'v>> + AsMut<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        mut key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let hv = hash(strand, &key)?;
        let mut borrow = this.borrow_mut(strand)?;
        let inner = borrow.as_mut();
        inner.insert(strand, key.take(), value.take(), hv, true);
        inner.epoch += 1;
        Ok(())
    }

    pub(crate) fn op_unpack<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
        make_unpack: impl FnOnce(
            &mut Strand<'v, 's>,
            GcObj<'v, T>,
            u64,
            UnpackState<'v>,
            bool,
        ) -> Value<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner: &Inner<'v> = (*borrow).as_ref();
        let mut skip = Skip::new();
        let matched = inner.unpack_matched(strand, sig, &mut out, Some(0), 0, &mut skip)?;
        inner.store_pos_rest(strand, sig, &mut out, &matched)?;
        let epoch = inner.epoch;
        drop(borrow);
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

            let hv = hash(strand, &key_value)?;
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

    pub(crate) fn iter_op_next<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        index: &Cell<usize>,
        epoch: u64,
        container: &GcObj<'v, T>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let container_borrow = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let inner: &Inner<'v> = (*container_borrow).as_ref();
        if inner.epoch != epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        loop {
            break if let Some(bucket) = inner.index.get(index.get()) {
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

    pub(crate) async fn op_spread<'a, 's, T: AsRef<Inner<'v>> + Protocol<'v> + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner: &Inner<'v> = (*borrow).as_ref();
        let mut next_pos = Some(0i64);
        unsafe {
            let mut i = 0usize;
            while i < inner.index.len() {
                let Some((bucket, subindex)) = inner.index.get_unchecked(i) else {
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

    pub(crate) fn mcall_clear<'a, 's>(
        this: Recv<'v, 'a, impl AsMut<Inner<'v>> + Protocol<'v>>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let inner = borrow.as_mut();
        inner.inner.clear();
        inner.index.clear();
        inner.total_pairs = 0;
        inner.epoch += 1;
        Ok(())
    }

    pub(crate) fn mcall_insert<'a, 's>(
        this: Recv<'v, 'a, impl AsMut<Inner<'v>> + Protocol<'v>>,
        strand: &'a mut Strand<'v, 's>,
        mut key: Slot<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let hv = hash(strand, &key)?;
        let mut borrow = this.borrow_mut(strand)?;
        let inner = borrow.as_mut();
        inner.insert(strand, key.take(), value.take(), hv, false);
        inner.epoch += 1;
        Ok(())
    }

    pub(crate) async fn mcall_get<'a, 's>(
        this: Recv<'v, 'a, impl AsRef<Inner<'v>> + Protocol<'v>>,
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
        if let Some(value) = (*this.borrow(strand)?)
            .as_ref()
            .get(strand, &key, subindex)?
        {
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

    pub(crate) async fn mcall_pop<'a, 's>(
        this: Recv<'v, 'a, impl AsMut<Inner<'v>> + Protocol<'v>>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        subindex: Option<Slot<'v, 'a>>,
        default: Option<Slot<'v, 'a>>,
        or_else: Option<Slot<'v, 'a>>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let hv = hash(strand, &key)?;
        let else_key = Sym::well_known(sym::ELSE);
        if default.is_some() && or_else.is_some() {
            return Err(Error::unexpected_key(strand, else_key));
        }
        let subindex = subindex
            .map(|s| s.to_i64(strand).map_err(|_| Error::index(strand)))
            .transpose()?;
        {
            let mut borrow = this.borrow_mut(strand)?;
            let inner = borrow.as_mut();
            if let Some(bucket) = inner.inner.find(hv, eq(strand, &key)) {
                unsafe {
                    match &mut bucket.as_mut().value {
                        EntryValue::Single { index, .. } => {
                            let subindex = match subindex {
                                Some(subindex) => index::element(1, subindex),
                                None => Some(0),
                            };
                            if subindex == Some(0) {
                                inner.total_pairs -= 1;
                                *inner.index.get_unchecked_mut(*index) = None;
                                let bucket = inner.inner.remove(bucket).0;
                                inner.epoch += 1;
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
                                inner.total_pairs -= 1;
                                let (value, index) = items.remove(subindex);
                                *inner.index.get_unchecked_mut(index) = None;
                                if items.is_empty() {
                                    inner.inner.remove(bucket);
                                } else {
                                    for i in subindex..items.len() {
                                        let (_, idx) = items.get_unchecked(i);
                                        inner.index.get_unchecked_mut(*idx).as_mut().unwrap().1 = i;
                                    }
                                }
                                inner.epoch += 1;
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

    pub(crate) fn mcall_delete<'a, 's, T: AsRef<Inner<'v>> + AsMut<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let hv = hash(strand, &key)?;
        let mut borrow = this.borrow_mut(strand)?;
        let inner = borrow.as_mut();
        let mut deleted = false;
        if let Some(bucket) = inner.inner.find(hv, eq(strand, &key)) {
            unsafe {
                inner.total_pairs -= bucket.as_ref().value.len();
                match &bucket.as_ref().value {
                    EntryValue::Single { index, .. } => {
                        *inner.index.get_unchecked_mut(*index) = None;
                    }
                    EntryValue::Multi(items) => {
                        for (_, index) in items.iter() {
                            *inner.index.get_unchecked_mut(*index) = None;
                        }
                    }
                }
                inner.inner.erase(bucket)
            }
            inner.epoch += 1;
            deleted = true;
        }
        Output::set(strand, out, deleted);
        Ok(())
    }

    pub(crate) fn mcall_contains<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        key: Slot<'v, 'a>,
        value: Option<Slot<'v, 'a>>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let hv = hash(strand, &key)?;
        let borrow = this.borrow(strand)?;
        let inner: &Inner<'v> = (*borrow).as_ref();
        let found = match inner.inner.find(hv, eq(strand, &key)) {
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

    pub(crate) fn mcall_count<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        key: Option<Slot<'v, 'a>>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner: &Inner<'v> = (*borrow).as_ref();
        let count = if let Some(key) = key {
            let hv = hash(strand, &key)?;
            inner
                .inner
                .find(hv, eq(strand, &key))
                .map(|bucket| unsafe { bucket.as_ref().value.len() })
                .unwrap_or(0)
        } else {
            inner.inner.len()
        };
        let value = i64::try_from(count).map_err(|_| Error::overflow(strand))?;
        out.store(Value::from_i64(strand, value));
        Ok(())
    }
}

// ── Unpack bookkeeping ──────────────────────────────────────────────

pub(crate) struct Seen<'v> {
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

pub(crate) struct Skip<'v> {
    pub(crate) table: RawTable<Seen<'v>>,
    pub(crate) count: usize,
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
    pub(crate) fn new() -> Self {
        Skip {
            table: RawTable::new(),
            count: 0,
        }
    }

    pub(crate) fn add<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
        hv: u64,
    ) -> usize {
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

    pub(crate) fn take(&mut self) -> Self {
        mem::replace(self, Skip::new())
    }
}

/// How far [`Inner::unpack_matched`] got.
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
pub(crate) enum UnpackState<'v> {
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
        inner: &Inner<'v>,
    ) -> Result<'v, 's, Option<(Value<'v>, Value<'v>)>> {
        loop {
            match self {
                UnpackState::Int { int, resume, skip } => {
                    let key = Value::from_i64(strand, *int);
                    if let Some(value) = inner.get(strand, &key, Some(0))? {
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
                    let Some(slot) = inner.index.get(*index) else {
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
                    let Some(slot) = inner.index.get(*index) else {
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

pub(crate) struct UnpackInner<'v, T: Protocol<'v> + AsRef<Inner<'v>> + AsMut<Inner<'v>>> {
    pub(crate) state: UnpackState<'v>,
    pub(crate) epoch: u64,
    pub(crate) kv: GcObj<'v, T>,
    /// Spreads every pair as keyed, for a `**name` rest.
    pub(crate) keyed: bool,
}

impl<'v, T: Protocol<'v> + AsRef<Inner<'v>> + AsMut<Inner<'v>>> UnpackInner<'v, T> {
    pub(crate) fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.kv.accept(visit)
    }

    pub(crate) fn clear(&mut self) {}

    fn check_epoch<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let container = self.kv.borrow().ok_or_else(|| Error::concurrency(strand))?;
        if (*container).as_ref().epoch != self.epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        Ok(())
    }

    pub(crate) async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, impl AsMut<Self> + Protocol<'v>>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
        make_unpack: impl FnOnce(
            &mut Strand<'v, 's>,
            GcObj<'v, T>,
            u64,
            UnpackState<'v>,
            bool,
        ) -> Value<'v>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let borrow = AsMut::<Self>::as_mut(&mut *borrow);
        borrow.check_epoch(strand)?;
        let container = borrow.kv.clone();
        let container_borrow = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        let inner: &Inner<'v> = (*container_borrow).as_ref();
        let (next, floor, resume, skip) = match &borrow.state {
            UnpackState::Int { int, resume, skip } => (Some(*int), *int, *resume, skip),
            UnpackState::Order { int, index, skip } => (Some(*int), *int, *index, skip),
            UnpackState::Resume { index, floor, skip } => (None, *floor, *index, skip),
        };

        // Match against a copy, so a failure leaves the rest as it was
        let mut skip = skip.clone();
        let matched = inner.unpack_matched(strand, sig, &mut out, next, floor, &mut skip)?;
        inner.store_pos_rest(strand, sig, &mut out, &matched)?;
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
        drop(container_borrow);

        if let Some((i, state)) = key_rest {
            let rest = make_unpack(strand, container, epoch, state, true);
            out.at(i).store(rest);
        } else if sig.variadic == Variadic::Capture {
            Output::set(strand, out.at(sig.len() - 1), &this);
        }
        Ok(())
    }

    pub(crate) fn op_next<'a, 's>(
        this: Recv<'v, 'a, impl AsMut<Self> + Protocol<'v>>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        let borrow = AsMut::<Self>::as_mut(&mut *borrow);
        if (*borrow
            .kv
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?)
        .as_ref()
        .epoch
            != borrow.epoch
        {
            return Err(Error::concurrency(strand));
        }
        let container = borrow.kv.clone();
        let container_borrow = container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        match borrow
            .state
            .next_pair(strand, (*container_borrow).as_ref())?
        {
            Some((key, value)) => {
                out.store(Value::from_object(tuple::tuple(strand, [key, value])));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub(crate) fn op_spread<'s>(
        this: Recv<'v, '_, impl AsMut<Self> + Protocol<'v>>,
        strand: &mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let borrow = AsMut::<Self>::as_mut(&mut *borrow);
        let container = borrow.kv.clone();
        if (*container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?)
        .as_ref()
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
                let container_borrow = container
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?;
                borrow
                    .state
                    .next_pair(strand, (*container_borrow).as_ref())?
            };
            let Some((key, value)) = pair else {
                return Ok(());
            };
            Inner::spread_key_value(strand, &mut next_pos, key, value, context, sink)?;
        }
    }
}
