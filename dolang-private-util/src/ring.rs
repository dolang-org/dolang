use std::{
    cell::Cell,
    ptr::{self, NonNull},
};

#[repr(C)]
pub struct Link<T> {
    prev: Cell<*const Link<T>>,
    next: Cell<*const Link<T>>,
}

impl<T> Link<T> {
    pub fn new() -> Self {
        Self {
            prev: Default::default(),
            next: Default::default(),
        }
    }

    /// # Safety
    /// The link must have been initialized after being moved to a stable address,
    /// and its neighbors (if any) must be valid and initialized.
    pub unsafe fn remove(&self) {
        unsafe {
            (*self.prev.get()).next.set(self.next.get());
            (*self.next.get()).prev.set(self.prev.get());
        }
        self.init()
    }

    pub fn init(&self) {
        self.next.set(self as *const _);
        self.prev.set(self as *const _);
    }

    pub fn in_ring(&self) -> bool {
        !ptr::eq(self.next.get(), self as *const _)
    }
}

#[repr(C)]
pub struct Ring<T, const O: usize>(Link<T>);

pub struct Iter<T, const O: usize> {
    head: *const Link<T>,
    cur: *const Cell<*const Link<T>>,
}

impl<T, const O: usize> Iterator for Iter<T, O> {
    type Item = NonNull<T>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let cur = (*self.cur).get();
            if ptr::eq(cur, self.head) {
                return None;
            }
            self.cur = &raw const (*cur).next;
            Some(NonNull::new_unchecked((cur as *const u8).sub(O) as *mut T))
        }
    }
}

impl<T> Default for Link<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const O: usize> Ring<T, O> {
    pub fn new() -> Self {
        Self(Link::new())
    }

    pub fn init(&self) {
        self.0.init()
    }

    /// # Safety
    /// `self` must be initialized and at a stable address; `item` must be valid
    /// and at a stable address.  `O` must be the correct offset of a link field
    /// in `T`.  `item` must not be present in any other ring through the same link.
    pub unsafe fn push_front(&self, item: NonNull<T>) {
        unsafe {
            let link = (item.as_ptr() as *mut u8).add(O) as *mut Link<T>;
            assert!(!(*link).in_ring());
            (*link).next.set(self.0.next.get());
            (*link).prev.set(&raw const self.0);
            (*self.0.next.get()).prev.set(link);
            self.0.next.set(link);
        }
    }

    /// # Safety
    /// `self` must be initialized and at a stable address; `O` must be the correct offset of a link field
    /// in `T`.  Ring contents must be valid.
    pub unsafe fn pop_front(&self) -> Option<NonNull<T>> {
        unsafe {
            if ptr::eq(self.0.next.get(), &self.0 as *const _) {
                return None;
            }
            let link = self.0.next.get();
            (*link).remove();
            Some(NonNull::new_unchecked((link as *mut u8).sub(O) as *mut T))
        }
    }

    /// # Safety
    /// The usual
    pub unsafe fn migrate(&self, dest: &Self) {
        if ptr::eq(self.0.next.get(), &self.0 as *const _) {
            return;
        }
        unsafe {
            (*dest.0.prev.get()).next.set(self.0.next.get());
            (*self.0.next.get()).prev.set(dest.0.prev.get());
            (*self.0.prev.get()).next.set(&dest.0 as *const _);
            dest.0.prev.set(self.0.prev.get());
            self.init()
        }
    }

    /// # Safety
    /// `self` must be initialized and at a stable address.  All items in ring must
    /// be valid and at stable addresses.
    pub unsafe fn iter(&self) -> Iter<T, O> {
        Iter {
            head: &self.0 as *const _,
            cur: &raw const self.0.next,
        }
    }
}

impl<T, const O: usize> Default for Ring<T, O> {
    fn default() -> Self {
        Self::new()
    }
}

#[macro_export]
macro_rules! ring {
    ($ty: ty, $field: ident) => {$crate::ring::Ring<$ty, {std::mem::offset_of!($ty, $field)}>};
}
