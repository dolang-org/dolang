use std::{
    cell::Cell,
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    ops::ControlFlow,
};

use crate::{
    arg::Args,
    error::{Error, Result, ResultExt},
    gc::{Collect, arena::Visit},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value},
    vm::Vm,
};

use super::{
    BoundMethod, iter, kv,
    protocol::{GcObj, Inspect, Protocol, Recv, Spread, SpreadContext, dispatch_native_method},
    tuple,
};

use dolang_util::hashbrown::raw::{Bucket, RawTable};

pub(crate) struct Entry<'v> {
    value: Value<'v>,
    hash: u64,
    index: usize,
}

pub(crate) struct Inner<'v> {
    inner: RawTable<Entry<'v>>,
    order: Vec<Option<Bucket<Entry<'v>>>>,
    epoch: u64,
}

impl<'v> Inner<'v> {
    fn new() -> Self {
        Self {
            inner: Default::default(),
            order: Default::default(),
            epoch: 0,
        }
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        unsafe {
            for bucket in self.inner.iter() {
                bucket.as_ref().value.accept(visit)?;
            }
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        if self.inner.is_empty() {
            return;
        }
        self.inner.clear();
        self.order.clear();
        self.epoch += 1;
    }

    fn insert_at_index(&mut self, hash: u64, insert: usize, value: Value<'v>) {
        let capacity = self.inner.capacity();
        let i = if self.order.len() >= 2 * capacity {
            let mut compacted = Vec::with_capacity(self.order.len());
            for entry in self.order.drain(..) {
                let Some(bucket) = entry else {
                    continue;
                };
                unsafe {
                    bucket.as_mut().index = compacted.len();
                }
                compacted.push(Some(bucket));
            }
            let next = compacted.len();
            self.order = compacted;
            next
        } else {
            self.order.len()
        };
        let bucket = unsafe {
            self.inner.insert_at_index(
                hash,
                insert,
                Entry {
                    value,
                    hash,
                    index: i,
                },
            )
        };
        self.order.push(Some(bucket));
        self.epoch += 1;
    }

    fn reserve_for_insert(&mut self) {
        if self.inner.len() == self.inner.capacity() {
            self.rehash()
        }
    }

    fn rehash(&mut self) {
        let capacity = (self.inner.capacity() * 2).max(1);
        let mut next = Self {
            inner: RawTable::with_capacity(capacity),
            order: Vec::new(),
            epoch: self.epoch + 1,
        };
        for entry in self.order.drain(..) {
            let Some(bucket) = entry else {
                continue;
            };
            let mut entry = unsafe { self.inner.remove(bucket).0 };
            entry.index = next.order.len();
            let bucket = next.inner.insert(entry.hash, entry, |entry| entry.hash);
            next.order.push(Some(bucket));
        }
        *self = next;
    }

    fn contains<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
        hash: u64,
    ) -> Result<'v, 's, bool> {
        Ok(self
            .inner
            .find(hash, |entry| value.eq(strand, &entry.value))
            .is_some())
    }

    fn insert_known_unique(&mut self, value: Value<'v>, hash: u64) {
        self.reserve_for_insert();
        let index = self.order.len();
        let bucket = unsafe {
            self.inner
                .insert_no_grow(hash, Entry { value, hash, index })
        };
        self.order.push(Some(bucket));
    }

    fn insert_unique<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
        hash: u64,
    ) -> Result<'v, 's, bool> {
        self.reserve_for_insert();
        match self.inner.find_or_find_insert_index(
            hash,
            |entry| value.eq(strand, &entry.value),
            |entry| entry.hash,
        ) {
            Ok(_) => Ok(false),
            Err(insert) => {
                self.insert_at_index(hash, insert, value.dup());
                Ok(true)
            }
        }
    }

    fn insert<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: Value<'v>,
        hash: u64,
    ) -> Result<'v, 's, bool> {
        self.reserve_for_insert();
        match self.inner.find_or_find_insert_index(
            hash,
            |entry| value.eq(strand, &entry.value),
            |entry| entry.hash,
        ) {
            Ok(_) => Ok(false),
            Err(insert) => {
                self.insert_at_index(hash, insert, value);
                Ok(true)
            }
        }
    }

    fn delete<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: &Value<'v>,
        hash: u64,
    ) -> Result<'v, 's, bool> {
        let Some(bucket) = self
            .inner
            .find(hash, |entry| value.eq(strand, &entry.value))
        else {
            return Ok(false);
        };
        unsafe {
            self.order.get_unchecked_mut(bucket.as_ref().index).take();
            self.inner.erase(bucket);
        }
        self.epoch += 1;
        Ok(true)
    }

    fn clone_ordered(&self) -> Set<'v> {
        let mut set = Set::new();
        unsafe {
            for entry in &self.order {
                let Some(bucket) = entry else {
                    continue;
                };
                let entry = bucket.as_ref();
                set.0.insert_known_unique(entry.value.dup(), entry.hash);
            }
        }
        set
    }

    fn extend_from<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        other: &Inner<'v>,
    ) -> Result<'v, 's, ()> {
        unsafe {
            for entry in &other.order {
                let Some(bucket) = entry else {
                    continue;
                };
                let entry = bucket.as_ref();
                self.insert_unique(strand, &entry.value, entry.hash)?;
            }
        }
        Ok(())
    }
}

pub(crate) struct Set<'v>(pub(crate) Inner<'v>);

impl<'v> Set<'v> {
    pub(crate) fn new() -> Self {
        Self(Inner::new())
    }

    fn from_inner(other: &Inner<'v>) -> Self {
        other.clone_ordered()
    }
}

unsafe impl<'v> Collect for Set<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.0.accept(visit)
    }

    fn clear(&mut self) {
        self.0.clear()
    }
}

struct SetSpread<'a, 'v>(&'a mut Set<'v>);

impl<'a, 'v, 's> Spread<'v, 's> for SetSpread<'a, 'v> {
    fn positional(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let hash = kv::hash(strand, &value)?;
        self.0.0.insert(strand, value.take(), hash)?;
        Ok(())
    }

    fn symbol(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: Sym<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let pair = Value::from_object(tuple::tuple(
            strand,
            [Value::from_object(strand.sym_obj(key)), value.take()],
        ));
        let hash = kv::hash(strand, &pair)?;
        self.0.0.insert(strand, pair, hash)?;
        Ok(())
    }

    fn keyed(
        &mut self,
        strand: &mut Strand<'v, 's>,
        mut key: Slot<'v, '_>,
        mut value: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let pair = Value::from_object(tuple::tuple(strand, [key.take(), value.take()]));
        let hash = kv::hash(strand, &pair)?;
        self.0.0.insert(strand, pair, hash)?;
        Ok(())
    }
}

pub(crate) struct Iter<'v> {
    index: Cell<usize>,
    epoch: u64,
    set: GcObj<'v, Set<'v>>,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.set.accept(visit)
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.vm().singletons().input_iter.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<set iterator>").into_do(strand)
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, &this);
        Ok(())
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.borrow(strand)?;
        let set = borrow
            .set
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        if set.0.epoch != borrow.epoch {
            return Err(Error::concurrency_msg(
                strand,
                "collection was modified during iteration",
            ));
        }
        loop {
            let Some(entry) = set.0.order.get(borrow.index.get()) else {
                return Ok(false);
            };
            borrow.index.set(borrow.index.get() + 1);
            let Some(bucket) = entry else {
                continue;
            };
            Output::set(strand, &mut out, unsafe { &bucket.as_ref().value });
            return Ok(true);
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
}

impl<'v> Protocol<'v> for Set<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, &strand.vm().singletons().set)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().set.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "set([").into_do(strand)?;
        let borrow = this.borrow(strand)?;
        let mut first = true;
        unsafe {
            for entry in &borrow.0.order {
                let Some(bucket) = entry else {
                    continue;
                };
                if !first {
                    write!(w, ", ").into_do(strand)?;
                }
                bucket.as_ref().value.op_debug(strand, w)?;
                first = false;
            }
        }
        write!(w, "])").into_do(strand)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let Ok(borrow) = this.borrow(strand) else {
            return true;
        };
        !borrow.0.inner.is_empty()
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut DefaultHasher,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        let mut digests = Vec::with_capacity(borrow.0.len());
        unsafe {
            for bucket in borrow.0.inner.iter() {
                let mut elem_hasher = DefaultHasher::new();
                bucket.as_ref().value.op_hash(strand, &mut elem_hasher)?;
                digests.push(elem_hasher.finish());
            }
        }
        digests.sort_unstable();
        sym::SET.hash(hasher);
        digests.len().hash(hasher);
        for digest in digests {
            digest.hash(hasher);
        }
        Ok(())
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        let Some(other) = other.downcast_ref(strand.builtin_types().set) else {
            return Ok(Value::FALSE);
        };
        let left = this.borrow(strand)?;
        let right = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
        if left.0.len() != right.0.len() {
            return Ok(Value::FALSE);
        }
        unsafe {
            for bucket in left.0.inner.iter() {
                let entry = bucket.as_ref();
                if right
                    .0
                    .inner
                    .find(entry.hash, |other| entry.value.eq(strand, &other.value))
                    .is_none()
                {
                    return Ok(Value::FALSE);
                }
            }
        }
        Ok(Value::TRUE)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::LEN => {
                let input = this.borrow(strand)?.0.len() as i64;
                Output::set(strand, out, input);
                Ok(())
            }
            sym::ADD
            | sym::DELETE
            | sym::CLEAR
            | sym::COPY
            | sym::CONTAINS
            | sym::UNION
            | sym::INTERSECT
            | sym::DIFF
            | sym::SYM_DIFF
            | sym::IS_SUBSET
            | sym::IS_SUPERSET => {
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
            sym::ADD => {
                let ([mut value], []) = unpack!(strand, args, 1, 0)?;
                let hash = kv::hash(strand, &value)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.0.insert(strand, value.take(), hash)?;
                Ok(())
            }
            sym::DELETE => {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let hash = kv::hash(strand, &value)?;
                let deleted = {
                    let mut borrow = this.borrow_mut(strand)?;
                    borrow.0.delete(strand, &value, hash)?
                };
                Output::set(strand, out, deleted);
                Ok(())
            }
            sym::CLEAR => {
                let _ = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.0.clear();
                Ok(())
            }
            sym::COPY => {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let borrow = this.borrow(strand)?;
                out.store(Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().set,
                    Self::from_inner(&borrow.0),
                )));
                Ok(())
            }
            sym::CONTAINS => {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let hash = kv::hash(strand, &value)?;
                let contains = this.borrow(strand)?.0.contains(strand, &value, hash)?;
                Output::set(strand, out, contains);
                Ok(())
            }
            sym::UNION
            | sym::INTERSECT
            | sym::DIFF
            | sym::SYM_DIFF
            | sym::IS_SUBSET
            | sym::IS_SUPERSET => {
                let ([other], []) = unpack!(strand, args, 1, 0)?;
                let other = other
                    .downcast_ref(strand.builtin_types().set)
                    .ok_or_else(|| Error::type_error(strand, "expected set"))?;
                match method.tag() {
                    sym::UNION => {
                        let mut result = Self::from_inner(&this.borrow(strand)?.0);
                        let other = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        result.0.extend_from(strand, &other.0)?;
                        out.store(Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().set,
                            result,
                        )));
                    }
                    sym::INTERSECT => {
                        let left = this.borrow(strand)?;
                        let other = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        let mut result = Self::new();
                        unsafe {
                            for entry in &left.0.order {
                                let Some(bucket) = entry else {
                                    continue;
                                };
                                let entry = bucket.as_ref();
                                if other.0.contains(strand, &entry.value, entry.hash)? {
                                    result.0.insert_unique(strand, &entry.value, entry.hash)?;
                                }
                            }
                        }
                        out.store(Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().set,
                            result,
                        )));
                    }
                    sym::DIFF => {
                        let left = this.borrow(strand)?;
                        let other = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        let mut result = Self::new();
                        unsafe {
                            for entry in &left.0.order {
                                let Some(bucket) = entry else {
                                    continue;
                                };
                                let entry = bucket.as_ref();
                                if !other.0.contains(strand, &entry.value, entry.hash)? {
                                    result.0.insert_unique(strand, &entry.value, entry.hash)?;
                                }
                            }
                        }
                        out.store(Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().set,
                            result,
                        )));
                    }
                    sym::SYM_DIFF => {
                        let left = this.borrow(strand)?;
                        let other = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        let mut result = Self::new();
                        unsafe {
                            for entry in &left.0.order {
                                let Some(bucket) = entry else {
                                    continue;
                                };
                                let entry = bucket.as_ref();
                                if !other.0.contains(strand, &entry.value, entry.hash)? {
                                    result.0.insert_unique(strand, &entry.value, entry.hash)?;
                                }
                            }
                            for entry in &other.0.order {
                                let Some(bucket) = entry else {
                                    continue;
                                };
                                let entry = bucket.as_ref();
                                if !left.0.contains(strand, &entry.value, entry.hash)? {
                                    result.0.insert_unique(strand, &entry.value, entry.hash)?;
                                }
                            }
                        }
                        out.store(Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().set,
                            result,
                        )));
                    }
                    sym::IS_SUBSET => {
                        let left = this.borrow(strand)?;
                        let other = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        let mut subset = true;
                        unsafe {
                            for bucket in left.0.inner.iter() {
                                let entry = bucket.as_ref();
                                if !other.0.contains(strand, &entry.value, entry.hash)? {
                                    subset = false;
                                    break;
                                }
                            }
                        }
                        Output::set(strand, out, subset);
                    }
                    sym::IS_SUPERSET => {
                        let left = this.borrow(strand)?;
                        let other = other.borrow().ok_or_else(|| Error::concurrency(strand))?;
                        let mut superset = true;
                        unsafe {
                            for bucket in other.0.inner.iter() {
                                let entry = bucket.as_ref();
                                if !left.0.contains(strand, &entry.value, entry.hash)? {
                                    superset = false;
                                    break;
                                }
                            }
                        }
                        Output::set(strand, out, superset);
                    }
                    _ => unreachable!(),
                }
                Ok(())
            }
            sym::LEN => Err(Error::type_error(
                strand,
                "set.len is a field, not a method",
            )),
            _ => iter::iterable_mcall(strand, &this, method, args, out).await,
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let iter = Iter {
            index: Cell::new(0),
            epoch: this.borrow(strand)?.0.epoch,
            set: this.to_strong(),
        };
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().set_iter,
            iter,
        )));
        Ok(())
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        unsafe {
            for (i, entry) in borrow.0.order.iter().enumerate() {
                let Some(bucket) = entry else {
                    continue;
                };
                if (i + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                    strand.check_trap()?;
                }
                let mut value = bucket.as_ref().value.dup();
                sink.positional(strand, Slot::new(&mut value))?;
            }
        }
        Ok(())
    }
}

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
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([], [items]) = unpack!(strand, args, 0, 1)?;
        let set = if let Some(items) = items {
            let mut set = Set::new();
            let mut spread = SetSpread(&mut set);
            // FIXME: `set` is not GC-scannable, but then again if it were it would also
            // be mutably borrowed, which would inhibit GC.  This needs a resolution.
            items
                .op_spread(strand, SpreadContext::Sequence, &mut spread)
                .await?;
            set
        } else {
            Set::new()
        };
        out.store(Value::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().set,
            set,
        )));
        Ok(())
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.vm().singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type std.set>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            members: vec![
                Sym::well_known(sym::STR_METHOD),
                Sym::well_known(sym::DBG_METHOD),
                Sym::well_known(sym::EQ_METHOD),
                Sym::well_known(sym::HASH_METHOD),
                Sym::well_known(sym::LEN),
                Sym::well_known(sym::ADD),
                Sym::well_known(sym::DELETE),
                Sym::well_known(sym::CLEAR),
                Sym::well_known(sym::COPY),
                Sym::well_known(sym::CONTAINS),
                Sym::well_known(sym::UNION),
                Sym::well_known(sym::INTERSECT),
                Sym::well_known(sym::DIFF),
                Sym::well_known(sym::SYM_DIFF),
                Sym::well_known(sym::IS_SUBSET),
                Sym::well_known(sym::IS_SUPERSET),
                Sym::well_known(sym::ITER_METHOD),
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
            | sym::EQ_METHOD
            | sym::HASH_METHOD
            | sym::LEN
            | sym::ADD
            | sym::DELETE
            | sym::CLEAR
            | sym::COPY
            | sym::CONTAINS
            | sym::UNION
            | sym::INTERSECT
            | sym::DIFF
            | sym::SYM_DIFF
            | sym::IS_SUBSET
            | sym::IS_SUPERSET
            | sym::ITER_METHOD => {
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
                let ([self_val], []) = unpack!(strand, args, 1, 0)?;
                let native = Value::from_object(GcObj::new(
                    strand.arena(),
                    strand.builtin_types().set,
                    Set::new(),
                ));
                self_val.op_fill(strand, &strand.vm().singletons().set, native)?;
                Ok(())
            }
            _ => {
                dispatch_native_method(strand, &strand.vm().singletons().set, method, args, out)
                    .await
            }
        }
    }
}
