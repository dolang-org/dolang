//! Read-only views of collections that grow through shared references.

/// Collections whose frozen view may be shared between threads.
///
/// # Safety
///
/// Every method of `Frozen<Self>` must only read: it must not grow the
/// collection or write any `Cell` or `RefCell`, including a borrow flag. Every
/// value it reaches, and every user code it runs (such as `Hash` and `Eq`
/// implementations), must be safe to share between threads.
pub unsafe trait Freeze {}

/// A read-only view of a collection.
///
/// Obtained by value with [`new`](Self::new) or from a mutable borrow with
/// [`from_mut`](Self::from_mut). Either way, nothing else can grow the
/// collection while the view exists, so the view can be shared between threads.
#[repr(transparent)]
pub struct Frozen<T>(T);

unsafe impl<T: Freeze> Sync for Frozen<T> {}

impl<T> Frozen<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Thaws the collection.
    pub fn into_inner(self) -> T {
        self.0
    }

    pub fn from_mut(value: &mut T) -> &Self {
        // Safety: `Frozen` is a transparent wrapper, and the exclusive borrow
        // keeps the collection from being accessed except through the view.
        unsafe { &*(value as *mut T as *const Self) }
    }

    /// The frozen collection, for implementing views.
    ///
    /// # Safety
    ///
    /// The caller must only use the collection as [`Freeze`] requires of the
    /// view's methods: it must only read, without writing any `Cell` or
    /// `RefCell`.
    pub(crate) unsafe fn inner(&self) -> &T {
        &self.0
    }

    /// A frozen view of part of the collection.
    ///
    /// # Safety
    ///
    /// `part` must return a value the collection owns, so that it is frozen
    /// along with the collection.
    pub(crate) unsafe fn part<U>(&self, part: impl FnOnce(&T) -> &U) -> &Frozen<U> {
        // Safety: `Frozen` is a transparent wrapper, and the caller ensures
        // the value is frozen.
        unsafe { &*(part(&self.0) as *const U as *const Frozen<U>) }
    }
}

#[cfg(test)]
mod test {
    use super::Frozen;
    use crate::{
        intern::{BinTable, Table},
        mono::{MonoHashMap, MonoHashSet, MonoVec},
    };

    const _: () = {
        const fn send<T: Send>() {}
        const fn shared<T: Send + Sync>() {}
        send::<MonoVec<u32>>();
        send::<MonoHashMap<String, u32>>();
        send::<MonoHashSet<String>>();
        send::<Table<String, ()>>();
        send::<BinTable>();
        shared::<Frozen<MonoVec<u32>>>();
        shared::<Frozen<MonoHashMap<String, u32>>>();
        shared::<Frozen<MonoHashSet<String>>>();
        shared::<Frozen<Table<String, ()>>>();
        shared::<Frozen<BinTable>>();
    };
}
