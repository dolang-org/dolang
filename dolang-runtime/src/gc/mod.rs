use std::{
    alloc::{self, Layout},
    any::type_name,
    cell::UnsafeCell,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ops::{ControlFlow, Deref, DerefMut},
    ptr::{self, NonNull},
};

use crate::value::repr::tag;

pub(crate) mod arena;

use arena::{Arena, Header, Upcast, Visit, Vtbl};

/// Trait for types used as the `Annex` of a [`Collect`] type.
///
/// Unlike `Collect`, this is not `unsafe` — implementors simply provide
/// GC traversal and clearing with interior-mutability semantics (`&self`).
pub(crate) trait Annex {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()>;
    /// Clear GC-rooted values held by this annex.
    /// Takes `&self` to allow interior mutability (e.g. `UnsafeCell`).
    fn clear(&self);
}

impl Annex for () {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self) {}
}

/// # Safety
/// This defines the metadata for garbage-collected types.
/// Implementors must correctly implement `accept` and `clear` to visit/zero
/// all strongly-referenced GC objects, or Strange Behavior will result.
pub(crate) unsafe trait Collect {
    const CYCLIC: bool;
    const IMMUTABLE: bool;
    const STRAND: bool = false;
    type Annex: Annex;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()>;
    fn clear(&mut self);
    #[allow(unused_variables)]
    fn finalize(this: NonNull<Header>) {}
}

/// Base garbage-collected strong pointer with least upper bound `T`
///
/// Intended to be wrapped by something exposing fewer details.
pub(crate) struct Base<'v, T: Upcast<Header>> {
    ptr: NonNull<T>,
    phantom: PhantomData<(&'v mut &'v (), T)>,
}

impl<'v, T: Upcast<Header>> Base<'v, T> {
    fn base(&self) -> NonNull<Header> {
        self.ptr.cast()
    }

    fn base_ref(&self) -> &Header {
        unsafe { self.base().as_ref() }
    }

    pub(crate) fn vtbl(&self) -> NonNull<Vtbl> {
        self.base_ref().vtbl
    }

    pub(crate) unsafe fn new_from_raw(arena: &Arena<'v>, ptr: NonNull<T>, size: usize) -> Self {
        unsafe { Self::new_from_raw_inner(NonNull::from_ref(&arena.0), ptr, size) }
    }

    unsafe fn new_from_raw_inner(
        arena: NonNull<arena::ArenaInner>,
        ptr: NonNull<T>,
        size: usize,
    ) -> Self {
        let this = Self {
            ptr,
            phantom: PhantomData,
        };

        unsafe {
            arena.as_ref().balance.update(|b| b + 1);
            arena.as_ref().adjust_allocated(size.try_into().unwrap());

            if this.vtbl().as_ref().cyclic {
                this.base().as_ref().cyclic.init();
                this.base().as_ref().queue.init();
                arena.as_ref().cyclic.push_front(this.base());
            }
        }
        this
    }

    pub(crate) unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        Self {
            ptr,
            phantom: PhantomData,
        }
    }

    pub(crate) unsafe fn from_weak(weak: &BaseWeak<'v, T>) -> Self {
        debug_assert_ne!(weak.strong_count(), 0);
        Self {
            ptr: weak.ptr,
            phantom: PhantomData,
        }
    }

    pub(crate) fn into_raw(this: Self) -> NonNull<T> {
        let ptr = this.ptr;
        mem::forget(this);
        ptr
    }

    pub(crate) fn downgrade(this: &Self) -> BaseWeak<'v, T> {
        this.base_ref().retain_weak();
        unsafe { BaseWeak::from_raw(this.ptr) }
    }

    pub(crate) fn upcast<U: Upcast<Header>>(this: Self) -> Base<'v, U>
    where
        T: Upcast<U>,
    {
        let base = this.ptr;
        mem::forget(this);
        unsafe { Base::from_raw(base.cast()) }
    }

    pub(crate) fn strong_count(&self) -> usize {
        self.base_ref().strong.get()
    }

    pub(crate) fn weak_count(&self) -> usize {
        self.base_ref().weak.get()
    }

    pub(crate) fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        visit.visit(self.base())
    }

    pub(crate) fn borrow(&self) -> Option<BaseRef<'v, '_, T>> {
        BaseRef::from_raw(self.ptr)
    }

    pub(crate) fn borrow_mut(&self) -> Option<BaseMut<'v, '_, T>> {
        BaseMut::from_raw(self.ptr)
    }
}

impl<'v, T: Upcast<Header>> Clone for Base<'v, T> {
    fn clone(&self) -> Self {
        self.base_ref().retain();
        unsafe { Self::from_raw(self.ptr) }
    }
}

impl<'v, T: Upcast<Header>> Drop for Base<'v, T> {
    fn drop(&mut self) {
        let this = self.base();
        unsafe {
            if this.as_ref().release() {
                Header::released(this)
            }
        }
    }
}

/// Base garbage-collected weak pointer with least upper bound `T`
/// Intended to be wrapped by something exposing fewer details
pub(crate) struct BaseWeak<'v, T: Upcast<Header>> {
    ptr: NonNull<T>,
    phantom: PhantomData<(&'v mut &'v (), T)>,
}

impl<'v, T: Upcast<Header>> std::fmt::Debug for BaseWeak<'v, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseWeak").finish()
    }
}

impl<'v, T: Upcast<Header>> BaseWeak<'v, T> {
    fn base(&self) -> NonNull<Header> {
        self.ptr.cast()
    }

    fn base_ref(&self) -> &Header {
        unsafe { self.base().as_ref() }
    }

    pub(crate) unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        Self {
            ptr,
            phantom: PhantomData,
        }
    }

    pub(crate) fn upgrade(&self) -> Option<Base<'v, T>> {
        if self.base_ref().try_retain() {
            Some(unsafe { Base::from_raw(self.ptr) })
        } else {
            None
        }
    }

    #[expect(dead_code)]
    fn upcast<U: Upcast<Header>>(self) -> BaseWeak<'v, U>
    where
        T: Upcast<U>,
    {
        let base = self.ptr;
        mem::forget(self);
        unsafe { BaseWeak::from_raw(base.cast()) }
    }

    pub(crate) fn strong_count(&self) -> usize {
        self.base_ref().strong.get()
    }

    pub(crate) fn ptr_eq_strong(&self, other: &Base<'v, T>) -> bool {
        self.ptr == other.ptr
    }
}

impl<'v, T: Upcast<Header>> Clone for BaseWeak<'v, T> {
    fn clone(&self) -> Self {
        self.base_ref().retain_weak();
        unsafe { Self::from_raw(self.ptr) }
    }
}

impl<'v, T: Upcast<Header>> Drop for BaseWeak<'v, T> {
    fn drop(&mut self) {
        let this = self.base();
        unsafe {
            if this.as_ref().release_weak() {
                Header::released_weak(this)
            }
        }
    }
}

pub(crate) struct BaseBorrow<'v, 'a, T> {
    base: NonNull<T>,
    phantom: PhantomData<(&'v mut &'v (), &'a T)>,
}

impl<'v, 'a, T> PartialEq for BaseBorrow<'v, 'a, T> {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

impl<'v, 'a, T> Eq for BaseBorrow<'v, 'a, T> {}

impl<'v, 'a, T> Hash for BaseBorrow<'v, 'a, T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.hash(state);
    }
}

impl<'v, 'a, T> Clone for BaseBorrow<'v, 'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, 'a, T> Copy for BaseBorrow<'v, 'a, T> {}

impl<'v, 'a, T: Upcast<Header>> BaseBorrow<'v, 'a, T> {
    pub(crate) unsafe fn new(ptr: NonNull<T>) -> Self {
        Self {
            base: ptr,
            phantom: PhantomData,
        }
    }

    pub(crate) fn to_strong(self) -> Base<'v, T> {
        unsafe {
            self.base.cast::<Header>().as_ref().retain();
            Base::from_raw(self.base)
        }
    }

    pub(crate) fn into_raw(self) -> NonNull<T> {
        self.base
    }

    pub(crate) fn base_get(&self) -> &'a T {
        unsafe { self.base.as_ref() }
    }

    pub(crate) fn upcast<U: Upcast<Header>>(self) -> BaseBorrow<'v, 'a, U>
    where
        T: Upcast<U>,
    {
        unsafe { BaseBorrow::new(self.base.cast()) }
    }

    pub(crate) fn borrow(&self) -> Option<BaseRef<'v, 'a, T>> {
        BaseRef::from_raw(self.base)
    }

    pub(crate) fn borrow_mut(&self) -> Option<BaseMut<'v, 'a, T>> {
        BaseMut::from_raw(self.base)
    }

    #[expect(dead_code)]
    pub(crate) fn strong_count(&self) -> usize {
        unsafe { self.base.cast::<Header>().as_ref().strong.get() }
    }
}

pub(crate) struct BaseRef<'v, 'a, T: Upcast<Header>> {
    ptr: NonNull<T>,
    phantom: PhantomData<(&'v mut &'v (), &'a T)>,
}

impl<'v, 'a, T: Upcast<Header>> BaseRef<'v, 'a, T> {
    fn from_raw(ptr: NonNull<T>) -> Option<Self> {
        if !unsafe { ptr.cast::<Header>().as_ref().borrow_imm() } {
            return None;
        }
        Some(Self {
            ptr,
            phantom: PhantomData,
        })
    }

    #[expect(dead_code)]
    pub(crate) fn to_strong(this: &Self) -> Base<'v, T> {
        unsafe {
            this.ptr.cast::<Header>().as_ref().retain();
            Base::from_raw(this.ptr)
        }
    }
}

impl<'v, 'a, T: Upcast<Header>> Drop for BaseRef<'v, 'a, T> {
    fn drop(&mut self) {
        unsafe { self.ptr.cast::<Header>().as_ref().unborrow_imm() }
    }
}

pub(crate) struct BaseMut<'v, 'a, T: Upcast<Header>> {
    ptr: NonNull<T>,
    phantom: PhantomData<(&'v mut &'v (), &'a mut T)>,
}

impl<'v, 'a, T: Upcast<Header>> BaseMut<'v, 'a, T> {
    fn from_raw(ptr: NonNull<T>) -> Option<Self> {
        if !unsafe { ptr.cast::<Header>().as_ref().borrow_mut() } {
            return None;
        }
        Some(Self {
            ptr,
            phantom: PhantomData,
        })
    }
}

impl<'v, 'a, T: Upcast<Header>> Drop for BaseMut<'v, 'a, T> {
    fn drop(&mut self) {
        unsafe { self.ptr.cast::<Header>().as_ref().unborrow_mut() }
    }
}

#[repr(C)]
pub(crate) struct BoxedSized<H: Upcast<Header>, T: Collect> {
    header: H,
    value: UnsafeCell<T>,
    annex: T::Annex,
}

impl<H: Upcast<Header>, T: Collect> BoxedSized<H, T> {
    pub(crate) fn from_parts(header: H, value: T) -> Self
    where
        T::Annex: Default,
    {
        Self {
            header,
            value: UnsafeCell::new(value),
            annex: Default::default(),
        }
    }

    pub(crate) fn from_parts_annex(header: H, value: T, annex: T::Annex) -> Self {
        Self {
            header,
            value: UnsafeCell::new(value),
            annex,
        }
    }
}

unsafe impl<H: Upcast<Header>, T: Collect> Upcast<H> for BoxedSized<H, T> {}
unsafe impl<H: Upcast<Header>, T: Collect> Upcast<BoxedSized<H, T>> for BoxedSized<H, T> {}

#[repr(C)]
pub(crate) struct BoxedSlice<H: Upcast<Header>, T> {
    header: H,
    len: usize,
    _value: [UnsafeCell<T>; 0],
}

unsafe impl<H: Upcast<Header>, T> Upcast<H> for BoxedSlice<H, T> {}
unsafe impl<H: Upcast<Header>, T> Upcast<BoxedSlice<H, T>> for BoxedSlice<H, T> {}

impl<H: Upcast<Header>, T> BoxedSlice<H, T> {
    fn raw_values(this: NonNull<Self>) -> NonNull<UnsafeCell<T>> {
        unsafe { this.cast::<u8>().add(size_of::<Self>()).cast() }
    }
}

impl<H: Upcast<Header>, T> Drop for BoxedSlice<H, T> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() {
            unsafe {
                let values = Self::raw_values(NonNull::from_mut(self));
                for i in 0..self.len {
                    ptr::drop_in_place(values.as_ptr().add(i));
                }
            }
        }
    }
}

type BoxedStr<H> = BoxedSlice<H, u8>;

pub(crate) struct Embryo<'v, H: Upcast<Header>, T: ?Sized + Boxable<H>> {
    ptr: NonNull<u8>,
    layout: Layout,
    phantom: PhantomData<(&'v mut &'v (), H, T)>,
}

impl<'v, H: Upcast<Header>, T: ?Sized + Boxable<H>> Drop for Embryo<'v, H, T> {
    fn drop(&mut self) {
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

impl<'v, H: Upcast<Header>> Embryo<'v, H, [u8]>
where
    Boxed<H, [u8]>: Upcast<Header>,
    [u8]: Boxable<H>,
{
    pub(crate) unsafe fn from_arena_capacity(arena: &Arena<'v>, cap: usize) -> Self {
        let layout = slice_layout::<H, u8>(cap);
        let ptr = unsafe { alloc::alloc(layout) };
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        let boxed = ptr.cast::<BoxedSlice<H, u8>>();
        unsafe {
            ptr.cast::<NonNull<arena::ArenaInner>>()
                .write(NonNull::from_ref(&arena.0));
            (&raw mut (*boxed).len).write(0);
        }
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
            layout,
            phantom: PhantomData,
        }
    }

    pub(crate) fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.data_ptr().add(self.len()).cast::<MaybeUninit<u8>>(),
                self.capacity() - self.len(),
            )
        }
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        if additional <= self.capacity() - self.len() {
            return;
        }
        let required = self
            .len()
            .checked_add(additional)
            .expect("embryo capacity overflow");
        let grown = self.capacity().saturating_mul(2).max(8);
        self.realloc(required.max(grown));
    }

    pub(crate) unsafe fn advance(&mut self, initialized: usize) {
        debug_assert!(initialized <= self.capacity() - self.len());
        unsafe { self.set_len(self.len().strict_add(initialized)) };
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        if len >= self.len() {
            return;
        }
        unsafe { self.set_len(len) }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data_ptr(), self.len()) }
    }

    /// # Safety
    /// The embryo must have at least `slice.len()` bytes of spare capacity.
    pub(crate) unsafe fn extend(&mut self, slice: &[u8]) {
        unsafe {
            ptr::copy_nonoverlapping(slice.as_ptr(), self.data_ptr().add(self.len()), slice.len());
            self.advance(slice.len());
        }
    }

    pub(crate) fn finalize(mut self, header: H) -> Base<'v, Boxed<H, [u8]>> {
        self.shrink_to_fit();
        unsafe {
            debug_assert_eq!(self.arena_inner(), Self::arena_inner_from_header(&header));
            self.header_ptr().write(header);
            let arena = self.arena_inner();
            let ptr = self.ptr.cast::<Boxed<H, [u8]>>();
            let size = self.layout.size();
            mem::forget(self);
            Base::new_from_raw_inner(arena, ptr, size)
        }
    }

    fn data_ptr(&self) -> *mut u8 {
        unsafe { self.ptr.as_ptr().add(size_of::<BoxedSlice<H, u8>>()) }
    }

    fn realloc(&mut self, new_cap: usize) {
        let old_layout = self.layout;
        let new_layout = slice_layout::<H, u8>(new_cap);
        let ptr = unsafe { alloc::realloc(self.ptr.as_ptr(), old_layout, new_layout.size()) };
        if ptr.is_null() {
            alloc::handle_alloc_error(new_layout);
        }
        self.ptr = unsafe { NonNull::new_unchecked(ptr) };
        self.layout = new_layout;
    }

    fn shrink_to_fit(&mut self) {
        if self.len() == self.capacity() {
            return;
        }
        self.realloc(self.len());
    }

    fn len(&self) -> usize {
        unsafe { self.len_ptr().read() }
    }

    unsafe fn set_len(&mut self, len: usize) {
        unsafe { self.len_ptr().write(len) }
    }

    pub(crate) fn capacity(&self) -> usize {
        (self.layout.size() - size_of::<BoxedSlice<H, u8>>()) / size_of::<u8>()
    }

    fn header_ptr(&mut self) -> *mut H {
        let boxed = self.ptr.cast::<BoxedSlice<H, u8>>().as_ptr();
        unsafe { &raw mut (*boxed).header }
    }

    fn len_ptr(&self) -> *mut usize {
        let boxed = self.ptr.cast::<BoxedSlice<H, u8>>().as_ptr();
        unsafe { &raw mut (*boxed).len }
    }

    unsafe fn arena_inner(&self) -> NonNull<arena::ArenaInner> {
        unsafe {
            self.ptr
                .cast::<NonNull<arena::ArenaInner>>()
                .as_ptr()
                .read()
        }
    }

    fn arena_inner_from_header(header: &H) -> NonNull<arena::ArenaInner> {
        unsafe { NonNull::from_ref(header).cast::<Header>().as_ref().arena }
    }
}

impl<'v, H: Upcast<Header>> Embryo<'v, H, [u8]>
where
    Boxed<H, [u8]>: Upcast<Header>,
    [u8]: Boxable<H>,
    Boxed<H, str>: Upcast<Header>,
    str: Boxable<H>,
{
    /// # Safety
    /// The initialized bytes must be valid UTF-8.
    pub(crate) unsafe fn finalize_str_unchecked(mut self, header: H) -> Base<'v, Boxed<H, str>> {
        self.shrink_to_fit();
        unsafe {
            debug_assert_eq!(self.arena_inner(), Self::arena_inner_from_header(&header));
            self.header_ptr().write(header);
            let arena = self.arena_inner();
            let ptr = self.ptr.cast::<Boxed<H, str>>();
            let size = self.layout.size();
            mem::forget(self);
            Base::new_from_raw_inner(arena, ptr, size)
        }
    }
}

pub(crate) trait Boxable<H: Upcast<Header>> {
    type Inner: Upcast<Header>;
    const VTBL: &Vtbl;

    /// # Safety
    /// This is unsafe unless the header was correctly allocated
    /// with space for the dynamic portion and initialized
    unsafe fn deref_inner(inner: NonNull<<Self as Boxable<H>>::Inner>) -> NonNull<Self>;
}

const fn sized_layout<H: Upcast<Header>, T: Collect>() -> Layout {
    match Layout::new::<BoxedSized<H, T>>().align_to(1 << tag::WIDTH) {
        Ok(l) => l,
        Err(_) => panic!(),
    }
}

fn slice_layout<H: Upcast<Header>, T>(len: usize) -> Layout {
    Layout::new::<BoxedSlice<H, T>>()
        .extend(Layout::array::<T>(len).unwrap())
        .and_then(|(l, _)| l.align_to(1 << tag::WIDTH))
        .unwrap()
}

impl<H: Upcast<Header>, T: Sized + Collect> Boxable<H> for T
where
    BoxedSized<H, T>: Upcast<Header>,
{
    type Inner = BoxedSized<H, T>;
    const VTBL: &Vtbl = &Vtbl {
        cyclic: T::CYCLIC,
        immutable: T::IMMUTABLE,
        is_strand: T::STRAND,
        name: || type_name::<T>(),
        drop: |this| unsafe {
            T::finalize(this);
            let boxed = this.cast::<BoxedSized<H, T>>();
            ptr::drop_in_place(boxed.as_ptr());
        },
        dealloc: |this| unsafe {
            alloc::dealloc(this.as_ptr() as *mut _, sized_layout::<H, T>());
            sized_layout::<H, T>().size()
        },
        trace: |this, visit| unsafe {
            let boxed = this.cast::<BoxedSized<H, T>>();
            (*boxed.as_ref().value.get()).accept(visit)?;
            boxed.as_ref().annex.accept(visit)
        },
        clear: |this| unsafe {
            let boxed = this.cast::<BoxedSized<H, T>>();
            T::finalize(this);
            (*boxed.as_ref().value.get()).clear();
            boxed.as_ref().annex.clear()
        },
    };

    unsafe fn deref_inner(inner: NonNull<<Self as Boxable<H>>::Inner>) -> NonNull<Self> {
        unsafe { NonNull::new_unchecked((*inner.as_ptr()).value.get()) }
    }
}

impl<H: Upcast<Header>, T: Collect> Boxable<H> for [T]
where
    BoxedSlice<H, T>: Upcast<Header>,
{
    type Inner = BoxedSlice<H, T>;
    const VTBL: &Vtbl = &Vtbl {
        cyclic: T::CYCLIC,
        immutable: T::IMMUTABLE,
        is_strand: false,
        name: || type_name::<T>(),
        drop: |this| unsafe {
            if mem::needs_drop::<T>() {
                let boxed = this.cast::<BoxedSlice<H, T>>();
                let len = (*boxed.as_ptr()).len;
                let values = BoxedSlice::raw_values(boxed);
                for i in 0..len {
                    ptr::drop_in_place(values.add(i).as_ptr());
                }
            }
        },
        dealloc: |this| unsafe {
            let boxed = this.cast::<BoxedSlice<H, T>>();
            let len = boxed.as_ref().len;
            let layout = slice_layout::<H, T>(len);
            alloc::dealloc(this.as_ptr() as *mut _, layout);
            layout.size()
        },
        trace: |this, visit| unsafe {
            let boxed = this.cast::<BoxedSlice<H, T>>();
            let len = (*boxed.as_ptr()).len;
            let values = BoxedSlice::raw_values(boxed);
            for i in 0..len {
                if let ControlFlow::Break(()) = (*values.add(i).as_ref().get()).accept(visit) {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        },
        clear: |this| unsafe {
            let boxed = this.cast::<BoxedSlice<H, T>>();
            let len = (*boxed.as_ptr()).len;
            let values = BoxedSlice::raw_values(boxed);
            for i in 0..len {
                (*values.add(i).as_ref().get()).clear()
            }
        },
    };

    unsafe fn deref_inner(inner: NonNull<<Self as Boxable<H>>::Inner>) -> NonNull<Self> {
        unsafe {
            NonNull::slice_from_raw_parts(
                BoxedSlice::raw_values(inner).cast(),
                (*inner.as_ptr()).len,
            )
        }
    }
}

unsafe impl<T: Collect> Collect for [T] {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = T::Annex;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        for elem in self.iter() {
            elem.accept(visit)?
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        for elem in self.iter_mut() {
            elem.clear()
        }
    }
}

impl<H: Upcast<Header>> Boxable<H> for str
where
    BoxedStr<H>: Upcast<Header>,
{
    type Inner = BoxedStr<H>;
    const VTBL: &Vtbl = &Vtbl {
        cyclic: false,
        immutable: true,
        is_strand: false,
        name: || "str",
        drop: |_| (),
        dealloc: |this| unsafe {
            let boxed = this.cast::<BoxedStr<H>>();
            let len = boxed.as_ref().len;
            let layout = slice_layout::<H, u8>(len);
            alloc::dealloc(this.as_ptr() as *mut _, layout);
            layout.size()
        },
        trace: |_, _| ControlFlow::Continue(()),
        clear: |_| (),
    };

    unsafe fn deref_inner(inner: NonNull<<Self as Boxable<H>>::Inner>) -> NonNull<Self> {
        unsafe {
            mem::transmute::<NonNull<[u8]>, NonNull<str>>(NonNull::slice_from_raw_parts(
                if (*inner.as_ptr()).len == 0 {
                    NonNull::dangling()
                } else {
                    let layout = Layout::new::<<Self as Boxable<H>>::Inner>();
                    let this = inner.cast::<u8>();
                    this.add(layout.size())
                },
                (*inner.as_ptr()).len,
            ))
        }
    }
}

#[repr(C)]
pub(crate) struct Boxed<H: Upcast<Header>, T: ?Sized + Boxable<H>>(<T as Boxable<H>>::Inner);

impl<H: Upcast<Header>, T: ?Sized + Boxable<H>> Boxed<H, T> {
    const VTBL: &Vtbl = <T as Boxable<H>>::VTBL;

    pub(crate) const fn vtbl() -> &'static Vtbl {
        Self::VTBL
    }
}

unsafe impl<H: Upcast<Header>, T: ?Sized + Boxable<H>> Upcast<H> for Boxed<H, T> {}
unsafe impl<H: Upcast<Header>, T: ?Sized + Boxable<H>> Upcast<Boxed<H, T>> for Boxed<H, T> {}

impl<'v, H: Upcast<Header>, T: Collect + Boxable<Header>> Base<'v, Boxed<H, T>>
where
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    pub(crate) unsafe fn from_parts(arena: &Arena<'v>, header: H, value: T) -> Self
    where
        T::Annex: Default,
    {
        let boxed = BoxedSized::from_parts(header, value);
        unsafe {
            let layout = sized_layout::<H, T>();
            let alloc = alloc::alloc(layout) as *mut BoxedSized<H, T>;
            if alloc.is_null() {
                alloc::handle_alloc_error(layout);
            }
            alloc.write(boxed);

            Base::new_from_raw(
                arena,
                NonNull::new_unchecked(alloc as *mut Boxed<H, T>),
                layout.size(),
            )
        }
    }

    pub(crate) unsafe fn from_parts_annex(
        arena: &Arena<'v>,
        header: H,
        value: T,
        annex: T::Annex,
    ) -> Self {
        let boxed = BoxedSized::from_parts_annex(header, value, annex);
        unsafe {
            let layout = sized_layout::<H, T>();
            let alloc = alloc::alloc(layout) as *mut BoxedSized<H, T>;
            if alloc.is_null() {
                alloc::handle_alloc_error(layout);
            }
            alloc.write(boxed);

            Base::new_from_raw(
                arena,
                NonNull::new_unchecked(alloc as *mut Boxed<H, T>),
                layout.size(),
            )
        }
    }
}

impl<'v, H: Upcast<Header>, T> Base<'v, Boxed<H, [T]>>
where
    Boxed<H, [T]>: Upcast<Header>,
    [T]: Boxable<H>,
{
    pub(crate) unsafe fn from_header_iter(
        arena: &Arena<'v>,
        header: H,
        iter: impl ExactSizeIterator<Item = T>,
    ) -> Self {
        let layout = slice_layout::<H, T>(iter.len());
        unsafe {
            let boxed = alloc::alloc(layout) as *mut BoxedSlice<H, T>;
            if boxed.is_null() {
                alloc::handle_alloc_error(layout);
            }
            ptr::write(&raw mut (*boxed).header, header);
            ptr::write(&raw mut (*boxed).len, iter.len());
            let slots = (boxed as *mut u8).add(size_of::<BoxedSlice<H, T>>()) as *mut UnsafeCell<T>;
            for (i, e) in iter.enumerate() {
                (*slots.add(i)).get().write(e);
            }
            Base::new_from_raw(
                arena,
                NonNull::new_unchecked(boxed as *mut Boxed<H, [T]>),
                layout.size(),
            )
        }
    }
}

impl<'v, H: Upcast<Header>> Base<'v, Boxed<H, [u8]>>
where
    Boxed<H, [u8]>: Upcast<Header>,
    [u8]: Boxable<H>,
{
    pub(crate) unsafe fn from_header_slice(arena: &Arena<'v>, header: H, value: &[u8]) -> Self {
        let embryo = unsafe {
            let mut embryo = Embryo::<H, [u8]>::from_arena_capacity(arena, value.len());
            embryo.extend(value);
            embryo
        };
        embryo.finalize(header)
    }
}

impl<'v, H: Upcast<Header>> Base<'v, Boxed<H, str>>
where
    Boxed<H, str>: Upcast<Header>,
    str: Boxable<H>,
    Boxed<H, [u8]>: Upcast<Header>,
    [u8]: Boxable<H>,
{
    pub(crate) unsafe fn from_header_utf8_slice(
        arena: &Arena<'v>,
        header: H,
        value: &[u8],
    ) -> Self {
        unsafe {
            let mut embryo = Embryo::<H, [u8]>::from_arena_capacity(arena, value.len());
            embryo.extend(value);
            embryo.finalize_str_unchecked(header)
        }
    }
}

pub(crate) type Box<'v, H, T> = Base<'v, Boxed<H, T>>;

impl<'v, H: Upcast<Header>, T: ?Sized + Boxable<H> + Collect> Box<'v, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    pub(crate) fn try_get(&self) -> Option<*const T> {
        if self.strong_count() == 1 && self.weak_count() == 1 {
            Some(unsafe { <T as Boxable<H>>::deref_inner(self.ptr.cast()) }.as_ptr())
        } else {
            None
        }
    }

    pub(crate) fn try_get_mut(&mut self) -> Option<*mut T> {
        if self.strong_count() == 1 && self.weak_count() == 1 {
            Some(unsafe { <T as Boxable<H>>::deref_inner(self.ptr.cast()) }.as_ptr())
        } else {
            None
        }
    }
}

impl<'v, H: Upcast<Header>, T: ?Sized + Boxable<H> + Collect> Deref for Box<'v, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        const { assert!(<T as Collect>::IMMUTABLE) }
        unsafe { <T as Boxable<H>>::deref_inner(self.ptr.cast()).as_ref() }
    }
}

impl<'v, H: Upcast<Header>, T: ?Sized + Boxable<H> + Collect> AsRef<T> for Box<'v, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    fn as_ref(&self) -> &T {
        self
    }
}

pub(crate) type BoxWeak<'v, H, T> = BaseWeak<'v, Boxed<H, T>>;

impl<'v, H: Upcast<Header>, T: ?Sized + Boxable<H>> BoxWeak<'v, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    pub(crate) unsafe fn get_unchecked(&self) -> &T {
        debug_assert_ne!(self.strong_count(), 0);
        unsafe { Boxable::deref_inner(self.ptr.cast()).as_ref() }
    }
}

pub(crate) type Gc<'v, T> = Box<'v, Header, T>;

impl<'v, T: Collect + Boxable<Header>> Gc<'v, T> {
    pub(crate) fn new(arena: &Arena<'v>, value: T) -> Self
    where
        T::Annex: Default,
    {
        unsafe {
            Gc::from_parts(
                arena,
                Header::new(arena, NonNull::from_ref(<T as Boxable<Header>>::VTBL)),
                value,
            )
        }
    }
}

pub(crate) type Weak<'v, T> = BoxWeak<'v, Header, T>;

pub(crate) type Borrow<'v, 'a, H, T> = BaseBorrow<'v, 'a, Boxed<H, T>>;

impl<'v, 'a, H: Upcast<Header>, T: Boxable<H, Inner = BoxedSized<H, T>> + Collect>
    Borrow<'v, 'a, H, T>
{
    pub(crate) fn annex(&self) -> &'a T::Annex
    where
        T: Collect,
    {
        unsafe { &self.base.cast::<BoxedSized<H, T>>().as_ref().annex }
    }
}

impl<'v, 'a, H: Upcast<Header>, T: ?Sized + Boxable<H>> Borrow<'v, 'a, H, T> {
    pub(crate) unsafe fn from_raw(ptr: NonNull<<T as Boxable<H>>::Inner>) -> Self {
        Self {
            base: ptr.cast(),
            phantom: PhantomData,
        }
    }

    pub(crate) fn as_header(self) -> NonNull<H> {
        self.base.cast()
    }

    pub(crate) fn get(&self) -> &'a T
    where
        T: Collect,
        <T as Boxable<H>>::Inner: 'a,
    {
        const { assert!(<T as Collect>::IMMUTABLE) }
        unsafe { Boxable::deref_inner(self.base.cast()).as_ref() }
    }
}

pub(crate) type Ref<'v, 'a, H, T> = BaseRef<'v, 'a, Boxed<H, T>>;

impl<'v, 'a, H: Upcast<Header>, T: Boxable<H, Inner = BoxedSized<H, T>> + Collect> Ref<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    pub(crate) fn annex(this: &Self) -> &'a T::Annex
    where
        T: Collect,
    {
        unsafe { &this.ptr.cast::<BoxedSized<H, T>>().as_ref().annex }
    }
}

impl<'v, 'a, H: Upcast<Header>, T: ?Sized + Boxable<H>> Deref for Ref<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { <T as Boxable<H>>::deref_inner(self.ptr.cast()).as_ref() }
    }
}

impl<'v, 'a, H: Upcast<Header>, T: ?Sized + Boxable<H>> AsRef<T> for Ref<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    fn as_ref(&self) -> &T {
        self
    }
}

pub(crate) type Mut<'v, 'a, H, T> = BaseMut<'v, 'a, Boxed<H, T>>;

impl<'v, 'a, H: Upcast<Header>, T: Boxable<H, Inner = BoxedSized<H, T>> + Collect> Mut<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    pub(crate) fn annex(this: &Self) -> &'a T::Annex
    where
        T: Collect,
    {
        unsafe { &this.ptr.cast::<BoxedSized<H, T>>().as_ref().annex }
    }
}

impl<'v, 'a, H: Upcast<Header>, T: ?Sized + Boxable<H>> Deref for Mut<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { <T as Boxable<H>>::deref_inner(self.ptr.cast()).as_ref() }
    }
}

impl<'v, 'a, H: Upcast<Header>, T: ?Sized + Boxable<H>> DerefMut for Mut<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *<T as Boxable<H>>::deref_inner(self.ptr.cast()).as_ptr() }
    }
}

impl<'v, 'a, H: Upcast<Header>, T: ?Sized + Boxable<H>> AsRef<T> for Mut<'v, 'a, H, T>
where
    Boxed<H, T>: Upcast<Header>,
{
    fn as_ref(&self) -> &T {
        self
    }
}
