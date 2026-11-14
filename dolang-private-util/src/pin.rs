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

    /// The first byte of the segment's data.
    #[inline]
    unsafe fn start(this: NonNull<Self>) -> NonNull<u8> {
        unsafe { NonNull::new_unchecked(&raw mut (*this.as_ptr()).data).cast() }
    }

    #[inline]
    unsafe fn is_empty(this: NonNull<Self>) -> bool {
        unsafe { (*this.as_ptr()).cur.get() == Self::start(this) }
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

/// Report a pinned future that was leaked rather than dropped.  The caller has already kept
/// its memory, so the arena stays sound if the panic is caught.
///
/// A leak skips the future's own cleanup too, so continuing would hide the bug.  Panicking
/// while already unwinding would abort, so stay quiet then.
#[cold]
#[inline(never)]
fn leaked() {
    if !std::thread::panicking() {
        panic!("a pinned future was leaked instead of dropped (e.g. by `mem::forget`)");
    }
}

pub struct Arena {
    seg: Cell<NonNull<Seg>>,
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe {
            // A nonempty segment holds a pinned future that was leaked rather than dropped.
            // Its memory must stay valid, so leak every segment with it.
            let mut seg = Some(self.seg.get());
            while let Some(cur) = seg {
                if !Seg::is_empty(cur) {
                    return leaked();
                }
                seg = (*cur.as_ptr()).prev;
            }
            let mut seg = Some(self.seg.get());
            while let Some(cur) = seg {
                seg = (*cur.as_ptr()).prev;
                Seg::dealloc(cur);
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
            // Must drop contents *first*, which drops any nested Pinned guard;
            // segment/offset updates will then occur in LIFO order as required
            ptr::drop_in_place(entry.as_ptr());
            // Free the entry only if it is on top: last in its own segment (`last_seg`),
            // with every newer segment empty.  An entry above it belongs to a pinned future
            // that was leaked rather than dropped; its memory must stay valid, so leave
            // everything in place.
            let end = entry.cast::<u8>().add(size_of::<Entry<T>>());
            if (*last_seg.as_ptr()).cur.get() != end {
                return leaked();
            }
            let mut seg = self.seg.get();
            while seg != last_seg {
                if !Seg::is_empty(seg) {
                    return leaked();
                }
                seg = (*seg.as_ptr()).prev.unwrap_unchecked();
            }
            while self.seg.get() != last_seg {
                self.seg.update(|seg| {
                    let prev = (*seg.as_ptr()).prev.unwrap_unchecked();
                    Seg::dealloc(seg);
                    prev
                })
            }
            (*self.seg.get().as_ptr()).cur.set(last_cur);
        }
    }

    pub fn new(initial_size: usize) -> Self {
        Self {
            seg: Cell::new(Seg::alloc(initial_size, None)),
        }
    }

    /// # Safety
    /// The caller must ensure that pins are strictly nested, except that a pin may be
    /// leaked: its memory is then never reused or freed.
    ///
    /// # Panics
    ///
    /// Dropping the returned pin, or the arena, panics if a pin above it was leaked.
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

#[cfg(test)]
mod tests {
    use std::{
        future::{self, Future, poll_fn},
        mem,
        panic::{self, AssertUnwindSafe},
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use super::Arena;

    fn poll_once(fut: impl Future) {
        let mut cx = Context::from_waker(Waker::noop());
        let _ = pin!(fut).poll(&mut cx);
    }

    #[test]
    fn leaked_pin_is_never_reused() {
        let arena = Arena::new(64);
        let leaked = std::cell::Cell::new(std::ptr::null::<u64>());
        // Safety: pins nest strictly, except for the leaked one
        let outer = unsafe {
            arena.pin_future_unchecked(async {
                let inner = arena.pin_future_unchecked(async {
                    let marker = 7u64;
                    leaked.set(&raw const marker);
                    poll_fn(|_| Poll::<()>::Pending).await;
                    assert_eq!(marker, 7);
                });
                let mut inner = Box::pin(inner);
                poll_once(inner.as_mut());
                mem::forget(inner);
            })
        };
        // Popping the outer pin finds the leaked one above it
        assert!(panic::catch_unwind(AssertUnwindSafe(|| poll_once(outer))).is_err());
        assert!(!leaked.get().is_null());
        // A later pin must not overwrite the leaked future's state
        let reuse = unsafe { arena.pin_future_unchecked(future::ready([0xffu64; 8])) };
        poll_once(reuse);
        assert_eq!(unsafe { *leaked.get() }, 7);
        // Dropping the arena finds it too, and leaks its memory
        assert!(panic::catch_unwind(AssertUnwindSafe(|| drop(arena))).is_err());
    }

    #[test]
    fn nested_pins_reuse_memory() {
        let arena = Arena::new(64);
        let first = unsafe { arena.pin_future_unchecked(future::ready([0u64; 4])) };
        let addr = first.entry;
        drop(first);
        let second = unsafe { arena.pin_future_unchecked(future::ready([0u64; 4])) };
        assert_eq!(second.entry, addr);
        // Grow past the initial segment, then pop back to it
        let big = unsafe { arena.pin_future_unchecked(future::ready([0u64; 64])) };
        drop(big);
        drop(second);
        let seg = arena.seg.get();
        assert!(unsafe { (*seg.as_ptr()).prev }.is_none());
        assert!(unsafe { super::Seg::is_empty(seg) });
    }
}
