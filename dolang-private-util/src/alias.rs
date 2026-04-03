use std::{
    alloc::{self, Layout},
    fmt, mem,
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
};

pub struct Box<T: ?Sized>(NonNull<T>);

impl Box<str> {
    pub fn new_str(value: impl AsRef<str>) -> Self {
        String::from(value.as_ref()).into()
    }
}

impl Default for Box<str> {
    fn default() -> Self {
        String::new().into()
    }
}

impl Clone for Box<str> {
    fn clone(&self) -> Self {
        self.as_ref().into()
    }
}

impl From<&str> for Box<str> {
    fn from(value: &str) -> Self {
        Self::new_str(value)
    }
}

impl<T> Box<T> {
    pub fn new(value: T) -> Self {
        let layout = Layout::for_value(&value);
        unsafe {
            let raw = if layout.size() != 0 {
                let raw = std::alloc::alloc(layout) as *mut T;
                ptr::write(raw, value);
                raw
            } else {
                ptr::dangling_mut()
            };
            Self(NonNull::new_unchecked(raw))
        }
    }
}

impl<T> From<Vec<T>> for Box<[T]> {
    fn from(mut value: Vec<T>) -> Self {
        let len = value.len();
        let cap = value.capacity();
        let ptr = value.as_mut_ptr();
        mem::forget(value);

        unsafe {
            let ptr = if mem::size_of::<T>() == 0 {
                ptr::dangling_mut()
            } else if len == 0 {
                if cap != 0 {
                    alloc::dealloc(
                        ptr.cast(),
                        Layout::array::<T>(cap).expect("vec capacity layout overflow"),
                    );
                }
                ptr::dangling_mut()
            } else if cap == len {
                ptr
            } else {
                let old_layout = Layout::array::<T>(cap).expect("vec capacity layout overflow");
                let new_layout = Layout::array::<T>(len).expect("slice layout overflow");
                let ptr = alloc::realloc(ptr.cast(), old_layout, new_layout.size()).cast::<T>();
                if ptr.is_null() {
                    alloc::handle_alloc_error(new_layout);
                }
                ptr
            };

            Self::from_non_null(NonNull::slice_from_raw_parts(
                NonNull::new_unchecked(ptr),
                len,
            ))
        }
    }
}

impl<T> Default for Box<[T]> {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl<T: Clone> Clone for Box<[T]> {
    fn clone(&self) -> Self {
        self.iter().cloned().collect()
    }
}

impl<T> FromIterator<T> for Box<[T]> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl From<String> for Box<str> {
    fn from(mut value: String) -> Self {
        let len = value.len();
        let cap = value.capacity();
        let ptr = value.as_mut_ptr();
        mem::forget(value);

        unsafe {
            let ptr = if len == 0 {
                if cap != 0 {
                    alloc::dealloc(
                        ptr.cast(),
                        Layout::array::<u8>(cap).expect("string capacity layout overflow"),
                    );
                }
                ptr::dangling_mut()
            } else if cap == len {
                ptr
            } else {
                let old_layout = Layout::array::<u8>(cap).expect("string capacity layout overflow");
                let new_layout = Layout::array::<u8>(len).expect("str layout overflow");
                let ptr = alloc::realloc(ptr.cast(), old_layout, new_layout.size());
                if ptr.is_null() {
                    alloc::handle_alloc_error(new_layout);
                }
                ptr
            };

            Self::from_non_null(mem::transmute::<NonNull<[u8]>, NonNull<str>>(
                NonNull::slice_from_raw_parts(NonNull::new_unchecked(ptr), len),
            ))
        }
    }
}

impl<T: ?Sized> Box<T> {
    /// # Safety
    /// It must be safe for `ptr` to have its contents dropped and be deallocated
    /// with std::alloc::dealloc on Drop
    pub unsafe fn from_non_null(ptr: NonNull<T>) -> Self {
        Self(ptr)
    }

    pub fn into_non_null(this: Self) -> NonNull<T> {
        let ptr = this.0;
        mem::forget(this);
        ptr
    }
}

impl<T: ?Sized> Drop for Box<T> {
    fn drop(&mut self) {
        unsafe {
            if std::mem::needs_drop::<T>() {
                ptr::drop_in_place(self.0.as_ptr());
            }
            let layout = Layout::for_value(self.0.as_ref());
            if layout.size() != 0 {
                std::alloc::dealloc(self.0.cast().as_ptr(), layout);
            }
        }
    }
}

impl<T: ?Sized> Deref for Box<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.0.as_ref() }
    }
}

impl<T: ?Sized> AsRef<T> for Box<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized> DerefMut for Box<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.0.as_mut() }
    }
}

impl<T: ?Sized> AsMut<T> for Box<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for Box<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        T::fmt(self, f)
    }
}

impl<T: fmt::Display + ?Sized> fmt::Display for Box<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        T::fmt(self, f)
    }
}

#[cfg(test)]
mod test {
    use super::Box;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn from_vec_exact_fit() {
        let boxed = Box::<[u32]>::from(vec![1, 2, 3]);
        assert_eq!(&*boxed, &[1, 2, 3]);
    }

    #[test]
    fn from_vec_shrinks() {
        let mut vec = Vec::with_capacity(16);
        vec.extend([1, 2, 3, 4]);

        let boxed = Box::<[u32]>::from(vec);
        assert_eq!(&*boxed, &[1, 2, 3, 4]);
    }

    #[test]
    fn from_empty_vec() {
        let boxed = Box::<[u32]>::from(Vec::new());
        assert!(boxed.is_empty());
    }

    #[test]
    fn from_empty_allocated_vec() {
        let vec = Vec::<u32>::with_capacity(16);
        let boxed = Box::<[u32]>::from(vec);
        assert!(boxed.is_empty());
    }

    #[test]
    fn from_string_exact_fit() {
        let boxed = Box::<str>::from(String::from("hello"));
        assert_eq!(&*boxed, "hello");
    }

    #[test]
    fn from_string_shrinks() {
        let mut string = String::with_capacity(16);
        string.push_str("hello");

        let boxed = Box::<str>::from(string);
        assert_eq!(&*boxed, "hello");
    }

    #[test]
    fn from_empty_string() {
        let boxed = Box::<str>::from(String::new());
        assert!(boxed.is_empty());
    }

    #[test]
    fn from_empty_allocated_string() {
        let string = String::with_capacity(16);
        let boxed = Box::<str>::from(string);
        assert!(boxed.is_empty());
    }

    #[test]
    fn from_zst_vec() {
        let boxed = Box::<[()]>::from(vec![(), (), ()]);
        assert_eq!(boxed.len(), 3);
    }

    #[test]
    fn from_vec_drops_elements_once() {
        struct DropCounter(Arc<AtomicUsize>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let boxed = Box::<[DropCounter]>::from(vec![
            DropCounter(drops.clone()),
            DropCounter(drops.clone()),
            DropCounter(drops.clone()),
        ]);

        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(boxed);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }
}
