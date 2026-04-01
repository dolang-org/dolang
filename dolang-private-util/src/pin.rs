use std::{
    alloc::{Layout, alloc, dealloc},
    cell::Cell,
    pin::Pin,
    ptr::{self, NonNull},
    task::{self, Poll},
};

#[repr(C)]
struct Seg {
    prev: Option<NonNull<Seg>>,
    cur: Cell<NonNull<u8>>,
    size: usize,
    data: [u8; 0],
}

impl Seg {
    #[inline]
    fn alloc(size: usize, prev: Option<NonNull<Seg>>) -> NonNull<Self> {
        let layout = Layout::new::<Seg>()
            .extend(Layout::array::<u8>(size).unwrap())
            .unwrap()
            .0;
        unsafe {
            let mem = NonNull::new(alloc(layout))
                .expect("failed to allocate memory")
                .cast::<Self>();
            ptr::write(&raw mut (*mem.as_ptr()).prev, prev);
            ptr::write(
                &raw mut (*mem.as_ptr()).cur,
                Cell::new(NonNull::new_unchecked(&raw mut (*mem.as_ptr()).data).cast()),
            );
            ptr::write(&raw mut (*mem.as_ptr()).size, size);
            mem
        }
    }

    #[inline]
    fn dealloc(this: NonNull<Self>) {
        unsafe {
            let layout = Layout::new::<Seg>()
                .extend(Layout::array::<u8>(this.as_ref().size).unwrap())
                .unwrap()
                .0;
            dealloc(this.cast().as_ptr(), layout);
        }
    }

    #[inline]
    fn push<T>(&self, value: T) -> Result<NonNull<Entry<T>>, (T, usize)> {
        let layout = Layout::new::<Entry<T>>();
        unsafe {
            let cur = self.cur.get();
            let space = self.size - cur.offset_from_unsigned(NonNull::from_ref(&self.data).cast());
            let align = cur.align_offset(layout.align());
            let size = layout.size();
            let total = align
                .checked_add(size)
                .expect("impossibly large allocation");
            if total > space {
                return Err((
                    value,
                    layout
                        .align()
                        .checked_add(size)
                        .expect("impossibly large allocation"),
                ));
            }
            let start = cur.add(align);
            ptr::write(
                start.cast().as_ptr(),
                Entry {
                    last_seg: NonNull::from_ref(self),
                    last_cur: cur,
                    value,
                },
            );
            self.cur.set(start.add(size));
            Ok(start.cast())
        }
    }
}

#[repr(C)]
struct Entry<T> {
    last_seg: NonNull<Seg>,
    last_cur: NonNull<u8>,
    value: T,
}

pub struct Pinned<'a, R> {
    arena: &'a Arena,
    entry: NonNull<()>,
    poll: unsafe fn(NonNull<()>, &mut task::Context<'_>) -> Poll<R>,
    drop: unsafe fn(&Arena, NonNull<()>),
}

impl<'a, R> Future for Pinned<'a, R> {
    type Output = R;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        unsafe { (self.poll)(self.entry, cx) }
    }
}

impl<'a, R> Drop for Pinned<'a, R> {
    #[inline]
    fn drop(&mut self) {
        unsafe { (self.drop)(self.arena, self.entry) }
    }
}

pub struct Arena {
    seg: Cell<NonNull<Seg>>,
}

impl Drop for Arena {
    fn drop(&mut self) {
        let mut seg = Some(self.seg.get());
        while let Some(cur) = seg {
            unsafe {
                let prev = (*cur.as_ptr()).prev;
                Seg::dealloc(cur);
                seg = prev
            }
        }
    }
}

impl Arena {
    #[inline]
    fn push<T>(&self, value: T) -> NonNull<Entry<T>> {
        unsafe {
            let seg = self.seg.get();
            let (value, needed) = match (*seg.as_ptr()).push(value) {
                Ok(entry) => return entry,
                Err(err) => err,
            };
            let mut size = (*seg.as_ptr())
                .size
                .checked_mul(2)
                .expect("impossibly large allocation size");
            while size < needed {
                size = size
                    .checked_mul(2)
                    .expect("impossibly large allocation size");
            }
            let seg = Seg::alloc(size, Some(seg));
            self.seg.set(seg);
            (*seg.as_ptr()).push(value).unwrap_unchecked()
        }
    }

    #[inline]
    unsafe fn pop<T>(&self, entry: NonNull<Entry<T>>) {
        unsafe {
            let last_seg = entry.as_ref().last_seg;
            let last_cur = entry.as_ref().last_cur;
            while self.seg.get() != last_seg {
                self.seg.update(|seg| {
                    let prev = (*seg.as_ptr()).prev.unwrap_unchecked();
                    Seg::dealloc(seg);
                    prev
                })
            }
            (*self.seg.get().as_ptr()).cur.set(last_cur);
            ptr::drop_in_place(entry.as_ptr());
        }
    }

    pub fn new(initial_size: usize) -> Self {
        Self {
            seg: Cell::new(Seg::alloc(initial_size, None)),
        }
    }

    /// # Safety
    /// The caller must ensure that pins are strictly nested
    #[inline]
    pub unsafe fn pin_future_unchecked<'a, R, F: Future<Output = R> + 'a>(
        &'a self,
        future: F,
    ) -> Pinned<'a, R> {
        let entry = self.push(future);
        Pinned {
            arena: self,
            entry: entry.cast(),
            poll: |entry, cx| unsafe {
                Pin::new_unchecked(&mut (*entry.cast::<Entry<F>>().as_ptr()).value).poll(cx)
            },
            drop: |arena, entry| unsafe { arena.pop(entry.cast::<Entry<F>>()) },
        }
    }
}
