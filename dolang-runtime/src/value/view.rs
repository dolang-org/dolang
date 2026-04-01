use std::{
    hash::{Hash, Hasher},
    marker::PhantomData,
    num::NonZero,
    ptr::NonNull,
};

use crate::{
    error::{Error, Result},
    gc,
    object::{
        array, dict,
        kv::{self, Entry, EntryValue},
        protocol::{GcObjBorrow, Header},
        record,
    },
    strand::Strand,
    sym::Sym,
    value::{Input, Output, Value},
    vm::Vm,
};

// ── ObjectId ────────────────────────────────────────────────────────────────

/// Opaque GC-object identity for cycle detection.
///
/// Holds a phantom reference to guarantee it cannot outlive the live GC
/// reference that pins the object's address in memory.
pub struct ObjectId<'v, 'a>(NonNull<Header>, PhantomData<(&'v mut &'v (), &'a ())>);

impl<'v, 'a> ObjectId<'v, 'a> {
    pub fn addr(&self) -> NonZero<usize> {
        self.0.addr()
    }
}

impl<'v, 'a> Clone for ObjectId<'v, 'a> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, 'a> Copy for ObjectId<'v, 'a> {}

impl<'v, 'a> PartialEq for ObjectId<'v, 'a> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<'v, 'a> Eq for ObjectId<'v, 'a> {}

impl<'v, 'a> Hash for ObjectId<'v, 'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

// ── ObjectView ───────────────────────────────────────────────────────────────

/// Opaque view of a GC object that is not one of the standard collection types.
///
/// This type is a placeholder that can be extended in the future to expose
/// more information about non-primitive objects.
pub struct ObjectView<'v, 'a> {
    ptr: NonNull<Header>,
    phantom: PhantomData<(&'v mut &'v (), &'a Header)>,
}

impl<'v, 'a> ObjectView<'v, 'a> {
    pub(crate) unsafe fn from_ptr(ptr: NonNull<Header>) -> Self {
        Self {
            ptr,
            phantom: PhantomData,
        }
    }

    /// Return the opaque identity of this object for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(self.ptr, PhantomData)
    }
}

// ── ArrayHandle ──────────────────────────────────────────────────────────────

/// Lightweight handle to a GC-managed array value.
pub struct Array<'v, 'a>(pub(crate) GcObjBorrow<'v, 'a, array::Array<'v>>);

impl<'v, 'a> Array<'v, 'a> {
    pub(crate) unsafe fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, array::Array<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this array for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        // BaseBorrow is Copy, so this copies and extracts the inner pointer.
        ObjectId(self.0.into_raw().cast(), PhantomData)
    }

    /// Number of elements. Briefly takes a shared interior borrow.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        Ok(borrow.inner.len())
    }

    /// Get element at `index`. Returns `false` if out of bounds.
    pub fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        match borrow.inner.get(index) {
            Some(v) => {
                Output::set(strand, out, v);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Write `value` to `index`. Returns `false` if out of bounds.
    pub fn set<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        match borrow.inner.get_mut(index) {
            Some(v) => {
                *v = Value::from_input(strand, value);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Append a value drawn from `input`. Briefly takes an exclusive interior borrow.
    pub fn push<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        input: impl Input<'v>,
    ) -> Result<'v, 's, ()> {
        // Build the value before taking the exclusive borrow.
        let value = Value::from_input(strand, input);
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.inner.push(value);
        Ok(())
    }

    /// Remove and write the last element to `out`. Returns `false` if empty.
    pub fn pop<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        // Remove before calling Output::set, mirroring push's pattern.
        let val = {
            let mut borrow = match self.0.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.inner.pop()
        };
        match val {
            Some(v) => {
                Output::set(strand, out, &v);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Lightweight handle to a GC-managed dict value.
pub struct Dict<'v, 'a>(pub(crate) GcObjBorrow<'v, 'a, dict::Dict<'v>>);

impl<'v, 'a> Dict<'v, 'a> {
    pub(crate) unsafe fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, dict::Dict<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this dict for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(self.0.into_raw().cast(), PhantomData)
    }

    /// Total number of key-value pairs (counting duplicate keys).
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        Ok(borrow.0.total_pairs)
    }

    /// Return a stateful cursor over insertion-order key-value pairs.
    pub fn pairs(&self) -> DictPairs<'v, 'a> {
        DictPairs {
            borrow: self.0,
            pos: 0,
        }
    }

    /// Insert a key-value pair.
    pub fn insert<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        key: impl Input<'v>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, ()> {
        let key = Value::from_input(strand, key);
        let value = Value::from_input(strand, value);
        let hv = kv::hash(strand, &key)?;
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.0.insert(strand, key, value, hv, false);
        Ok(())
    }
}

// ── RecordHandle ─────────────────────────────────────────────────────────────

/// Lightweight handle to a GC-managed record value.
pub struct Record<'v, 'a>(gc::Borrow<'v, 'a, Header, record::Record<'v>>);

impl<'v, 'a> Record<'v, 'a> {
    pub(crate) unsafe fn from_borrow(borrow: GcObjBorrow<'v, 'a, record::Record<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this record for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(self.0.into_raw().cast(), PhantomData)
    }

    /// Total number of key-value pairs.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        Ok(borrow.0.total_pairs)
    }

    /// Return a stateful cursor over insertion-order key-value pairs.
    pub fn pairs(&self) -> RecordPairs<'v, 'a> {
        RecordPairs {
            borrow: self.0,
            pos: 0,
        }
    }
}

// ── TupleHandle ──────────────────────────────────────────────────────────────

/// Lightweight handle to a GC-managed tuple value.
pub struct Tuple<'v, 'a>(gc::Borrow<'v, 'a, Header, [Value<'v>]>);

impl<'v, 'a> Tuple<'v, 'a> {
    pub(crate) unsafe fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, [Value<'v>]>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this tuple for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(self.0.into_raw().cast(), PhantomData)
    }

    /// Number of elements. Tuples are fixed-size immutable slices; no borrow needed.
    pub fn len(&self) -> usize {
        self.0.get().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Write element `index` to `out`. Returns `false` if out of bounds.
    pub fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        match self.0.get().get(index) {
            Some(v) => {
                Output::set(strand, out, v);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

// ── ValueView ────────────────────────────────────────────────────────────────

/// Type-discriminating view of a [`crate::value::Value`].
pub enum View<'v, 'a> {
    Nil,
    Bool(bool),
    /// Covers both immediate and boxed/verbatim `i64`.
    Int(i64),
    /// Covers both immediate and boxed/verbatim `f64`.
    Float(f64),
    Str(&'a str),
    Bin(&'a [u8]),
    Sym(Sym<'v, 'a>),
    Array(Array<'v, 'a>),
    Dict(Dict<'v, 'a>),
    Record(Record<'v, 'a>),
    Tuple(Tuple<'v, 'a>),
    /// Any GC object that does not match the standard types above.
    Object(ObjectView<'v, 'a>),
}

// ── DictPairs ────────────────────────────────────────────────────────────────

/// Stateful cursor for iterating insertion-order key-value pairs of a [`Dict`].
pub struct DictPairs<'v, 'a> {
    borrow: gc::Borrow<'v, 'a, Header, dict::Dict<'v>>,
    pos: usize,
}

impl<'v, 'a> DictPairs<'v, 'a> {
    /// Write the next key-value pair to the outputs, advancing the cursor.
    /// Returns `false` when all pairs have been yielded.
    pub fn next<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: impl Output<'v>,
        value: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        let borrow = match self.borrow.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        Ok(kv_next_pair(
            &borrow.0,
            &mut self.pos,
            strand.vm(),
            key,
            value,
        ))
    }
}

// ── RecordPairs ───────────────────────────────────────────────────────────────

/// Stateful cursor for iterating insertion-order key-value pairs of a `Record`.
pub struct RecordPairs<'v, 'a> {
    borrow: GcObjBorrow<'v, 'a, record::Record<'v>>,
    pos: usize,
}

impl<'v, 'a> RecordPairs<'v, 'a> {
    /// Write the next key-value pair to the outputs, advancing the cursor.
    /// Returns `false` when all pairs have been yielded.
    pub fn next<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: impl Output<'v>,
        value: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        let borrow = match self.borrow.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        Ok(kv_next_pair(
            &borrow.0,
            &mut self.pos,
            strand.vm(),
            key,
            value,
        ))
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Advance `pos` through `inner.index` skipping holes, write the next live
/// key-value pair to the outputs, and return `true`.  Returns `false` when
/// the index is exhausted.
///
/// The caller must hold the interior borrow on the GC object for the duration
/// of this call.
fn kv_next_pair<'v>(
    inner: &kv::Inner<'v>,
    pos: &mut usize,
    vm: &Vm<'v>,
    key: impl Output<'v>,
    value: impl Output<'v>,
) -> bool {
    while let Some(slot) = inner.index.get(*pos) {
        *pos += 1;
        if let Some((bucket, subindex)) = slot {
            // Safety: the interior borrow (BaseRef) held by the caller
            // prevents concurrent modification of the table; the bucket
            // pointer was set up correctly during insertion and remains
            // valid for the duration of the borrow.
            let entry: &Entry<'v> = unsafe { bucket.as_ref() };
            Output::set(vm, key, &entry.key);
            Output::set(
                vm,
                value,
                match &entry.value {
                    EntryValue::Single { value, .. } => value,
                    EntryValue::Multi(items) => &items[*subindex].0,
                },
            );
            return true;
        }
    }
    false
}
