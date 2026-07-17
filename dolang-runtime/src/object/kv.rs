use std::{
    cell::{Cell, RefCell},
    hash::{DefaultHasher, Hash, Hasher},
    mem,
    ops::ControlFlow,
};

use bitvec::boxed::BitBox;

use crate::{
    bytecode::Variadic,
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
    fn spread_key_value<'s>(
        strand: &mut Strand<'v, 's>,
        next_pos: &mut i64,
        mut key: Value<'v>,
        mut value: Value<'v>,
        context: SpreadContext,
        sink: &mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        if context == SpreadContext::Sequence {
            value = Value::from_object(tuple::tuple(strand, [key, value]));
            sink.positional(strand, Slot::new(&mut value))
        } else if key.to_i64(strand).ok() == Some(*next_pos) {
            *next_pos += 1;
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
        if sig.variadic == Variadic::None && next(index).is_some() {
            return Err(Error::unexpected_positional(strand, sig.required));
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
        let mut next_pos = 0i64;
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
        w: &mut dyn crate::value::Format<'v>,
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
        if sig.variadic == Variadic::Capture {
            out.at(sig.len() - 1)
                .store(Value::from_input(strand, &this))
        }
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
        w: &mut dyn crate::value::Format<'v>,
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
        if sig.variadic == Variadic::Capture {
            out.at(sig.len() - 1)
                .store(Value::from_input(strand, &this))
        }
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
        w: &mut dyn crate::value::Format<'v>,
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
        if sig.variadic == Variadic::Capture {
            out.at(sig.len() - 1)
                .store(Value::from_input(strand, &this))
        }
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
    pub(crate) fn op_debug<'a, 's, T: AsRef<Inner<'v>> + Collect + 'v>(
        this: Recv<'v, 'a, T>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn crate::value::Format<'v>,
        open: &str,
        close: &str,
        separator: &str,
    ) -> Result<'v, 's, ()> {
        let this_borrow = this.borrow(strand)?;
        crate::fmt!(strand, w, "{open}")?;
        let mut index = 0usize;
        let mut next_int_key = Some(0);
        let mut first = true;
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
                if first {
                    first = false
                } else {
                    crate::fmt!(strand, w, "{separator}")?;
                }
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
                Output::set(strand, out, unsafe { pair.as_ref().value.at(0) });
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
        make_unpack: impl FnOnce(&mut Strand<'v, 's>, GcObj<'v, T>, u64, Skip<'v>) -> Value<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let inner: &Inner<'v> = (*borrow).as_ref();
        let pos_count = sig.required + sig.optional.len();
        for i in 0..pos_count {
            let value = i64::try_from(i).map_err(|_| Error::overflow(strand))?;
            let key = Value::from_i64(strand, value);
            if let Some(value) = inner.get(strand, &key, Some(0))? {
                out.at(i).store(value.dup());
            } else if i >= sig.required
                && let Some(default) = sig.optional.get(i)
            {
                out.at(i).store(default.dup());
            } else {
                return Err(Error::missing_positional(strand, i));
            }
        }
        let value = i64::try_from(pos_count).map_err(|_| Error::overflow(strand))?;
        if sig.variadic == Variadic::None
            && inner
                .get(strand, &Value::from_i64(strand, value), Some(0))?
                .is_some()
        {
            return Err(Error::unexpected_positional(strand, sig.required));
        }
        let mut skip = Skip::new();
        let mut found_keys = 0;
        for (i, key) in sig.keys.iter().enumerate() {
            let key_value = match &key.kind {
                UnpackKeyKind::Sym(sym) => Value::from_object(strand.sym_obj(*sym)),
                UnpackKeyKind::Const(value) => value.dup(),
            };

            let hv = hash(strand, &key_value)?;
            let seen = skip.add(strand, &key_value, hv);

            let instance = i64::try_from(seen).map_err(|_| Error::overflow(strand))?;
            if let Some(value) = inner.get(strand, &key_value, Some(instance))? {
                out.at(sig.required + i).store(value.dup())
            } else if let Some(default) = &key.default {
                out.at(sig.required + i).store(default.dup())
            } else {
                return Err(match &key.kind {
                    UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                    UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
                });
            }
            found_keys += 1;
        }
        if sig.variadic == Variadic::None && found_keys + pos_count < inner.inner.len() {
            return Err(Error::unexpected_key(
                strand,
                Sym::well_known(sym::ITEM_ERROR),
            ));
        }
        match sig.variadic {
            Variadic::None | Variadic::Discard => {}
            Variadic::Capture => {
                let container = this.to_strong();
                let epoch = inner.epoch;
                out.at(sig.required + sig.keys.len())
                    .store(make_unpack(strand, container, epoch, skip));
            }
        }
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
        let mut next_pos = 0i64;
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
                            let subindex = match subindex {
                                Some(subindex) => index::element(items.len(), subindex),
                                None => Some(items.len().saturating_sub(1)),
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

pub(crate) enum UnpackState<'v> {
    Int {
        int: i64,
        resume: usize,
        skip: Skip<'v>,
    },
    Order {
        int: i64,
        index: usize,
        skip: Skip<'v>,
    },
    Resume {
        index: usize,
        skip: Skip<'v>,
    },
}

pub(crate) struct UnpackInner<'v, T: Protocol<'v> + AsRef<Inner<'v>> + AsMut<Inner<'v>>> {
    pub(crate) state: UnpackState<'v>,
    pub(crate) epoch: u64,
    pub(crate) kv: GcObj<'v, T>,
}

impl<'v, T: Protocol<'v> + AsRef<Inner<'v>> + AsMut<Inner<'v>>> UnpackInner<'v, T> {
    pub(crate) fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.kv.accept(visit)
    }

    pub(crate) fn clear(&mut self) {}

    pub(crate) async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, impl AsMut<Self> + Protocol<'v>>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a sig::Unpack<'v, 'a>,
        mut out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let mut borrow = this.borrow_mut(strand)?;
        let borrow = AsMut::<Self>::as_mut(&mut *borrow);
        let dict = borrow.kv.clone();
        let dict_borrow = dict.borrow().ok_or_else(|| Error::concurrency(strand))?;
        let inner: &Inner<'v> = (*dict_borrow).as_ref();
        let (int, skip) = match &mut borrow.state {
            UnpackState::Int { int, skip, .. } | UnpackState::Order { int, skip, .. } => {
                (*int, skip)
            }
            UnpackState::Resume { skip, .. } => (i64::MAX, skip),
        };

        // Phase 1: Clone Skip for validation - mutate clone instead of original
        let mut temp_skip = skip.clone();

        let pos_count = sig.required + sig.optional.len();
        for i in 0..pos_count {
            let value = i64::try_from(i)
                .ok()
                .and_then(|i| i.checked_add(int))
                .ok_or_else(|| Error::overflow(strand))?;
            let key = Value::from_i64(strand, value);
            if let Some(value) = inner.get(strand, &key, Some(0))? {
                out.at(i).store(value.dup());
            } else if i >= sig.required
                && let Some(default) = sig.optional.get(i)
            {
                out.at(i).store(default.dup());
            } else {
                // Error during validation - temp_skip discarded, original untouched
                return Err(Error::missing_positional(strand, i));
            }
        }
        let value = i64::try_from(pos_count)
            .ok()
            .and_then(|i| i.checked_add(int))
            .ok_or_else(|| Error::overflow(strand))?;
        if sig.variadic == Variadic::None
            && inner
                .get(strand, &Value::from_i64(strand, value), Some(0))?
                .is_some()
        {
            // Error during validation - temp_skip discarded, original untouched
            return Err(Error::unexpected_positional(strand, sig.required));
        }
        let skipped = temp_skip.count;
        let mut found_keys = 0;
        for (i, key) in sig.keys.iter().enumerate() {
            // Convert key to value based on kind
            let key_value = match &key.kind {
                UnpackKeyKind::Sym(sym) => Value::from_object(strand.sym_obj(*sym)),
                UnpackKeyKind::Const(value) => value.dup(),
            };

            let hv = hash(strand, &key_value)?;
            // Mutate CLONE instead of original
            let seen = temp_skip.add(strand, &key_value, hv);

            let instance = i64::try_from(seen).map_err(|_| Error::overflow(strand))?;
            if let Some(value) = inner.get(strand, &key_value, Some(instance))? {
                out.at(sig.required + i).store(value.dup())
            } else if let Some(default) = &key.default {
                out.at(sig.required + i).store(default.dup())
            } else {
                // Error during validation - temp_skip discarded, original untouched
                return Err(match &key.kind {
                    UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                    UnpackKeyKind::Const(val) => Error::missing_key(strand, val),
                });
            }
            found_keys += 1;
        }
        // Final validation
        if sig.variadic == Variadic::None
            && found_keys + pos_count + int as usize + skipped < inner.inner.len()
        {
            // Error during validation - temp_skip discarded, original untouched
            return Err(Error::unexpected_key(
                strand,
                Sym::well_known(sym::ITEM_ERROR),
            ));
        }

        // Phase 2: All validation passed, commit Skip changes
        borrow.state = match &mut borrow.state {
            UnpackState::Int { int, resume, .. } => UnpackState::Int {
                int: *int + sig.required as i64,
                resume: *resume,
                skip: temp_skip, // Commit the validated Skip
            },
            UnpackState::Order { int, index, .. } => UnpackState::Int {
                int: *int + sig.required as i64,
                resume: *index,
                skip: temp_skip, // Commit the validated Skip
            },
            UnpackState::Resume { index, .. } => UnpackState::Resume {
                index: *index,
                skip: temp_skip, // Commit the validated Skip
            },
        };
        match sig.variadic {
            Variadic::None | Variadic::Discard => {}
            Variadic::Capture => {
                Output::set(strand, out.at(sig.required + sig.keys.len()), &this);
            }
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
        let container = borrow.kv.clone();
        let epoch = borrow.epoch;
        if (*container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?)
        .as_ref()
        .epoch
            != epoch
        {
            return Err(Error::concurrency(strand));
        }
        'main: loop {
            break match &mut borrow.state {
                UnpackState::Int { int, resume, skip } => {
                    let key = Value::from_i64(strand, *int);
                    if let Some(value) = (*container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?)
                    .as_ref()
                    .get(strand, &key, Some(0))?
                    {
                        out.store(Value::from_object(tuple::tuple(strand, [key, value.dup()])));
                        *int += 1;
                        Ok(true)
                    } else {
                        borrow.state = UnpackState::Resume {
                            index: *resume,
                            skip: skip.take(),
                        };
                        continue;
                    }
                }
                UnpackState::Resume { index, skip } => loop {
                    break if let Some(bucket) = (*container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?)
                    .as_ref()
                    .index
                    .get(*index)
                    {
                        *index += 1;
                        if let Some((bucket, subindex)) = bucket {
                            let bucket = unsafe { bucket.as_ref() };
                            let key = &bucket.key;
                            let hv = hash(strand, key)?;
                            let key = if (*subindex == 0 && key.is_int(strand))
                                || skip.add(strand, key, hv) >= bucket.value.len()
                            {
                                continue;
                            } else {
                                key.dup()
                            };
                            let value = bucket.value.at(*subindex).dup();
                            out.store(Value::from_object(tuple::tuple(strand, [key, value])));
                            Ok(true)
                        } else {
                            continue;
                        }
                    } else {
                        Ok(false)
                    };
                },
                UnpackState::Order { int, index, skip } => loop {
                    break if let Some(bucket) = (*container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?)
                    .as_ref()
                    .index
                    .get(*index)
                    {
                        *index += 1;
                        if let Some((bucket, subindex)) = bucket {
                            let bucket = unsafe { bucket.as_ref() };
                            let key = &bucket.key;
                            let hv = bucket.hash;
                            let key = if let Some(int_key) =
                                key.as_int(strand).and_then(|x| i64::try_from(x).ok())
                            {
                                if int_key == *int {
                                    *int += 1;
                                    key.dup()
                                } else {
                                    borrow.state = UnpackState::Int {
                                        int: *int,
                                        resume: *index - 1,
                                        skip: skip.take(),
                                    };
                                    continue 'main;
                                }
                            } else if skip.add(strand, key, hv) >= bucket.value.len() {
                                continue;
                            } else {
                                key.dup()
                            };
                            let value = bucket.value.at(*subindex).dup();
                            out.store(Value::from_object(tuple::tuple(strand, [key, value])));
                            Ok(true)
                        } else {
                            continue;
                        }
                    } else {
                        Ok(false)
                    };
                },
            };
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
        let epoch = borrow.epoch;
        if (*container
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?)
        .as_ref()
        .epoch
            != epoch
        {
            return Err(Error::concurrency(strand));
        }
        let mut next_pos = match &borrow.state {
            UnpackState::Int { int, .. } | UnpackState::Order { int, .. } => *int,
            UnpackState::Resume { .. } => i64::MAX,
        };
        let mut counter = 0usize;
        'main: loop {
            counter += 1;
            if counter.is_multiple_of(crate::INTERRUPT_INTERVAL) {
                strand.check_trap()?;
            }
            match &mut borrow.state {
                UnpackState::Int { int, resume, skip } => {
                    let key = Value::from_i64(strand, *int);
                    if let Some(value) = (*container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?)
                    .as_ref()
                    .get(strand, &key, Some(0))?
                    {
                        Inner::spread_key_value(
                            strand,
                            &mut next_pos,
                            key,
                            value.dup(),
                            context,
                            sink,
                        )?;
                        *int += 1;
                    } else {
                        borrow.state = UnpackState::Resume {
                            index: *resume,
                            skip: skip.take(),
                        };
                    }
                }
                UnpackState::Resume { index, skip } => loop {
                    let container_borrow = container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?;
                    let inner: &Inner<'v> = (*container_borrow).as_ref();
                    let Some(bucket) = inner.index.get(*index) else {
                        return Ok(());
                    };
                    *index += 1;
                    let Some((bucket, subindex)) = bucket else {
                        continue;
                    };
                    let bucket = unsafe { bucket.as_ref() };
                    let key = &bucket.key;
                    let hv = bucket.hash;
                    if (*subindex == 0 && key.is_int(strand))
                        || skip.add(strand, key, hv) >= bucket.value.len()
                    {
                        continue;
                    }
                    Inner::spread_key_value(
                        strand,
                        &mut next_pos,
                        key.dup(),
                        bucket.value.at(*subindex).dup(),
                        context,
                        sink,
                    )?;
                    break;
                },
                UnpackState::Order { int, index, skip } => loop {
                    let container_borrow = container
                        .borrow()
                        .ok_or_else(|| Error::concurrency(strand))?;
                    let inner: &Inner<'v> = (*container_borrow).as_ref();
                    let Some(bucket) = inner.index.get(*index) else {
                        return Ok(());
                    };
                    *index += 1;
                    let Some((bucket, subindex)) = bucket else {
                        continue;
                    };
                    let bucket = unsafe { bucket.as_ref() };
                    let key = &bucket.key;
                    let hv = bucket.hash;
                    let key = if let Some(int_key) =
                        key.as_int(strand).and_then(|x| i64::try_from(x).ok())
                    {
                        if int_key == *int {
                            let key = key.dup();
                            *int += 1;
                            key
                        } else {
                            borrow.state = UnpackState::Int {
                                int: *int,
                                resume: *index - 1,
                                skip: skip.take(),
                            };
                            continue 'main;
                        }
                    } else if skip.add(strand, key, hv) >= bucket.value.len() {
                        continue;
                    } else {
                        key.dup()
                    };
                    Inner::spread_key_value(
                        strand,
                        &mut next_pos,
                        key,
                        bucket.value.at(*subindex).dup(),
                        context,
                        sink,
                    )?;
                    break;
                },
            }
        }
    }
}
