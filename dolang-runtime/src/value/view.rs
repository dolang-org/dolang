use std::{
    fmt::{self, Display, Formatter},
    hash::{Hash, Hasher},
    marker::PhantomData,
    num::NonZero,
    ops::Deref,
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
    strand::{Access, Strand},
    sym::Sym,
    value::{Input, Output, Value},
    vm::Alloc,
};

/// Object identifier
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

/// Typed view of a `str` value.
#[derive(Clone, Copy)]
pub struct Str<'v, 'a> {
    value: &'a str,
    phantom: PhantomData<&'v mut &'v ()>,
}

/// Pinned `str` view
///
/// The underlying string slice is guaranteed to remain address-stable for
/// the lifetime of this struct.
#[derive(Clone)]
pub struct PinStr<'v, 'a> {
    value: &'a str,
    phantom: PhantomData<&'v mut &'v ()>,
}

impl<'v, 'a> Str<'v, 'a> {
    pub(crate) fn from_value(value: &'a str) -> Self {
        Self {
            value,
            phantom: PhantomData,
        }
    }

    /// Get underlying string slice.
    ///
    /// This requires a token from [`Strand::access`].
    pub fn as_str<'s, 'x, 'b>(&self, access: &'x Access<'v, 's>) -> &'b str
    where
        'a: 'b,
        'x: 'b,
    {
        let _ = access;
        self.value
    }

    /// Get length of string
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Is string empty?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get pinned view of string
    pub fn pin(&self) -> PinStr<'v, 'a> {
        PinStr {
            value: self.value,
            phantom: PhantomData,
        }
    }
}

impl<'v, 'a> Display for Str<'v, 'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self.value, f)
    }
}

impl<'v, 'a> Deref for PinStr<'v, 'a> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<'v, 'a> Drop for PinStr<'v, 'a> {
    fn drop(&mut self) {}
}

impl<'v, 'a> From<Str<'v, 'a>> for String {
    fn from(value: Str<'v, 'a>) -> Self {
        value.value.to_owned()
    }
}

/// Typed view of a `bin` value.
#[derive(Clone, Copy)]
pub struct Bin<'v, 'a> {
    value: &'a [u8],
    phantom: PhantomData<&'v mut &'v ()>,
}

/// Pinned `bin` view
///
/// The underlying byte slice is guaranteed to remain address-stable for
/// the lifetime of this struct.
#[derive(Clone)]
pub struct PinBin<'v, 'a> {
    value: &'a [u8],
    phantom: PhantomData<&'v mut &'v ()>,
}

impl<'v, 'a> Bin<'v, 'a> {
    pub(crate) fn from_value(value: &'a [u8]) -> Self {
        Self {
            value,
            phantom: PhantomData,
        }
    }

    /// Get underlying byte slice.
    ///
    /// This requires a token from [`Strand::access`].
    pub fn as_slice<'s, 'x, 'b>(&self, access: &'x Access<'v, 's>) -> &'b [u8]
    where
        'a: 'b,
        'x: 'b,
    {
        let _ = access;
        self.value
    }

    /// Get length of `bin`
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Is the the `bin` empty?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Convert to owned [`Vec`]
    pub fn to_vec(&self) -> Vec<u8> {
        self.value.to_owned()
    }

    /// Get pinned view of `bin`
    pub fn pin(&self) -> PinBin<'v, 'a> {
        PinBin {
            value: self.value,
            phantom: PhantomData,
        }
    }
}

impl<'v, 'a> Deref for PinBin<'v, 'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<'v, 'a> Drop for PinBin<'v, 'a> {
    fn drop(&mut self) {}
}

impl<'v, 'a> From<Bin<'v, 'a>> for Vec<u8> {
    fn from(value: Bin<'v, 'a>) -> Self {
        value.value.to_vec()
    }
}

/// View of a value that is not one of the standard types.
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

/// Array view
pub struct Array<'v, 'a>(pub(crate) GcObjBorrow<'v, 'a, array::Array<'v>>);

impl<'v, 'a> Array<'v, 'a> {
    pub(crate) fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, array::Array<'v>>) -> Self {
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

/// Dict view
pub struct Dict<'v, 'a>(pub(crate) GcObjBorrow<'v, 'a, dict::Dict<'v>>);

impl<'v, 'a> Dict<'v, 'a> {
    pub(crate) fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, dict::Dict<'v>>) -> Self {
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

    /// Write the value for `key` to `out`.
    ///
    /// Returns `true` if a matching entry was found. When multiple values exist
    /// for the same key, `instance` selects which one to fetch using the same
    /// indexing rules as the Do `dict.get` method. `None` selects the default
    /// instance.
    pub fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        key: impl Input<'v>,
        instance: Option<i64>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        let key = Value::from_input(strand, key);
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        match borrow.get(strand, &key, instance)? {
            Some(value) => {
                Output::set(strand, out, value);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Insert a key-value pair.
    ///
    /// When `unique` is `true`, any existing values for the same key are
    /// replaced by `value`. When `false`, the new pair is appended.
    pub fn insert<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        key: impl Input<'v>,
        value: impl Input<'v>,
        unique: bool,
    ) -> Result<'v, 's, ()> {
        let key = Value::from_input(strand, key);
        let value = Value::from_input(strand, value);
        let hv = kv::hash(strand, &key)?;
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.0.insert(strand, key, value, hv, unique);
        Ok(())
    }
}

/// Record view
pub struct Record<'v, 'a>(gc::Borrow<'v, 'a, Header, record::Record<'v>>);

impl<'v, 'a> Record<'v, 'a> {
    pub(crate) fn from_borrow(borrow: GcObjBorrow<'v, 'a, record::Record<'v>>) -> Self {
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

/// Tuple view
pub struct Tuple<'v, 'a>(gc::Borrow<'v, 'a, Header, [Value<'v>]>);

impl<'v, 'a> Tuple<'v, 'a> {
    pub(crate) fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, [Value<'v>]>) -> Self {
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

/// Type-discriminating view of a [`Value`].
pub enum View<'v, 'a> {
    /// Nil
    Nil,
    /// Bool
    Bool(bool),
    /// Int
    Int(i128),
    /// Float
    Float(f64),
    /// String
    Str(Str<'v, 'a>),
    /// Binary data
    Bin(Bin<'v, 'a>),
    /// Symbol
    Sym(Sym<'v, 'a>),
    /// Array
    Array(Array<'v, 'a>),
    /// Dict
    Dict(Dict<'v, 'a>),
    /// Record
    Record(Record<'v, 'a>),
    /// Tuple
    Tuple(Tuple<'v, 'a>),
    /// Any value which not match the standard types above.
    Object(ObjectView<'v, 'a>),
}

/// Iterator over a [`Dict`].
pub struct DictPairs<'v, 'a> {
    borrow: gc::Borrow<'v, 'a, Header, dict::Dict<'v>>,
    pos: usize,
}

impl<'v, 'a> DictPairs<'v, 'a> {
    /// Get the next key/value pair.
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
        Ok(kv_next_pair(&borrow.0, &mut self.pos, strand, key, value))
    }
}

/// Iterator over a [`Record`].
pub struct RecordPairs<'v, 'a> {
    borrow: GcObjBorrow<'v, 'a, record::Record<'v>>,
    pos: usize,
}

impl<'v, 'a> RecordPairs<'v, 'a> {
    /// Get the next key/value pair.
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
        Ok(kv_next_pair(&borrow.0, &mut self.pos, strand, key, value))
    }
}

fn kv_next_pair<'v>(
    inner: &kv::Inner<'v>,
    pos: &mut usize,
    alloc: &mut impl Alloc<'v>,
    key: impl Output<'v>,
    value: impl Output<'v>,
) -> bool {
    while let Some(slot) = inner.index.get(*pos) {
        *pos += 1;
        if let Some((bucket, subindex)) = slot {
            let entry: &Entry<'v> = unsafe { bucket.as_ref() };
            Output::set(alloc, key, &entry.key);
            Output::set(
                alloc,
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
