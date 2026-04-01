use std::{
    alloc::{self, Layout, LayoutError},
    cell::UnsafeCell,
    marker::PhantomData,
    mem::needs_drop,
    ops::{Index, IndexMut},
    ptr::{NonNull, copy_nonoverlapping, drop_in_place},
};

pub struct ArenaVec<T> {
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

impl<T> ArenaVec<T> {
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

impl<T> Default for ArenaVec<T> {
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

impl<T> Index<usize> for ArenaVec<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("index out of bounds")
    }
}

impl<T> IndexMut<usize> for ArenaVec<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.get_mut(index).expect("index out of bounds")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    const SIZE: usize = 1 << 10;

    #[test]
    fn push_a_lotta_things() {
        let vec = ArenaVec::with_capacity(SIZE >> 2);
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
        let vec = ArenaVec::with_capacity(SIZE >> 2);
        assert!(vec.capacity() >= SIZE >> 2);

        for _ in 0..SIZE {
            vec.push(());
        }

        for i in 0..SIZE - 1 {
            assert_ne!(&vec[i] as *const (), &vec[i + 1] as *const ());
        }
    }
}
