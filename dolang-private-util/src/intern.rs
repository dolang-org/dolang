use std::{
    borrow::Borrow, collections::HashMap, fmt::Debug, hash::Hash, marker::PhantomData, ops::Index,
};

use crate::arena::ArenaVec;

pub struct Id<Tag>(usize, PhantomData<*const Tag>);

unsafe impl<Tag> Sync for Id<Tag> {}
unsafe impl<Tag> Send for Id<Tag> {}

impl<Tag> Debug for Id<Tag> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == usize::MAX {
            write!(f, "Id(<INVALID>)")
        } else {
            write!(f, "Id({})", self.0)
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
    pub fn new(index: usize) -> Self {
        Self(index, PhantomData)
    }

    pub fn index(&self) -> usize {
        self.0
    }
}

pub struct Table<T, Tag> {
    values: ArenaVec<(T, bool)>,
    index: HashMap<T, Id<Tag>>,
    phantom: PhantomData<Tag>,
}

impl<T, Tag> Table<T, Tag> {
    pub fn new() -> Self {
        Table {
            values: ArenaVec::new(),
            index: HashMap::new(),
            phantom: PhantomData,
        }
    }

    pub fn id<Q>(&mut self, k: &Q) -> Id<Tag>
    where
        T: Hash + Eq + Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = T> + ?Sized,
    {
        if let Some(id) = self.index.get(k) {
            *id
        } else {
            let id = Id::new(self.values.len());
            self.values.push((k.to_owned(), false));
            self.index.insert(k.to_owned(), id);
            id
        }
    }

    /// Allocate a fresh `Id` without inserting into the reverse index.
    ///
    /// Two calls with the same `k` produce different `Id` values; the entry
    /// cannot be looked up by key.  Used for private symbols whose uniqueness
    /// must be preserved across separately-compiled modules.
    pub fn fresh(&mut self, k: T) -> Id<Tag>
    where
        T: Hash + Eq,
    {
        let id = Id::new(self.values.len());
        self.values.push((k, true));
        id
    }

    pub fn is_fresh(&self, id: Id<Tag>) -> bool {
        self.values[id.0].1
    }

    pub fn get_by_index(&self, index: usize) -> Option<&T> {
        self.values.get(index).map(|(t, _)| t)
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
        if index == self.table.values.len() {
            None
        } else {
            self.index += 1;
            Some((Id(index, PhantomData), &self.table.values[index].0))
        }
    }
}

impl<T, Tag> Index<Id<Tag>> for Table<T, Tag> {
    type Output = T;

    fn index(&self, index: Id<Tag>) -> &Self::Output {
        &self.values[index.0].0
    }
}

impl<T, Tag> Default for Table<T, Tag> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct StrId(usize, usize);

impl StrId {
    pub fn start(&self) -> usize {
        self.0
    }
    pub fn end(&self) -> usize {
        self.1
    }

    pub fn as_bin_id(self) -> BinId {
        BinId(self.0, self.1)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BinId(usize, usize);

impl BinId {
    pub fn start(&self) -> usize {
        self.0
    }
    pub fn end(&self) -> usize {
        self.1
    }
}

pub struct BinTable {
    arena: Vec<u8>,
    index: HashMap<Vec<u8>, BinId>,
}

impl BinTable {
    pub fn new() -> Self {
        BinTable {
            arena: Default::default(),
            index: Default::default(),
        }
    }

    pub fn id(&mut self, bytes: &[u8]) -> BinId {
        if bytes.is_empty() {
            BinId(0, 0)
        } else if let Some(&id) = self.index.get(bytes) {
            id
        } else {
            let start = self.arena.len();
            self.arena.extend_from_slice(bytes);
            let end = self.arena.len();
            let id = BinId(start, end);
            self.index.insert(bytes.to_vec(), id);
            id
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.arena
    }

    /// Intern a UTF-8 string and return a `StrId` witnessing its validity.
    pub fn id_str(&mut self, s: &str) -> StrId {
        let BinId(start, end) = self.id(s.as_bytes());
        StrId(start, end)
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
        &self.arena[index.0..index.1]
    }
}

impl Index<StrId> for BinTable {
    type Output = str;

    fn index(&self, index: StrId) -> &Self::Output {
        // Safety: StrId is only constructable via `id_str`, which guarantees
        // the byte range contains valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.arena[index.0..index.1]) }
    }
}
