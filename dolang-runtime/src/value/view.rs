use std::{
    fmt::{self, Display, Formatter},
    hash::{DefaultHasher, Hash, Hasher},
    marker::PhantomData,
    mem,
    num::NonZero,
    ops::Deref,
    ptr::NonNull,
};

use crate::{
    error::{Error, Result},
    gc,
    object::{
        array,
        dict::{self, Entry, EntryValue},
        native::Cast,
        protocol::{GcObjBorrow, Header},
        range, record, set,
    },
    stdlib::fmt as stdfmt,
    strand::{Access, Strand},
    sym::Sym,
    value::{Input, Output, Slot, Value, fmt::Spec},
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

/// Typed view of a `Str` value.
#[derive(Clone, Copy)]
pub struct Str<'v, 'a> {
    value: &'a str,
    phantom: PhantomData<&'v mut &'v ()>,
}

/// Pinned `Str` view
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

impl<'v, 'a> PinStr<'v, 'a> {
    /// Widen the borrow lifetime of this pin without changing its dynamic pin state.
    ///
    /// This pin guarantees only address stability of the underlying string until it is
    /// dropped. It does not guarantee rootedness or liveness of the underlying GC object.
    ///
    /// # Safety
    /// The caller must ensure the underlying string object remains rooted and alive for the
    /// full widened lifetime, and that any references derived from the widened pin are dropped
    /// before the pin.
    pub unsafe fn into_static_unchecked(self) -> PinStr<'v, 'static> {
        unsafe { mem::transmute::<PinStr<'v, 'a>, PinStr<'v, 'static>>(self) }
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

/// Typed view of a `Bin` value.
#[derive(Clone, Copy)]
pub struct Bin<'v, 'a> {
    value: &'a [u8],
    phantom: PhantomData<&'v mut &'v ()>,
}

/// Pinned `Bin` view
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

    /// Get length of `Bin`
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Is the the `Bin` empty?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Convert to owned [`Vec`]
    pub fn to_vec(&self) -> Vec<u8> {
        self.value.to_owned()
    }

    /// Get pinned view of `Bin`
    pub fn pin(&self) -> PinBin<'v, 'a> {
        PinBin {
            value: self.value,
            phantom: PhantomData,
        }
    }
}

impl<'v, 'a> PinBin<'v, 'a> {
    /// Widen the borrow lifetime of this pin without changing its dynamic pin state.
    ///
    /// This pin guarantees only address stability of the underlying binary until it is dropped.
    /// It does not guarantee rootedness or liveness of the underlying GC object.
    ///
    /// # Safety
    /// The caller must ensure the underlying binary object remains rooted and alive for the
    /// full widened lifetime, and that any references derived from the widened pin are dropped
    /// before the pin.
    pub unsafe fn into_static_unchecked(self) -> PinBin<'v, 'static> {
        unsafe { mem::transmute::<PinBin<'v, 'a>, PinBin<'v, 'static>>(self) }
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

/// Read-only view of a `range` value.
pub struct Range<'v, 'a>(pub(crate) GcObjBorrow<'v, 'a, range::Range<'v>>);

impl<'v, 'a> Range<'v, 'a> {
    /// Copy the start, end, and step into rooted output slots.
    pub fn parts<'s>(&self, strand: &mut Strand<'v, 's>, [start, end, step]: [Slot<'v, '_>; 3]) {
        let (start_value, end_value, step_value) = self.0.get().parts();
        Output::set(strand, start, start_value);
        Output::set(strand, end, end_value);
        Output::set(strand, step, step_value);
    }
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

    /// Append values from rooted slots. Briefly takes an exclusive interior borrow.
    pub fn push_all<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        values: &mut [Slot<'v, '_>],
    ) -> Result<'v, 's, ()> {
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.inner.extend(values.iter_mut().map(Slot::take));
        Ok(())
    }

    /// Insert values at `index`, shifting later elements up. Returns `false`
    /// if `index` is not a valid insertion position.
    pub fn insert<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        values: &mut [Slot<'v, '_>],
    ) -> Result<'v, 's, bool> {
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        if index > borrow.inner.len() {
            return Ok(false);
        }
        borrow
            .inner
            .splice(index..index, values.iter_mut().map(Slot::take));
        Ok(true)
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

    /// Remove and write the element at `index` to `out`. Returns `false` if
    /// out of bounds.
    pub fn pop_at<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        let val = {
            let mut borrow = match self.0.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            if index >= borrow.inner.len() {
                return Ok(false);
            }
            borrow.inner.remove(index)
        };
        Output::set(strand, out, &val);
        Ok(true)
    }

    /// Remove the element at `index`, shifting later elements down. Returns
    /// `false` if out of bounds.
    pub fn delete<'s>(&self, strand: &mut Strand<'v, 's>, index: usize) -> Result<'v, 's, bool> {
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        if index >= borrow.inner.len() {
            return Ok(false);
        }
        borrow.inner.remove(index);
        Ok(true)
    }

    /// Remove all elements.
    pub fn clear<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.inner.clear();
        Ok(())
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
        Ok(borrow.total_pairs)
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
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher)?;
        let hv = hasher.finish();
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.insert(strand, key, value, hv, unique);
        Ok(())
    }
}

/// Set view
pub struct Set<'v, 'a>(pub(crate) GcObjBorrow<'v, 'a, set::Set<'v>>);

impl<'v, 'a> Set<'v, 'a> {
    pub(crate) fn from_borrow(borrow: gc::Borrow<'v, 'a, Header, set::Set<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this set for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(self.0.into_raw().cast(), PhantomData)
    }

    /// Number of members.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        Ok(borrow.len())
    }

    /// Return a stateful cursor over the members in insertion order.
    pub fn members(&self) -> SetMembers<'v, 'a> {
        SetMembers {
            borrow: self.0,
            pos: 0,
        }
    }

    /// Is `value` a member?
    pub fn contains<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        let value = Value::from_input(strand, value);
        let mut hasher = DefaultHasher::new();
        value.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        let borrow = match self.0.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.contains(strand, &value, hash)
    }

    /// Add `value`. Returns `false` if it was already a member, leaving the
    /// position it was first added in alone.
    pub fn add<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        // Build the value and hash it before taking the exclusive borrow.
        let value = Value::from_input(strand, value);
        let mut hasher = DefaultHasher::new();
        value.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.insert(strand, value, hash)
    }

    /// Remove `value`. Returns `false` if it was not a member.
    pub fn delete<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        let value = Value::from_input(strand, value);
        let mut hasher = DefaultHasher::new();
        value.op_hash(strand, &mut hasher)?;
        let hash = hasher.finish();
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.delete(strand, &value, hash)
    }

    /// Remove every member.
    pub fn clear<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let mut borrow = match self.0.borrow_mut() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        borrow.clear();
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
        Ok(borrow.items().len())
    }

    /// Return a stateful cursor over the key-value pairs in order. A positional
    /// item's key is its position.
    pub fn pairs(&self) -> RecordPairs<'v, 'a> {
        RecordPairs {
            borrow: self.0,
            pos: 0,
            int: 0,
        }
    }
}

/// Tuple view
pub struct Tuple<'v, 'a>(pub(super) gc::Borrow<'v, 'a, Header, [Value<'v>]>);

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

/// Formatted sequence view
///
/// A sequence is an immutable, ordered run of segments, each of which is a
/// `Str` of literal program text, a [`FmtValue`], or a [`FmtParam`]. Nothing
/// else can appear, so a consumer that recognizes those three has covered the
/// sequence.
pub struct Fmt<'v, 'a>(Cast<'v, 'a, stdfmt::Fmt>);

impl<'v, 'a> Fmt<'v, 'a> {
    pub(crate) fn from_cast(cast: Cast<'v, 'a, stdfmt::Fmt>) -> Self {
        Self(cast)
    }

    /// Number of segments.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0
            .enter_sync(strand, |strand, this| stdfmt::view_fmt_len(this, strand))
    }

    /// Write segment `index` to `out`. Returns `false` if out of bounds.
    pub fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.enter_sync(strand, |strand, this| {
            stdfmt::view_fmt_segment(this, strand, index, out)
        })
    }
}

/// Bound interpolation view
///
/// One segment of a [`Fmt`]: a value to be formatted, and the specification to
/// format it under.
pub struct FmtValue<'v, 'a>(Cast<'v, 'a, stdfmt::FmtValue>);

impl<'v, 'a> FmtValue<'v, 'a> {
    pub(crate) fn from_cast(cast: Cast<'v, 'a, stdfmt::FmtValue>) -> Self {
        Self(cast)
    }

    /// Write the bound value to `out`.
    pub fn value<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.0.enter_sync(strand, |strand, this| {
            stdfmt::view_value_bound(this, strand, out)
        })
    }

    /// The formatting specification.
    pub fn spec(&self, strand: &mut Strand<'v, '_>) -> Spec {
        self.0.enter_sync(strand, |_, this| stdfmt::view_spec(this))
    }

    /// The text this was written as, or [`None`] when it was built at runtime
    /// and so has no source.
    pub fn source<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Option<String>> {
        self.0
            .enter_sync(strand, |strand, this| stdfmt::view_source(this, strand))
    }
}

/// Unbound parameter view
///
/// One segment of a [`Fmt`]: a hole named by an `Int` or a `Sym`, and the
/// specification it will impose once filled.
pub struct FmtParam<'v, 'a>(Cast<'v, 'a, stdfmt::FmtParam>);

impl<'v, 'a> FmtParam<'v, 'a> {
    pub(crate) fn from_cast(cast: Cast<'v, 'a, stdfmt::FmtParam>) -> Self {
        Self(cast)
    }

    /// Write the parameter's name to `out`.
    pub fn name<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.0.enter_sync(strand, |strand, this| {
            stdfmt::view_param_name(this, strand, out)
        })
    }

    /// The formatting specification.
    pub fn spec(&self, strand: &mut Strand<'v, '_>) -> Spec {
        self.0.enter_sync(strand, |_, this| stdfmt::view_spec(this))
    }

    /// The text this was written as, or [`None`] when it was built at runtime
    /// and so has no source.
    pub fn source<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Option<String>> {
        self.0
            .enter_sync(strand, |strand, this| stdfmt::view_source(this, strand))
    }
}

/// Type-discriminating view of a [`Value`].
///
/// New variants may be added as more types gain a view, so a consumer outside
/// this crate must decide what an unrecognized one means rather than being
/// silently broken by it.
#[non_exhaustive]
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
    /// Set
    Set(Set<'v, 'a>),
    /// Record
    Record(Record<'v, 'a>),
    /// Tuple
    Tuple(Tuple<'v, 'a>),
    /// Formatted sequence
    Fmt(Fmt<'v, 'a>),
    /// Bound interpolation
    FmtValue(FmtValue<'v, 'a>),
    /// Unbound parameter
    FmtParam(FmtParam<'v, 'a>),
    /// Formatting specification
    FmtSpec(Spec),
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
        Ok(dict_next_pair(&borrow, &mut self.pos, strand, key, value))
    }
}

/// Iterator over a [`Set`].
pub struct SetMembers<'v, 'a> {
    borrow: gc::Borrow<'v, 'a, Header, set::Set<'v>>,
    pos: usize,
}

impl<'v, 'a> SetMembers<'v, 'a> {
    /// Get the next member.
    /// Returns `false` when all members have been yielded.
    pub fn next<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        value: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        let borrow = match self.borrow.borrow() {
            Some(b) => b,
            None => return Err(Error::concurrency(strand)),
        };
        match borrow.next_from(&mut self.pos) {
            Some(member) => {
                Output::set(strand, value, member);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Iterator over a [`Record`].
pub struct RecordPairs<'v, 'a> {
    borrow: GcObjBorrow<'v, 'a, record::Record<'v>>,
    pos: usize,
    int: i64,
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
        let Some((item_key, item_value)) = borrow.items().get(self.pos) else {
            return Ok(false);
        };
        self.pos += 1;
        match item_key {
            Some(item_key) => Output::set(strand, key, unsafe { Sym::from_obj(item_key) }),
            None => {
                Output::set(strand, key, self.int);
                self.int += 1;
            }
        }
        Output::set(strand, value, item_value);
        Ok(true)
    }
}

fn dict_next_pair<'v>(
    dict: &dict::Dict<'v>,
    pos: &mut usize,
    alloc: &mut impl Alloc<'v>,
    key: impl Output<'v>,
    value: impl Output<'v>,
) -> bool {
    while let Some(slot) = dict.index.get(*pos) {
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
