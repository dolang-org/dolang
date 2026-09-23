use std::{
    alloc::{self, Layout},
    borrow::Borrow,
    cell::Cell,
    fmt::Debug,
    hash::Hash,
    marker::PhantomData,
    num::NonZeroU32,
    ops::{Index, Range},
    ptr::NonNull,
    slice,
};

use crate::mono::{MonoHashMap, MonoVec};

/// An interned value's index, stored plus one so `Option<Id>` needs no tag.
pub struct Id<Tag>(NonZeroU32, PhantomData<*const Tag>);

unsafe impl<Tag> Sync for Id<Tag> {}
unsafe impl<Tag> Send for Id<Tag> {}

impl<Tag> Debug for Id<Tag> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == Self::INVALID {
            write!(f, "Id(<INVALID>)")
        } else {
            write!(f, "Id({})", self.index())
        }
    }
}

impl<Tag> PartialEq for Id<Tag> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<Tag> Eq for Id<Tag> {}

impl<Tag> PartialOrd for Id<Tag> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Tag> Ord for Id<Tag> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<Tag> Hash for Id<Tag> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<Tag> Clone for Id<Tag> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Tag> Copy for Id<Tag> {}

impl<Tag> Id<Tag> {
    /// A placeholder that no table issues.
    pub const INVALID: Self = Self(NonZeroU32::MAX, PhantomData);

    pub fn new(index: usize) -> Self {
        u32::try_from(index)
            .ok()
            .and_then(|i| i.checked_add(1))
            .and_then(NonZeroU32::new)
            .filter(|&raw| raw != NonZeroU32::MAX)
            .map(|raw| Self(raw, PhantomData))
            .expect("intern table too large")
    }

    pub fn index(&self) -> usize {
        self.0.get() as usize - 1
    }
}

pub struct Table<T, Tag> {
    map: MonoHashMap<T, ()>,
    phantom: PhantomData<Tag>,
}

impl<T, Tag> Table<T, Tag> {
    pub fn new() -> Self {
        Table {
            map: MonoHashMap::new(),
            phantom: PhantomData,
        }
    }

    pub fn id<Q>(&self, k: &Q) -> Id<Tag>
    where
        T: Hash + Eq + Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = T> + ?Sized,
    {
        Id::new(self.map.get_or_insert_index_with(k, |k| (k.to_owned(), ())))
    }

    /// Like [`id`](Self::id), but takes an owned value, which is dropped if an
    /// equal one is already interned.
    pub fn id_owned(&self, k: T) -> Id<Tag>
    where
        T: Hash + Eq,
    {
        match self.map.try_insert_index(k, ()) {
            Ok(i) | Err((i, ..)) => Id::new(i),
        }
    }

    /// Allocate a fresh `Id` without inserting into the reverse index.
    ///
    /// Two calls with the same `k` produce different `Id` values; the entry
    /// cannot be looked up by key.  Used for private symbols whose uniqueness
    /// must be preserved across separately-compiled modules.
    pub fn fresh(&self, k: T) -> Id<Tag> {
        Id::new(self.map.push_unindexed(k, ()))
    }

    pub fn is_fresh(&self, id: Id<Tag>) -> bool {
        !self.map.is_indexed(id.index())
    }

    pub fn get_by_index(&self, index: usize) -> Option<&T> {
        self.map.get_index(index).map(|(t, _)| t)
    }

    pub fn iter(&self) -> Iter<'_, T, Tag> {
        Iter {
            table: self,
            index: 0,
        }
    }
}

pub struct Iter<'a, T, Tag> {
    table: &'a Table<T, Tag>,
    index: usize,
}

impl<'a, T, Tag> Iterator for Iter<'a, T, Tag> {
    type Item = (Id<Tag>, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.index;
        let value = self.table.get_by_index(index)?;
        self.index += 1;
        Some((Id::new(index), value))
    }
}

impl<T, Tag> Index<Id<Tag>> for Table<T, Tag> {
    type Output = T;

    fn index(&self, index: Id<Tag>) -> &Self::Output {
        self.get_by_index(index.index())
            .expect("index out of bounds")
    }
}

impl<T, Tag> Default for Table<T, Tag> {
    fn default() -> Self {
        Self::new()
    }
}

struct BinTag;

/// A byte string interned in a [`BinTable`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BinId(Id<BinTag>);

/// A UTF-8 string interned in a [`BinTable`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct StrId(Id<BinTag>);

impl StrId {
    pub fn as_bin_id(self) -> BinId {
        BinId(self.0)
    }
}

/// A heap segment of a [`BinTable`], filled front to back.
struct Segment {
    ptr: NonNull<u8>,
    cap: usize,
    used: Cell<usize>,
}

impl Segment {
    fn new(cap: usize) -> Self {
        let layout = Layout::array::<u8>(cap).expect("segment too large");
        let ptr = NonNull::new(unsafe { alloc::alloc(layout) })
            .unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Segment {
            ptr,
            cap,
            used: Cell::new(0),
        }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.used.get()) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        unsafe { alloc::dealloc(self.ptr.as_ptr(), Layout::array::<u8>(self.cap).unwrap()) }
    }
}

/// Interned bytes within a segment of the owning [`BinTable`].
///
/// Never escapes the table, whose segments outlive it and never move.
struct Blob(NonNull<[u8]>);

impl Blob {
    fn bytes(&self) -> &[u8] {
        unsafe { self.0.as_ref() }
    }
}

impl Borrow<[u8]> for Blob {
    fn borrow(&self) -> &[u8] {
        self.bytes()
    }
}

impl Hash for Blob {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes().hash(state)
    }
}

impl PartialEq for Blob {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}

impl Eq for Blob {}

const MIN_SEGMENT: usize = 4096;

/// Interns byte strings through `&self`.
///
/// Each byte string is assigned a range of logical offsets in insertion order,
/// which [`flatten`](Self::flatten) lays out contiguously.
pub struct BinTable {
    segments: MonoVec<Segment>,
    // Maps each byte string to its logical start offset.
    index: MonoHashMap<Blob, usize>,
    len: Cell<usize>,
}

impl BinTable {
    pub fn new() -> Self {
        BinTable {
            segments: MonoVec::new(),
            index: MonoHashMap::new(),
            len: Cell::new(0),
        }
    }

    pub fn id(&self, bytes: &[u8]) -> BinId {
        let i = self.index.get_or_insert_index_with(bytes, |bytes| {
            let start = self.len.get();
            self.len.set(start + bytes.len());
            (self.store(bytes), start)
        });
        BinId(Id::new(i))
    }

    /// Intern a UTF-8 string and return a `StrId` witnessing its validity.
    pub fn id_str(&self, s: &str) -> StrId {
        StrId(self.id(s.as_bytes()).0)
    }

    /// Returns the logical offsets of `id`'s bytes.
    pub fn range(&self, id: BinId) -> Range<usize> {
        let (blob, &start) = self.entry(id);
        start..start + blob.bytes().len()
    }

    /// Copies every interned byte string into one buffer, at its logical
    /// offsets.
    pub fn flatten(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len.get());
        for segment in self.segments.iter() {
            out.extend_from_slice(segment.bytes());
        }
        out
    }

    fn entry(&self, id: BinId) -> (&Blob, &usize) {
        self.index
            .get_index(id.0.index())
            .expect("index out of bounds")
    }

    /// Copies `bytes` into the end of the last segment, starting a new one if
    /// it doesn't fit, so segments hold byte strings in insertion order.
    fn store(&self, bytes: &[u8]) -> Blob {
        let last = self
            .segments
            .len()
            .checked_sub(1)
            .map(|i| &self.segments[i]);
        let segment = match last {
            Some(segment) if segment.cap - segment.used.get() >= bytes.len() => segment,
            _ => {
                let cap = last
                    .map_or(0, |s| s.cap.saturating_mul(2))
                    .max(bytes.len())
                    .max(MIN_SEGMENT);
                self.segments.push(Segment::new(cap));
                &self.segments[self.segments.len() - 1]
            }
        };
        let used = segment.used.get();
        unsafe {
            // Only bytes past `used` are written, so no reference into the
            // segment overlaps them.
            let dst = segment.ptr.add(used);
            dst.copy_from_nonoverlapping(NonNull::from(bytes).cast(), bytes.len());
            segment.used.set(used + bytes.len());
            Blob(NonNull::slice_from_raw_parts(dst, bytes.len()))
        }
    }
}

impl Default for BinTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<BinId> for BinTable {
    type Output = [u8];

    fn index(&self, index: BinId) -> &Self::Output {
        self.entry(index).0.bytes()
    }
}

impl Index<StrId> for BinTable {
    type Output = str;

    fn index(&self, index: StrId) -> &Self::Output {
        // Safety: StrId is only constructable via `id_str`, which guarantees
        // the bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self[index.as_bin_id()]) }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn bin_table_offsets() {
        let table = BinTable::new();
        let big = vec![7u8; MIN_SEGMENT * 3];
        let a = table.id_str("hello");
        let b = table.id(&big);
        let c = table.id(b"");
        let d = table.id_str("world");
        assert_eq!(table.id_str("hello"), a);
        assert_eq!(table.id(&big), b);
        assert_eq!(&table[a], "hello");
        assert_eq!(&table[b], &big[..]);
        assert_eq!(&table[c], b"");
        let flat = table.flatten();
        for id in [a.as_bin_id(), b, c, d.as_bin_id()] {
            assert_eq!(&flat[table.range(id)], &table[id]);
        }
        assert_eq!(flat.len(), 10 + big.len());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn bin_table_many() {
        let table = BinTable::new();
        let ids: Vec<_> = (0..10_000).map(|i| table.id_str(&i.to_string())).collect();
        let flat = table.flatten();
        for (i, id) in ids.into_iter().enumerate() {
            assert_eq!(&table[id], i.to_string());
            assert_eq!(&flat[table.range(id.as_bin_id())], i.to_string().as_bytes());
        }
    }
}
