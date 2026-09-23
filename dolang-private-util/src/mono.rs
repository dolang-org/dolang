//! Collections that grow monotonically through shared references.

use std::{
    alloc::{self, Layout, LayoutError},
    borrow::Borrow,
    cell::{RefCell, UnsafeCell},
    hash::{BuildHasher, Hash, RandomState},
    marker::PhantomData,
    mem::needs_drop,
    ops::{Index, IndexMut},
    ptr::{NonNull, copy_nonoverlapping, drop_in_place},
};

use crate::hashbrown::raw::RawTable;

pub struct MonoVec<T> {
    inner: UnsafeCell<Inner<T, 0>>,
}

struct Inner<T, const C: usize> {
    _data: PhantomData<T>,
    index: NonNull<*mut u8>,
    len: usize,
    chunks: u32,
}

const fn expect<V: Copy>(res: Result<V, LayoutError>) -> V {
    match res {
        Ok(v) => v,
        Err(_) => panic!("Overflow in memory layout"),
    }
}

const fn max(l: usize, r: usize) -> usize {
    if l > r { l } else { r }
}

const fn elem_layout<T>() -> Layout {
    if size_of::<T>() == 0 {
        unsafe { Layout::from_size_align_unchecked(align_of::<T>(), align_of::<T>()) }
    } else {
        Layout::new::<T>()
    }
}

const unsafe fn elem_add<T>(p: *mut T, offset: usize) -> *mut T {
    unsafe { (p as *mut u8).add(elem_layout::<T>().size().checked_mul(offset).unwrap()) as *mut T }
}

const fn default_base_cap<T>() -> usize {
    expect(elem_layout::<T>().extend(Layout::new::<*mut u8>()))
        .0
        .size()
        .div_ceil(elem_layout::<T>().size())
}

impl<T, const C: usize> Inner<T, C> {
    const fn log_base_cap() -> u32 {
        if C == 0 { default_base_cap::<T>() } else { C }
            .checked_next_power_of_two()
            .unwrap()
            .ilog2()
    }

    const fn data_layout(count: usize) -> Layout {
        if size_of::<T>() == 0 {
            unsafe {
                Layout::from_size_align_unchecked(
                    align_of::<T>().checked_mul(count).unwrap(),
                    align_of::<T>(),
                )
            }
        } else {
            expect(Layout::array::<T>(count))
        }
    }

    const fn index_layout(count: usize) -> Layout {
        expect(Layout::array::<*mut u8>(count))
    }

    const fn chunk_layout(index: u32) -> (Layout, usize) {
        let count = index as usize + 1;
        let cap = 1usize.checked_shl(Self::log_base_cap() + index).unwrap();
        let index_layout = Self::index_layout(count);
        let data_layout =
            expect(Self::data_layout(cap).align_to(index_layout.align())).pad_to_align();
        let layout = unsafe {
            Layout::from_size_align_unchecked(
                max(index_layout.size(), data_layout.size()),
                data_layout.align(),
            )
        };
        (layout, layout.size() - index_layout.size())
    }

    const fn chunk_of(index: usize) -> u32 {
        ((index >> Self::log_base_cap()) + 1).ilog2()
    }

    const fn chunk_base(chunk: u32) -> usize {
        ((1 << (chunk as usize)) - 1) << Self::log_base_cap()
    }

    const fn coord_of(index: usize) -> (u32, usize) {
        let chunk = Self::chunk_of(index);
        (chunk, index - Self::chunk_base(chunk))
    }

    unsafe fn ensure_chunk(&mut self, chunk: u32) {
        while self.chunks <= chunk {
            let (layout, offset) = Self::chunk_layout(self.chunks);
            unsafe {
                let chunk = alloc::alloc(layout);
                if chunk.is_null() {
                    alloc::handle_alloc_error(layout);
                }
                let new_index = chunk.add(offset) as *mut *mut u8;
                if self.chunks != 0 {
                    copy_nonoverlapping(self.index.as_ptr(), new_index, self.chunks as usize);
                }
                new_index.add(self.chunks as usize).write(chunk);
                self.index = NonNull::new_unchecked(new_index);
                self.chunks += 1;
            }
        }
    }

    unsafe fn ensure_index(&mut self, index: usize) {
        let (chunk, subi) = Self::coord_of(index);
        unsafe { self.ensure_chunk(chunk) };
        if chunk + 1 == self.chunks {
            let (_, index_offset) = Self::chunk_layout(chunk);
            if (subi + 1) * elem_layout::<T>().size() > index_offset {
                unsafe { self.ensure_chunk(chunk + 1) };
            }
        }
    }
}

struct Drain<'a, T> {
    inner: &'a mut Inner<T, 0>,
    len: usize,
    index: usize,
}

impl<'a, T> Iterator for Drain<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.len == self.index {
            return None;
        }
        let (chunk, offset) = Inner::<T, 0>::coord_of(self.index);
        self.index += 1;
        unsafe {
            let data = self.inner.index.add(chunk as usize).read() as *mut T;
            Some(elem_add(data, offset).read())
        }
    }
}

impl<'a, T> Drop for Drain<'a, T> {
    fn drop(&mut self) {
        for _ in self {}
    }
}

impl<T> MonoVec<T> {
    unsafe fn inner(&self) -> &Inner<T, 0> {
        unsafe { &*self.inner.get() }
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn inner_mut(&self) -> &mut Inner<T, 0> {
        unsafe { &mut *self.inner.get() }
    }

    pub fn len(&self) -> usize {
        unsafe { self.inner() }.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        let chunks = unsafe { self.inner() }.chunks;
        Inner::<T, 0>::chunk_base(chunks)
            - Inner::<T, 0>::index_layout(chunks as usize)
                .size()
                .div_ceil(elem_layout::<T>().size())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let vec = Self {
            inner: UnsafeCell::new(Inner {
                _data: PhantomData,
                index: NonNull::dangling(),
                len: 0,
                chunks: 0,
            }),
        };
        if capacity != 0 {
            vec.reserve(capacity)
        }
        vec
    }

    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn reserve(&self, additional: usize) {
        if additional == 0 {
            return;
        }
        unsafe {
            let inner = self.inner_mut();
            inner.ensure_index(inner.len + additional - 1);
        }
    }

    unsafe fn push_internal(&self, value: T) -> *mut T {
        let inner = unsafe { self.inner_mut() };
        let (chunk, offset) = Inner::<T, 0>::coord_of(inner.len);
        unsafe {
            inner.ensure_index(inner.len);
            let data = elem_add(inner.index.add(chunk as usize).read() as *mut T, offset);
            data.write(value);
            inner.len += 1;
            data
        }
    }

    pub fn push(&self, value: T) {
        unsafe { self.push_internal(value) };
    }

    pub fn push_mut(&mut self, value: T) -> &mut T {
        unsafe { &mut *self.push_internal(value) }
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        let inner = unsafe { self.inner() };
        if index >= inner.len {
            return None;
        }
        let (chunk, offset) = Inner::<T, 0>::coord_of(index);
        Some(unsafe {
            let data = inner.index.add(chunk as usize).read() as *mut T;
            &*elem_add(data, offset)
        })
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        let inner = unsafe { self.inner() };
        if index >= inner.len {
            return None;
        }
        let (chunk, offset) = Inner::<T, 0>::coord_of(index);
        Some(unsafe {
            let data = inner.index.add(chunk as usize).read() as *mut T;
            &mut *elem_add(data, offset)
        })
    }

    pub fn pop(&mut self) -> Option<T> {
        let inner = unsafe { self.inner_mut() };
        if inner.len == 0 {
            return None;
        }
        let (chunk, offset) = Inner::<T, 0>::coord_of(inner.len - 1);
        inner.len -= 1;
        Some(unsafe {
            let data = inner.index.add(chunk as usize).read() as *mut T;
            elem_add(data, offset).read()
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len()).map(|i| &self[i])
    }

    pub fn drain<'a>(&'a mut self) -> impl Iterator<Item = T> + 'a {
        let inner = unsafe { self.inner_mut() };
        let len = inner.len;
        inner.len = 0;
        Drain {
            inner,
            len,
            index: 0,
        }
    }
}

impl<T> Default for MonoVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const C: usize> Drop for Inner<T, C> {
    fn drop(&mut self) {
        for chunk in 0..self.chunks {
            let cap = 1usize << (Self::log_base_cap() + chunk);
            let base = Self::chunk_base(chunk);
            let len = self.len.saturating_sub(base).min(cap);

            unsafe {
                let data = self.index.add(chunk as usize).read() as *mut T;
                if needs_drop::<T>() {
                    for i in 0..len {
                        drop_in_place(elem_add(data, i))
                    }
                }
                alloc::dealloc(data as *mut u8, Self::chunk_layout(chunk).0);
            }
        }
    }
}

impl<T> Index<usize> for MonoVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("index out of bounds")
    }
}

impl<T> IndexMut<usize> for MonoVec<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.get_mut(index).expect("index out of bounds")
    }
}

/// A hash map that permits insertion through a shared reference.
///
/// References to keys and values stay valid as the map grows. Entries cannot
/// be removed, and inserting a key that is already present fails.
///
/// `Eq` implementations of keys may look up entries during a lookup, but any
/// access to the map from `Eq` during an insertion panics, as does insertion
/// during a lookup.
pub struct MonoHashMap<K, V, S = RandomState> {
    // Entries in insertion order; crate-internal users rely on this order
    // through the index-based methods.
    entries: MonoVec<Entry<K, V>>,
    // Holds entry indices only, so growth rehashes from stored hashes without
    // running user code.
    index: RefCell<RawTable<usize>>,
    hasher: S,
}

struct Entry<K, V> {
    hash: u64,
    key: K,
    value: V,
}

impl<K, V> MonoHashMap<K, V> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, RandomState::new())
    }
}

impl<K, V, S> MonoHashMap<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        Self::with_capacity_and_hasher(0, hasher)
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self {
            entries: MonoVec::with_capacity(capacity),
            index: RefCell::new(RawTable::with_capacity(capacity)),
            hasher,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|e| (&e.key, &e.value))
    }

    /// Returns the entry at insertion index `index`.
    pub(crate) fn get_index(&self, index: usize) -> Option<(&K, &V)> {
        self.entries.get(index).map(|e| (&e.key, &e.value))
    }

    /// Appends an entry that key lookups never find, returning its index.
    ///
    /// Growth rehashes the table's buckets rather than walking `entries`, so
    /// the entry never enters the index; its hash is never used.
    pub(crate) fn push_unindexed(&self, key: K, value: V) -> usize {
        let i = self.entries.len();
        self.entries.push(Entry {
            hash: 0,
            key,
            value,
        });
        i
    }

    /// Returns whether key lookups can find the entry at `index`.
    pub(crate) fn is_indexed(&self, index: usize) -> bool {
        let hash = self.entries[index].hash;
        self.index.borrow().get(hash, |&j| j == index).is_some()
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> MonoHashMap<K, V, S> {
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_key_value(key).map(|(_, v)| v)
    }

    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let entry = &self.entries[self.get_index_of(key)?];
        Some((&entry.key, &entry.value))
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.get_index_of(key)?;
        Some(&mut self.entries[i].value)
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_index_of(key).is_some()
    }

    /// Inserts an entry, returning its value.
    ///
    /// If the key is present, returns the rejected key and value.
    pub fn try_insert(&self, key: K, value: V) -> Result<&V, (K, V)> {
        match self.try_insert_index(key, value) {
            Ok(i) => Ok(&self.entries[i].value),
            Err((_, key, value)) => Err((key, value)),
        }
    }

    /// Returns the value of `key`, inserting the entry built by `make` if it is
    /// absent.
    ///
    /// The built key must equal `key`. `make` must not access the map.
    pub fn get_or_insert_with<Q>(&self, key: &Q, make: impl FnOnce(&Q) -> (K, V)) -> &V
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        &self.entries[self.get_or_insert_index_with(key, make)].value
    }

    /// Returns the insertion index of `key`.
    pub(crate) fn get_index_of<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hasher.hash_one(key);
        self.index
            .borrow()
            .get(hash, |&i| {
                <K as Borrow<Q>>::borrow(&self.entries[i].key) == key
            })
            .copied()
    }

    /// Like [`try_insert`](Self::try_insert), but returns insertion indices.
    pub(crate) fn try_insert_index(&self, key: K, value: V) -> Result<usize, (usize, K, V)> {
        let hash = self.hasher.hash_one(&key);
        let mut index = self.index.borrow_mut();
        match self.probe(&mut index, hash, &key) {
            Ok(i) => Err((i, key, value)),
            Err(slot) => Ok(unsafe { self.insert_at(&mut index, slot, hash, key, value) }),
        }
    }

    /// Like [`get_or_insert_with`](Self::get_or_insert_with), but returns the
    /// insertion index.
    pub(crate) fn get_or_insert_index_with<Q>(
        &self,
        key: &Q,
        make: impl FnOnce(&Q) -> (K, V),
    ) -> usize
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hasher.hash_one(key);
        let mut index = self.index.borrow_mut();
        match self.probe(&mut index, hash, key) {
            Ok(i) => i,
            Err(slot) => {
                let (owned, value) = make(key);
                debug_assert!(<K as Borrow<Q>>::borrow(&owned) == key, "built key differs");
                debug_assert_eq!(self.hasher.hash_one(&owned), hash, "built key hash differs");
                unsafe { self.insert_at(&mut index, slot, hash, owned, value) }
            }
        }
    }

    /// Finds `key`, returning its index or a slot to insert it at.
    fn probe<Q>(&self, index: &mut RawTable<usize>, hash: u64, key: &Q) -> Result<usize, usize>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let entries = &self.entries;
        match index.find_or_find_insert_index(
            hash,
            |&i| <K as Borrow<Q>>::borrow(&entries[i].key) == key,
            |&i| entries[i].hash,
        ) {
            Ok(bucket) => Ok(unsafe { *bucket.as_ref() }),
            Err(slot) => Err(slot),
        }
    }

    /// # Safety
    ///
    /// `slot` must come from `probe` on `index` with the same `hash`, and
    /// `index` must not have been mutated since.
    unsafe fn insert_at(
        &self,
        index: &mut RawTable<usize>,
        slot: usize,
        hash: u64,
        key: K,
        value: V,
    ) -> usize {
        let i = self.entries.len();
        self.entries.push(Entry { hash, key, value });
        unsafe { index.insert_at_index(hash, slot, i) };
        i
    }
}

impl<K, V, S: Default> Default for MonoHashMap<K, V, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

/// A hash set that permits insertion through a shared reference.
///
/// A [`MonoHashMap`] with `()` values, with the same guarantees and
/// reentrancy rules.
pub struct MonoHashSet<T, S = RandomState> {
    map: MonoHashMap<T, (), S>,
}

impl<T> MonoHashSet<T> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, RandomState::new())
    }
}

impl<T, S> MonoHashSet<T, S> {
    pub fn with_hasher(hasher: S) -> Self {
        Self::with_capacity_and_hasher(0, hasher)
    }

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self {
            map: MonoHashMap::with_capacity_and_hasher(capacity, hasher),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.map.iter().map(|(k, _)| k)
    }
}

impl<T: Hash + Eq, S: BuildHasher> MonoHashSet<T, S> {
    pub fn get<Q>(&self, value: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_key_value(value).map(|(k, _)| k)
    }

    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(value)
    }

    /// Inserts `value`, returning a reference to it.
    ///
    /// If an equal value is present, returns the rejected value.
    pub fn try_insert(&self, value: T) -> Result<&T, T> {
        match self.map.try_insert_index(value, ()) {
            Ok(i) => Ok(&self.map.entries[i].key),
            Err((_, value, ())) => Err(value),
        }
    }

    /// Returns the value equal to `value`, inserting the one built by `make` if
    /// it is absent.
    ///
    /// The built value must equal `value`. `make` must not access the set.
    pub fn get_or_insert_with<Q>(&self, value: &Q, make: impl FnOnce(&Q) -> T) -> &T
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let i = self.map.get_or_insert_index_with(value, |q| (make(q), ()));
        &self.map.entries[i].key
    }
}

impl<T, S: Default> Default for MonoHashSet<T, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    const SIZE: usize = 1 << 10;

    #[test]
    fn push_a_lotta_things() {
        let vec = MonoVec::with_capacity(SIZE >> 2);
        assert!(vec.capacity() >= SIZE >> 2);

        for i in 0..SIZE {
            vec.push(i);
        }

        for i in 0..SIZE {
            assert_eq!(vec[i], i);
        }
    }

    #[test]
    fn zst() {
        let vec = MonoVec::with_capacity(SIZE >> 2);
        assert!(vec.capacity() >= SIZE >> 2);

        for _ in 0..SIZE {
            vec.push(());
        }

        for i in 0..SIZE - 1 {
            assert_ne!(&vec[i] as *const (), &vec[i + 1] as *const ());
        }
    }

    #[test]
    fn map_insert_and_find() {
        let map = MonoHashMap::new();
        let first = map.try_insert(String::from("0"), 0).unwrap();
        for i in 1..SIZE {
            assert_eq!(map.try_insert(i.to_string(), i), Ok(&i));
        }
        assert_eq!(*first, 0);
        for i in 0..SIZE {
            let key = i.to_string();
            assert_eq!(map.get_key_value(key.as_str()), Some((&key, &i)));
            assert_eq!(map.get_index_of(key.as_str()), Some(i));
        }
        assert!(!map.contains_key("missing"));
        assert_eq!(map.len(), SIZE);
        assert!(map.iter().map(|(_, v)| *v).eq(0..SIZE));
    }

    #[test]
    fn map_duplicates() {
        let map = MonoHashMap::new();
        assert_eq!(map.try_insert("a", 1), Ok(&1));
        assert_eq!(map.try_insert("a", 2), Err(("a", 2)));
        assert_eq!(map.try_insert_index("a", 2), Err((0, "a", 2)));
        assert_eq!(map.get_or_insert_with(&"a", |k| (*k, 3)), &1);
        assert_eq!(map.get_or_insert_with(&"b", |k| (*k, 4)), &4);
        assert_eq!(map.get_or_insert_index_with(&"b", |k| (*k, 5)), 1);
        assert_eq!(map.get("a"), Some(&1));
        assert_eq!(map.get("b"), Some(&4));
    }

    #[test]
    fn map_unindexed() {
        let map = MonoHashMap::new();
        assert_eq!(map.try_insert_index(0, ()), Ok(0));
        assert_eq!(map.push_unindexed(0, ()), 1);
        for i in 1..SIZE {
            map.try_insert_index(i, ()).unwrap();
            map.push_unindexed(i, ());
        }
        for i in 0..SIZE {
            assert_eq!(map.get_index_of(&i), Some(2 * i));
            assert!(map.is_indexed(2 * i));
            assert!(!map.is_indexed(2 * i + 1));
            assert_eq!(map.get_index(2 * i + 1), Some((&i, &())));
        }
    }

    #[test]
    fn set_insert_and_find() {
        let set = MonoHashSet::new();
        let first = set.try_insert(String::from("0")).unwrap();
        for i in 1..SIZE {
            assert_eq!(set.try_insert(i.to_string()), Ok(&i.to_string()));
        }
        assert_eq!(first, "0");
        assert_eq!(set.try_insert(String::from("1")), Err(String::from("1")));
        assert_eq!(set.get_or_insert_with("2", str::to_owned), "2");
        assert_eq!(set.get_or_insert_with("x", str::to_owned), "x");
        assert_eq!(set.get("x").map(String::as_str), Some("x"));
        assert!(set.contains("5"));
        assert!(!set.contains("missing"));
        assert_eq!(set.len(), SIZE + 1);
        assert!(
            set.iter()
                .take(SIZE)
                .eq((0..SIZE).map(|i| i.to_string()).collect::<Vec<_>>().iter())
        );
    }

    #[derive(Debug)]
    struct Reentrant(u32);

    thread_local! {
        static REENTRANT_MAP: std::cell::Cell<*const MonoHashMap<Reentrant, ()>> =
            const { std::cell::Cell::new(std::ptr::null()) };
    }

    impl Hash for Reentrant {
        fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
    }

    impl PartialEq for Reentrant {
        fn eq(&self, other: &Self) -> bool {
            let map = REENTRANT_MAP.get();
            if !map.is_null() {
                let _ = unsafe { &*map }.try_insert(Reentrant(u32::MAX), ());
            }
            self.0 == other.0
        }
    }

    impl Eq for Reentrant {}

    #[test]
    #[should_panic(expected = "already borrowed")]
    fn map_reentrant_insert() {
        let map = MonoHashMap::new();
        map.try_insert(Reentrant(0), ()).unwrap();
        REENTRANT_MAP.set(&map);
        let _ = map.try_insert(Reentrant(1), ());
    }
}
