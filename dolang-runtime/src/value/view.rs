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
        array,
        bin::BinObj,
        dict::{self, Entry, EntryValue},
        native::Cast,
        pack::{self, ArgItem},
        protocol::{GcObj, Header},
        range, record, set,
        str::StrObj,
        structure, tuple,
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
pub struct Str<'v, 'a>(&'a GcObj<'v, StrObj>);

/// Pinned `Str`.
///
/// Owns a reference to the string, which keeps it alive and its text address-stable
/// until this pin is dropped.  A native object can hold one alongside a structure
/// borrowing the text, provided the borrower is dropped first.
#[derive(Clone)]
pub struct PinStr<'v>(GcObj<'v, StrObj>);

impl<'v, 'a> Str<'v, 'a> {
    pub(crate) fn from_obj(obj: &'a GcObj<'v, StrObj>) -> Self {
        Self(obj)
    }

    /// Get underlying string slice.
    ///
    /// This requires a token from [`Strand::access`].
    pub fn as_str<'s, 'b>(&self, access: &'b Access<'v, 's>) -> &'b str
    where
        'a: 'b,
    {
        self.0.pin_access(access).str()
    }

    /// Call `f` with the underlying string slice.  Unlike [`Self::as_str`], `f` keeps
    /// the strand.
    pub fn with_str<'s, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut Strand<'v, 's>, &str) -> R,
    ) -> R {
        self.0.pin_sync(strand, |strand, pin| f(strand, pin.str()))
    }

    /// Async counterpart to [`Self::with_str`].
    pub async fn with_str_async<'s, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>, &str) -> R,
    ) -> R {
        // SAFETY: scoped to `f`, as for `GcObj::pin`, inlined to keep futures shallow
        f(strand, unsafe { self.0.pin_unchecked() }.str()).await
    }

    /// Get length of string
    pub fn len(&self) -> usize {
        self.0.read(|p| p.str().len())
    }

    /// Is string empty?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get pinned string
    pub fn pin(&self) -> PinStr<'v> {
        PinStr(self.0.clone())
    }
}

impl<'v, 'a> Display for Str<'v, 'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // Write through a held pin; there's no strand for a scoped one
        Display::fmt(&*self.pin(), f)
    }
}

impl<'v> PinStr<'v> {
    pub(crate) fn new(obj: GcObj<'v, StrObj>) -> Self {
        Self(obj)
    }

    /// The pinned string object.
    pub(crate) fn obj(&self) -> &GcObj<'v, StrObj> {
        &self.0
    }
}

impl<'v> Deref for PinStr<'v> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        // SAFETY: this pin holds the string pinned until it is dropped
        unsafe { self.0.pin_unchecked() }.str()
    }
}

impl<'v, 'a> From<Str<'v, 'a>> for String {
    fn from(value: Str<'v, 'a>) -> Self {
        value.0.read(|p| p.str().to_owned())
    }
}

/// Typed view of a `Bin` value.
#[derive(Clone, Copy)]
pub struct Bin<'v, 'a>(&'a GcObj<'v, BinObj>);

/// Pinned `Bin`.
///
/// As [`PinStr`], for bytes.
#[derive(Clone)]
pub struct PinBin<'v>(GcObj<'v, BinObj>);

impl<'v, 'a> Bin<'v, 'a> {
    pub(crate) fn from_obj(obj: &'a GcObj<'v, BinObj>) -> Self {
        Self(obj)
    }

    /// Get underlying byte slice.
    ///
    /// This requires a token from [`Strand::access`].
    pub fn as_slice<'s, 'b>(&self, access: &'b Access<'v, 's>) -> &'b [u8]
    where
        'a: 'b,
    {
        self.0.pin_access(access).bytes()
    }

    /// Call `f` with the underlying byte slice.  Unlike [`Self::as_slice`], `f` keeps
    /// the strand.
    pub fn with_slice<'s, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        f: impl FnOnce(&mut Strand<'v, 's>, &[u8]) -> R,
    ) -> R {
        self.0
            .pin_sync(strand, |strand, pin| f(strand, pin.bytes()))
    }

    /// Async counterpart to [`Self::with_slice`].
    pub async fn with_slice_async<'s, R>(
        &self,
        strand: &mut Strand<'v, 's>,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>, &[u8]) -> R,
    ) -> R {
        // SAFETY: scoped to `f`, as for `GcObj::pin`, inlined to keep futures shallow
        f(strand, unsafe { self.0.pin_unchecked() }.bytes()).await
    }

    /// Get length of `Bin`
    pub fn len(&self) -> usize {
        self.0.read(|p| p.bytes().len())
    }

    /// Is the the `Bin` empty?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Convert to owned [`Vec`]
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.read(|p| p.bytes().to_vec())
    }

    /// Get pinned bytes
    pub fn pin(&self) -> PinBin<'v> {
        PinBin(self.0.clone())
    }
}

impl<'v> PinBin<'v> {
    pub(crate) fn new(obj: GcObj<'v, BinObj>) -> Self {
        Self(obj)
    }

    /// The pinned binary object.
    pub(crate) fn obj(&self) -> &GcObj<'v, BinObj> {
        &self.0
    }
}

impl<'v> Deref for PinBin<'v> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: this pin holds the bytes pinned until it is dropped
        unsafe { self.0.pin_unchecked() }.bytes()
    }
}

impl<'v, 'a> From<Bin<'v, 'a>> for Vec<u8> {
    fn from(value: Bin<'v, 'a>) -> Self {
        value.to_vec()
    }
}

/// View of a value that is not one of the standard types.
pub struct ObjectView<'v, 'a> {
    ptr: NonNull<Header>,
    phantom: PhantomData<(&'v mut &'v (), &'a Header)>,
}

/// Read-only view of a `range` value.
pub struct Range<'v, 'a>(pub(crate) &'a GcObj<'v, range::RangeObj<'v>>);

impl<'v, 'a> Range<'v, 'a> {
    /// Copy the start, end, and step into rooted output slots.
    pub fn parts<'s>(&self, strand: &mut Strand<'v, 's>, [start, end, step]: [Slot<'v, '_>; 3]) {
        self.0.pin_sync(strand, |strand, pin| {
            let (start_value, end_value, step_value) = pin.get().parts();
            Output::set(strand, start, start_value);
            Output::set(strand, end, end_value);
            Output::set(strand, step, step_value);
        })
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
pub struct Array<'v, 'a>(pub(crate) &'a GcObj<'v, array::Array<'v>>);

impl<'v, 'a> Array<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, array::Array<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this array for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        // BasePin is Copy, so this copies and extracts the inner pointer.
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Number of elements. Briefly takes a shared interior borrow.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(borrow.inner.len())
        })
    }

    /// Get element at `index`. Returns `false` if out of bounds.
    pub fn get<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
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
        })
    }

    /// Write `value` to `index`. Returns `false` if out of bounds.
    pub fn set<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
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
        })
    }

    /// Append a value drawn from `input`. Briefly takes an exclusive interior borrow.
    pub fn push<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        input: impl Input<'v>,
    ) -> Result<'v, 's, ()> {
        self.0.pin_sync(strand, |strand, pin| {
            // Build the value before taking the exclusive borrow.
            let value = Value::from_input(strand, input);
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.inner.push(value);
            Ok(())
        })
    }

    /// Append values from rooted slots. Briefly takes an exclusive interior borrow.
    pub fn push_all<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        values: &mut [Slot<'v, '_>],
    ) -> Result<'v, 's, ()> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.inner.extend(values.iter_mut().map(Slot::take));
            Ok(())
        })
    }

    /// Insert values at `index`, shifting later elements up. Returns `false`
    /// if `index` is not a valid insertion position.
    pub fn insert<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        values: &mut [Slot<'v, '_>],
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
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
        })
    }

    /// Remove and write the last element to `out`. Returns `false` if empty.
    pub fn pop<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            // Remove before calling Output::set, mirroring push's pattern.
            let val = {
                let mut borrow = match pin.borrow_mut() {
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
        })
    }

    /// Remove and write the element at `index` to `out`. Returns `false` if
    /// out of bounds.
    pub fn pop_at<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        index: usize,
        out: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let val = {
                let mut borrow = match pin.borrow_mut() {
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
        })
    }

    /// Remove the element at `index`, shifting later elements down. Returns
    /// `false` if out of bounds.
    pub fn delete<'s>(&self, strand: &mut Strand<'v, 's>, index: usize) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            if index >= borrow.inner.len() {
                return Ok(false);
            }
            borrow.inner.remove(index);
            Ok(true)
        })
    }

    /// Remove all elements.
    pub fn clear<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.inner.clear();
            Ok(())
        })
    }

    /// Reverse the elements in place.
    pub fn reverse<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.inner.reverse();
            Ok(())
        })
    }

    /// Write a tuple of the current elements to `out`.
    pub fn to_tuple<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.0.pin_sync(strand, |strand, pin| {
            let tuple = {
                let borrow = match pin.borrow() {
                    Some(b) => b,
                    None => return Err(Error::concurrency(strand)),
                };
                tuple::tuple(strand.vm(), borrow.inner.iter().map(Value::dup))
            };
            Output::set(strand, out, Value::from_object(tuple));
            Ok(())
        })
    }
}

/// Dict view
pub struct Dict<'v, 'a>(pub(crate) &'a GcObj<'v, dict::Dict<'v>>);

impl<'v, 'a> Dict<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, dict::Dict<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this dict for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Total number of key-value pairs (counting duplicate keys).
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(borrow.total_pairs)
        })
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
        self.0.pin_sync(strand, |strand, pin| {
            let key = Value::from_input(strand, key);
            let borrow = match pin.borrow() {
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
        })
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
        self.0.pin_sync(strand, |strand, pin| {
            let key = Value::from_input(strand, key);
            let value = Value::from_input(strand, value);
            let mut hasher = strand.vm().hasher();
            key.op_hash(strand, &mut hasher)?;
            let hv = hasher.finish();
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.insert(strand, key, value, hv, unique)?;
            Ok(())
        })
    }

    /// Delete all values for `key`. Returns whether the key was present.
    pub fn delete<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        key: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let key = Value::from_input(strand, key);
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.delete(strand, &key)
        })
    }
}

/// Set view
pub struct Set<'v, 'a>(pub(crate) &'a GcObj<'v, set::Set<'v>>);

impl<'v, 'a> Set<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, set::Set<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this set for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Number of members.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(borrow.len())
        })
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
        self.0.pin_sync(strand, |strand, pin| {
            let value = Value::from_input(strand, value);
            let mut hasher = strand.vm().hasher();
            value.op_hash(strand, &mut hasher)?;
            let hash = hasher.finish();
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.contains(strand, &value, hash)
        })
    }

    /// Add `value`. Returns `false` if it was already a member, leaving the
    /// position it was first added in alone.
    pub fn add<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            // Build the value and hash it before taking the exclusive borrow.
            let value = Value::from_input(strand, value);
            let mut hasher = strand.vm().hasher();
            value.op_hash(strand, &mut hasher)?;
            let hash = hasher.finish();
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.insert(strand, value, hash)
        })
    }

    /// Remove `value`. Returns `false` if it was not a member.
    pub fn delete<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        value: impl Input<'v>,
    ) -> Result<'v, 's, bool> {
        self.0.pin_sync(strand, |strand, pin| {
            let value = Value::from_input(strand, value);
            let mut hasher = strand.vm().hasher();
            value.op_hash(strand, &mut hasher)?;
            let hash = hasher.finish();
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.delete(strand, &value, hash)
        })
    }

    /// Remove every member.
    pub fn clear<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        self.0.pin_sync(strand, |strand, pin| {
            let mut borrow = match pin.borrow_mut() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            borrow.clear();
            Ok(())
        })
    }
}

/// [`Pack`](crate::object::pack::Pack) view
pub struct Pack<'v, 'a>(&'a GcObj<'v, pack::PackObj<'v>>);

impl<'v, 'a> Pack<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, pack::PackObj<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this args value for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Total number of key-value pairs.
    pub fn len<'s>(&self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0
            .pin_sync(_strand, |_strand, pin| Ok(pin.get().items().len()))
    }

    /// Return a stateful cursor over the key-value pairs in order. A positional
    /// item's key is its position.
    pub fn pairs(&self) -> PackPairs<'v, 'a> {
        PackPairs {
            borrow: self.0,
            pos: 0,
            int: 0,
        }
    }
}

/// [`Record`](crate::object::record::Record) view
pub struct Record<'v, 'a>(&'a GcObj<'v, record::Record<'v>>);

impl<'v, 'a> Record<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, record::Record<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this record for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Total number of items.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(borrow.len())
        })
    }

    /// Return a stateful cursor over the key-value pairs in order. A positional
    /// item's key is its position.
    pub fn pairs(&self) -> RecordPairs<'v, 'a> {
        RecordPairs {
            borrow: self.0,
            slot: 0,
            int: 0,
        }
    }
}

/// Struct view
pub struct Struct<'v, 'a>(&'a GcObj<'v, structure::Struct<'v>>);

impl<'v, 'a> Struct<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, structure::Struct<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this struct for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Total number of key-value pairs.
    pub fn len<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, usize> {
        self.0.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(borrow.items().len())
        })
    }

    /// Return a stateful cursor over the key-value pairs in order. A positional
    /// item's key is its position.
    pub fn pairs(&self) -> StructPairs<'v, 'a> {
        StructPairs {
            borrow: self.0,
            pos: 0,
            int: 0,
        }
    }
}

/// Tuple view
pub struct Tuple<'v, 'a>(pub(super) &'a GcObj<'v, tuple::TupleObj<'v>>);

impl<'v, 'a> Tuple<'v, 'a> {
    pub(crate) fn from_borrow(borrow: &'a GcObj<'v, tuple::TupleObj<'v>>) -> Self {
        Self(borrow)
    }

    /// Return the opaque identity of this tuple for cycle detection.
    pub fn id(&self) -> ObjectId<'v, 'a> {
        ObjectId(gc::Base::as_ptr(self.0).cast(), PhantomData)
    }

    /// Number of elements. Tuples are fixed-size immutable slices; no borrow needed.
    pub fn len(&self) -> usize {
        self.0.read(|p| p.items().len())
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
        self.0
            .pin_sync(strand, |strand, pin| match pin.items().get(index) {
                Some(v) => {
                    Output::set(strand, out, v);
                    Ok(true)
                }
                None => Ok(false),
            })
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
    /// [`Pack`] view
    Pack(Pack<'v, 'a>),
    /// [`Record`] view
    Record(Record<'v, 'a>),
    /// Struct
    Struct(Struct<'v, 'a>),
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
    borrow: &'a GcObj<'v, dict::Dict<'v>>,
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
        self.borrow.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(dict_next_pair(&borrow, &mut self.pos, strand, key, value))
        })
    }
}

/// Iterator over a [`Set`].
pub struct SetMembers<'v, 'a> {
    borrow: &'a GcObj<'v, set::Set<'v>>,
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
        self.borrow.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
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
        })
    }
}

/// Iterator over a [`Pack`].
pub struct PackPairs<'v, 'a> {
    borrow: &'a GcObj<'v, pack::PackObj<'v>>,
    pos: usize,
    int: i64,
}

impl<'v, 'a> PackPairs<'v, 'a> {
    /// Get the next key/value pair.
    /// Returns `false` when all pairs have been yielded.
    pub fn next<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: impl Output<'v>,
        value: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.borrow.pin_sync(strand, |strand, pin| {
            let borrow = pin.get();
            Ok(next_item_pair(
                borrow.items(),
                &mut self.pos,
                &mut self.int,
                strand,
                key,
                value,
            ))
        })
    }
}

/// Iterator over a [`Record`].
pub struct RecordPairs<'v, 'a> {
    borrow: &'a GcObj<'v, record::Record<'v>>,
    slot: usize,
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
        self.borrow.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            let order = borrow.order();
            while let Some(item) = order.get(self.slot) {
                self.slot += 1;
                let Some((item_key, item_value)) = item else {
                    continue;
                };
                match item_key {
                    Some(item_key) => Output::set(strand, key, unsafe { Sym::from_obj(item_key) }),
                    None => {
                        Output::set(strand, key, self.int);
                        self.int += 1;
                    }
                }
                Output::set(strand, value, item_value);
                return Ok(true);
            }
            Ok(false)
        })
    }
}

/// Iterator over a [`Struct`].
pub struct StructPairs<'v, 'a> {
    borrow: &'a GcObj<'v, structure::Struct<'v>>,
    pos: usize,
    int: i64,
}

impl<'v, 'a> StructPairs<'v, 'a> {
    /// Get the next key/value pair.
    /// Returns `false` when all pairs have been yielded.
    pub fn next<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        key: impl Output<'v>,
        value: impl Output<'v>,
    ) -> Result<'v, 's, bool> {
        self.borrow.pin_sync(strand, |strand, pin| {
            let borrow = match pin.borrow() {
                Some(b) => b,
                None => return Err(Error::concurrency(strand)),
            };
            Ok(next_item_pair(
                borrow.items(),
                &mut self.pos,
                &mut self.int,
                strand,
                key,
                value,
            ))
        })
    }
}

/// Outputs the key and value of the pack or struct item at `pos`, if any,
/// and advances past it. `int` counts the positional items so far.
fn next_item_pair<'v>(
    items: &[ArgItem<'v>],
    pos: &mut usize,
    int: &mut i64,
    alloc: &mut impl Alloc<'v>,
    key: impl Output<'v>,
    value: impl Output<'v>,
) -> bool {
    let Some((item_key, item_value)) = items.get(*pos) else {
        return false;
    };
    *pos += 1;
    match item_key {
        Some(item_key) => Output::set(alloc, key, unsafe { Sym::from_obj(item_key) }),
        None => {
            Output::set(alloc, key, *int);
            *int += 1;
        }
    }
    Output::set(alloc, value, item_value);
    true
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
