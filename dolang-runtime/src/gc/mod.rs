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

use dolang_util::alias;

use crate::{
    strand::{Access, PinScope},
    value::repr::tag,
};

pub(crate) mod arena;

/// Run a block with a GC object's contents pinned for its duration.
///
/// Invoke as `with_pin!(strand, obj, |pin| body)`.  The closure-like pattern binds the
/// [`BasePin`], which can't outlive the body.  `strand` must be the name of the current
/// [`Strand`].  The body runs inline, as for [`with_slots!`](crate::with_slots): `?` and
/// `return` leave the enclosing function.
macro_rules! with_pin {
    ($strand:ident, $obj:expr, |$pin:pat_param| $body:expr) => {{
        let obj = &$obj;
        // The pin is tied to the strand, as a moving collector's pin list would be
        let _: &$crate::strand::Strand<'_, '_> = $strand;
        let scope = $crate::strand::PinScope;
        // Safety: `scope` is a local of this block
        #[allow(unused_unsafe)]
        let $pin = unsafe { obj.scoped_pin(&scope) };
        $body
    }};
}
pub(crate) use with_pin;

use arena::{Arena, Header, Upcast, Visit, Vtbl};

/// Trait for types used as the `Annex` of a [`Collect`] type.
///
/// Unlike `Collect`, this is not `unsafe` — implementors simply provide
/// GC traversal and clearing with interior-mutability semantics (`&self`).
pub(crate) trait Annex {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()>;
    /// Clear GC-rooted values held by this annex.
    /// Takes `&self` to allow interior mutability (e.g. `UnsafeCell`).
    ///
    /// Immutable state needs no clearing: an immutable object can only reference
    /// objects that existed before it, so any cycle through it also passes through
    /// mutable state, which is cleared. Only state behind interior mutability
    /// must be cleared.
    fn clear(&self) {}
}

impl Annex for () {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self) {}
}

/// How an annex is laid out in its GC box: either sized, or a [`Tail`] whose
/// length is recorded in the box.
///
/// # Safety
/// `layout(meta)` must have alignment `ALIGN` and describe the annex that
/// `from_raw(ptr, meta)` points to.
pub(crate) unsafe trait AnnexLayout {
    /// What the box records to recover the annex: `()` when sized, the tail length
    /// for a [`Tail`].
    type Meta: Copy + Default;
    const ALIGN: usize;

    fn layout(meta: Self::Meta) -> Layout;

    fn from_raw(ptr: *mut u8, meta: Self::Meta) -> *mut Self;
}

unsafe impl<A: Annex> AnnexLayout for A {
    type Meta = ();
    const ALIGN: usize = align_of::<A>();

    fn layout((): ()) -> Layout {
        Layout::new::<A>()
    }

    fn from_raw(ptr: *mut u8, (): ()) -> *mut Self {
        ptr.cast()
    }
}

/// An annex with a fixed `prefix` followed by a flexible array member.
///
/// A box records the tail length, so its annex can be a dynamically sized
/// `Tail<P, [E]>`. The prefix decides how the tail is traced and cleared
/// through [`TailAnnex`].
#[repr(C)]
pub(crate) struct Tail<P, T: ?Sized> {
    pub(crate) prefix: P,
    pub(crate) tail: T,
}

// Safety: `repr(C)` places `tail` after `prefix`, aligned to `E`, and a pointer
// cast between unsized types keeps the metadata: the element count.
unsafe impl<P, E> AnnexLayout for Tail<P, [E]> {
    type Meta = usize;
    const ALIGN: usize = if align_of::<P>() > align_of::<E>() {
        align_of::<P>()
    } else {
        align_of::<E>()
    };

    fn layout(len: usize) -> Layout {
        Layout::new::<P>()
            .extend(Layout::array::<E>(len).unwrap())
            .unwrap()
            .0
            .pad_to_align()
    }

    fn from_raw(ptr: *mut u8, len: usize) -> *mut Self {
        ptr::slice_from_raw_parts_mut(ptr, len) as *mut Self
    }
}

// Safety: as for `Tail<P, [E]>`; `str` metadata is its length in bytes.
unsafe impl<P> AnnexLayout for Tail<P, str> {
    type Meta = usize;
    const ALIGN: usize = align_of::<P>();

    fn layout(len: usize) -> Layout {
        <Tail<P, [u8]> as AnnexLayout>::layout(len)
    }

    fn from_raw(ptr: *mut u8, len: usize) -> *mut Self {
        ptr::slice_from_raw_parts_mut(ptr, len) as *mut Self
    }
}

impl<P: Annex> Annex for Tail<P, str> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.prefix.accept(visit)
    }

    fn clear(&self) {
        self.prefix.clear()
    }
}

/// Bytes hold no references.
impl TailAnnex<u8> for () {
    fn accept(&self, _tail: &[u8], _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&self, _tail: &[u8]) {}
}

/// Traces and clears a [`Tail`] annex, implemented by its prefix.
pub(crate) trait TailAnnex<E> {
    fn accept(&self, tail: &[E], visit: &mut dyn Visit) -> ControlFlow<()>;
    /// Clear GC-rooted values held by the annex. Immutable state needs no
    /// clearing: any cycle through it also passes through mutable state.
    fn clear(&self, tail: &[E]);
}

impl<P: TailAnnex<E>, E> Annex for Tail<P, [E]> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.prefix.accept(&self.tail, visit)
    }

    fn clear(&self) {
        self.prefix.clear(&self.tail)
    }
}

/// # Safety
/// This defines the metadata for garbage-collected types.
/// Implementors must correctly implement `accept` and `clear` to visit/zero
/// all strongly-referenced GC objects, or Strange Behavior will result.
pub(crate) unsafe trait Collect {
    const CYCLIC: bool;
    const STRAND: bool = false;
    type Annex: ?Sized + Annex + AnnexLayout;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()>;
    fn clear(&mut self);
    #[allow(unused_variables)]
    fn finalize(this: NonNull<Header>) {}
}

/// The state of an [`Imm`] object.
///
/// # Safety
/// As for [`Collect`]: `accept` must visit every strongly referenced GC object, and
/// `CYCLIC` must be true if any can be.
pub(crate) unsafe trait ImmState: Annex {
    const CYCLIC: bool;
}

/// An immutable object: a unit value whose state `T` is its annex.
///
/// Its state is reached without a runtime borrow, through [`Deref`] on its
/// handles and `get` on its borrows.
pub(crate) struct Imm<T>(PhantomData<T>);

impl<T> Imm<T> {
    /// The unit value. Not `new`, which would shadow constructors through aliases.
    pub(crate) const UNIT: Self = Self(PhantomData);
}

unsafe impl<T: ImmState> Collect for Imm<T> {
    const CYCLIC: bool = T::CYCLIC;
    type Annex = T;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

/// Base garbage-collected strong pointer with least upper bound `T`
///
/// Intended to be wrapped by something exposing fewer details.  A `&Base` witnesses
/// the object's location; dereferencing its contents takes a [`BasePin`].
///
/// Transparent over its pointer, so an object [`Value`](crate::value::Value) can be
/// viewed as one in place.
#[repr(transparent)]
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

    /// Creates another owned reference to the same object.
    ///
    /// # Safety
    /// The result is an owned reference the collector cannot see until it is moved
    /// into a scanned location.  It must get there before any possible collection:
    /// no allocation and no code reached with a `Strand` in between.
    pub(crate) unsafe fn dup(&self) -> Self {
        self.base_ref().retain();
        unsafe { Self::from_raw(self.ptr) }
    }

    pub(crate) unsafe fn from_weak(weak: &BaseWeak<'v, T>) -> Self {
        debug_assert_ne!(weak.strong_count(), 0);
        Self {
            ptr: weak.ptr,
            phantom: PhantomData,
        }
    }

    pub(crate) fn as_ptr(this: &Self) -> NonNull<T> {
        this.ptr
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

    /// Pin this object's contents for the borrow of `self`.
    ///
    /// # Safety
    /// No collection may happen before the pin's last use.  Prefer a scoped or
    /// [`Access`](crate::strand::Access) pin where one is available.
    pub(crate) unsafe fn pin_unchecked(&self) -> BasePin<'v, '_, T> {
        unsafe { BasePin::new_unchecked(self.ptr) }
    }

    /// Read from this object's contents through a transient pin.
    ///
    /// `f` is a function pointer, so it captures nothing that could reach a
    /// collector, and its result cannot borrow from the pin.
    #[inline]
    pub(crate) fn read<R>(&self, f: for<'p> fn(BasePin<'v, 'p, T>) -> R) -> R {
        // SAFETY: `f` cannot collect, and the pin does not outlive it
        f(unsafe { self.pin_unchecked() })
    }

    /// Project an out-of-line part of this object's contents through a transient pin.
    ///
    /// As [`Self::project_unchecked`], made safe by taking a function pointer as
    /// [`Self::read`] does.
    #[inline]
    pub(crate) fn project<'g, O: ?Sized + OutOfLine + 'g>(
        &'g self,
        f: for<'p> fn(BasePin<'v, 'p, T>) -> &'p O,
    ) -> &'g O::Target {
        // SAFETY: as for `read`
        unsafe { self.project_unchecked(f) }
    }

    /// Project an out-of-line part of this object's contents.
    ///
    /// `f` sees the contents through a pin and returns an [`OutOfLine`] owner inside
    /// them; its target lives in a separate allocation that doesn't move with the
    /// object, so the result is bounded by the borrow of `self` rather than the pin.
    ///
    /// # Safety
    /// As for [`Self::pin_unchecked`], for the duration of `f`.
    pub(crate) unsafe fn project_unchecked<'g, O: ?Sized + OutOfLine + 'g>(
        &'g self,
        f: impl for<'p> FnOnce(BasePin<'v, 'p, T>) -> &'p O,
    ) -> &'g O::Target {
        let owner: *const O::Target = &**f(unsafe { self.pin_unchecked() });
        // SAFETY: the owner stays in the object, which `self` keeps alive for `'g`, and
        // its target never moves with it.
        unsafe { &*owner }
    }
}

impl<'v, T: Upcast<Header>> Base<'v, T> {
    /// Pin this object's contents for the lifetime of `access`, inside which nothing
    /// can await or collect.
    #[inline]
    pub(crate) fn pin_access<'p>(&'p self, access: &'p Access<'v, '_>) -> BasePin<'v, 'p, T> {
        let _ = access;
        // SAFETY: nothing can collect while an `Access` is live
        unsafe { self.pin_unchecked() }
    }

    /// Pin this object's contents for the borrow of `scope`.
    ///
    /// This backs [`with_pin!`].  A no-op under the current collector; under a moving
    /// one the scope would push and pop an entry on the strand's pin list.
    ///
    /// # Safety
    ///
    /// `scope` must be a local of the block that uses the pin.
    #[inline]
    pub(crate) unsafe fn scoped_pin<'p>(&'p self, _scope: &'p PinScope) -> BasePin<'v, 'p, T> {
        // SAFETY: the caller's contract bounds the pin to its block
        unsafe { self.pin_unchecked() }
    }
}

/// Interior-mutable holder of an optional typed GC reference: the typed counterpart
/// of [`ValueCell`](crate::value::ValueCell).
///
/// A holder that is reachable while other code runs keeps its reference here.
/// Writing goes through a [`GcSlot`] from [`slot`](Self::slot), which drops the old
/// reference only after the write: a drop can run native code that reaches the same
/// cell.
#[repr(transparent)]
pub(crate) struct GcCell<'v, T: Upcast<Header>>(UnsafeCell<Option<Base<'v, T>>>);

impl<'v, T: Upcast<Header>> GcCell<'v, T> {
    #[inline]
    pub(crate) fn new(value: Option<Base<'v, T>>) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// Visit the stored reference for the garbage collector.
    #[inline]
    pub(crate) fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        // SAFETY: a `GcSlot` into the cell never keeps a `&mut` alive, and no write is
        // in progress while the collector runs.
        match unsafe { &*self.0.get() } {
            Some(base) => base.accept(visit),
            None => ControlFlow::Continue(()),
        }
    }

    /// Duplicate the stored reference.
    ///
    /// # Safety
    /// No `&mut` into the cell may be alive, and the result is loose, as for
    /// [`Base::dup`].
    #[inline]
    pub(crate) unsafe fn get(&self) -> Option<Base<'v, T>> {
        unsafe { (*self.0.get()).as_ref().map(|base| base.dup()) }
    }

    /// Borrow the stored reference as a location.
    ///
    /// # Safety
    /// Nothing may write the cell while the borrow is alive.
    #[inline]
    pub(crate) unsafe fn as_ref(&self) -> Option<&Base<'v, T>> {
        unsafe { (*self.0.get()).as_ref() }
    }

    /// Obtain a [`GcSlot`] for writing the cell.
    ///
    /// # Safety
    /// No other access to the cell may happen while the slot is alive, except reads
    /// that do not borrow across a write.
    #[inline]
    pub(crate) unsafe fn slot(&self) -> GcSlot<'v, '_, T> {
        // SAFETY: `UnsafeCell::get` is never null
        unsafe { GcSlot::new(NonNull::new_unchecked(self.0.get())) }
    }
}

/// Exclusive access to a scannable location holding an optional typed GC reference:
/// the typed counterpart of [`Slot`](crate::value::Slot).
///
/// Like a `Slot`, it never holds a `&mut` to the location except for the duration of
/// a write, so the collector can scan the location while the slot lives.
pub(crate) struct GcSlot<'v, 'a, T: Upcast<Header>>(
    NonNull<Option<Base<'v, T>>>,
    PhantomData<&'a mut Option<Base<'v, T>>>,
);

impl<'v, 'a, T: Upcast<Header>> GcSlot<'v, 'a, T> {
    /// Creates a slot over a location.
    ///
    /// # Safety
    /// As for [`Slot::new`](crate::value::Slot::new): the location stays scannable
    /// and address-stable for `'a`, and the slot has exclusive access to it.
    #[inline]
    pub(crate) unsafe fn new(inner: NonNull<Option<Base<'v, T>>>) -> Self {
        Self(inner, PhantomData)
    }

    /// Replaces the reference, returning the old one.
    #[inline]
    pub(crate) fn replace(&mut self, value: Option<Base<'v, T>>) -> Option<Base<'v, T>> {
        // SAFETY: the slot has exclusive access; the `&mut` lives only for the write
        unsafe { mem::replace(self.0.as_mut(), value) }
    }

    #[inline]
    pub(crate) fn take(&mut self) -> Option<Base<'v, T>> {
        self.replace(None)
    }

    /// Stores `value`, dropping the old reference only after the write: a drop can
    /// run native code that reaches this slot.
    #[inline]
    pub(crate) fn store(&mut self, value: Option<Base<'v, T>>) {
        let old = self.replace(value);
        drop(old);
    }
}

/// An owner whose [`Deref`] target lives in its own allocation.
///
/// # Safety
/// The target must stay at the same address while the owner moves, and must not be
/// freed or reallocated while the owner is only shared.
pub(crate) unsafe trait OutOfLine: Deref {}

unsafe impl<T: ?Sized> OutOfLine for std::boxed::Box<T> {}
unsafe impl<T: ?Sized> OutOfLine for alias::Box<T> {}
unsafe impl<T> OutOfLine for Vec<T> {}
unsafe impl OutOfLine for String {}
unsafe impl<T: Clone> OutOfLine for std::borrow::Cow<'static, [T]> {}
unsafe impl OutOfLine for std::borrow::Cow<'static, str> {}

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

/// Witness that a GC object's contents are pinned for `'p`.
///
/// A `&Base` (like a `&Value`) only witnesses the object's *location*: the reference
/// is address-stable and some scanned owner sits above it.  Dereferencing the
/// object's contents additionally requires that they not move, which a pin
/// witnesses.  Under the current non-moving collector every pin is free; the
/// minting paths differ in what they would cost under a moving one:
///
/// - [`Base::pin_access`] mints pins for the lifetime of an [`Access`], since nothing
///   can await or collect inside it.
/// - [`with_pin!`] scopes a pin to a block that may allocate and await.  Under a
///   moving collector it would push and pop a strand-local pin list, whose entries
///   a stop-the-world collection promotes to live pins.
/// - [`Base::pin_unchecked`] is for low-level code, such as protocol dispatch and
///   transient accesses with no possible collection before the pin's last use.
pub(crate) struct BasePin<'v, 'p, T> {
    base: NonNull<T>,
    phantom: PhantomData<(&'v mut &'v (), &'p T)>,
}

impl<'v, 'p, T> PartialEq for BasePin<'v, 'p, T> {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

impl<'v, 'p, T> Eq for BasePin<'v, 'p, T> {}

impl<'v, 'p, T> Hash for BasePin<'v, 'p, T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.hash(state);
    }
}

impl<'v, 'p, T> Clone for BasePin<'v, 'p, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, 'p, T> Copy for BasePin<'v, 'p, T> {}

impl<'v, 'p, T: Upcast<Header>> BasePin<'v, 'p, T> {
    /// # Safety
    /// `ptr` must point to a live object whose contents stay pinned for `'p`.
    pub(crate) unsafe fn new_unchecked(ptr: NonNull<T>) -> Self {
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

    pub(crate) fn borrow(&self) -> Option<BaseRef<'v, 'p, T>> {
        BaseRef::from_raw(self.base)
    }

    pub(crate) fn borrow_mut(&self) -> Option<BaseMut<'v, 'p, T>> {
        BaseMut::from_raw(self.base)
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

type Meta<T> = <<T as Collect>::Annex as AnnexLayout>::Meta;

/// A GC box with a sized value: the header, the annex's [`AnnexLayout::Meta`] and
/// the value, followed by the annex at [`Self::ANNEX_OFFSET`].
#[repr(C)]
pub(crate) struct BoxedSized<H: Upcast<Header>, T: Collect> {
    header: H,
    meta: Meta<T>,
    value: UnsafeCell<T>,
}

impl<H: Upcast<Header>, T: Collect> BoxedSized<H, T> {
    const ANNEX_OFFSET: usize =
        size_of::<Self>().next_multiple_of(<T::Annex as AnnexLayout>::ALIGN);

    fn layout(meta: Meta<T>) -> Layout {
        let (layout, offset) = Layout::new::<Self>()
            .extend(T::Annex::layout(meta))
            .unwrap();
        debug_assert_eq!(offset, Self::ANNEX_OFFSET);
        layout.align_to(1 << tag::WIDTH).unwrap()
    }

    /// # Safety
    /// `this` must point to a box whose `meta` is initialized.
    unsafe fn annex_ptr(this: NonNull<Self>) -> *mut T::Annex {
        unsafe {
            T::Annex::from_raw(
                this.cast::<u8>().as_ptr().add(Self::ANNEX_OFFSET),
                (*this.as_ptr()).meta,
            )
        }
    }

    /// # Safety
    /// `this` must point to an initialized box.
    unsafe fn annex<'a>(this: NonNull<Self>) -> &'a T::Annex {
        unsafe { &*Self::annex_ptr(this) }
    }
}

unsafe impl<H: Upcast<Header>, T: Collect> Upcast<H> for BoxedSized<H, T> {}
unsafe impl<H: Upcast<Header>, T: Collect> Upcast<BoxedSized<H, T>> for BoxedSized<H, T> {}

/// A GC allocation whose payload is not yet initialized.
///
/// Creating an object takes two steps: allocate an embryo, then finish it with
/// the header and payload. Finishing never allocates, so a caller can allocate
/// first and then move GC references straight from a rooted location into the
/// payload with no possible collection in between. Until it is finished, an
/// embryo is not linked into the cyclic ring, traced, or counted in the
/// arena's allocation balance. Dropping an unfinished embryo only deallocates
/// it. If allocation ever triggers collection, the check belongs where an
/// embryo is created, not where it is finished.
///
/// An embryo never holds an initialized value outside a `finish*` call, except
/// the bytes of a byte-tail embryo, which need no drop.
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

impl<'v, H: Upcast<Header>, T: ?Sized + Boxable<H>> Embryo<'v, H, T> {
    /// Allocates `layout` uninitialized and records the arena where the
    /// header's arena pointer goes.
    unsafe fn alloc_raw(arena: &Arena<'v>, layout: Layout) -> Self {
        let Some(ptr) = NonNull::new(unsafe { alloc::alloc(layout) }) else {
            alloc::handle_alloc_error(layout)
        };
        unsafe {
            ptr.cast::<NonNull<arena::ArenaInner>>()
                .write(NonNull::from_ref(&arena.0));
        }
        Self {
            ptr,
            layout,
            phantom: PhantomData,
        }
    }

    /// Links the finished object into its arena.
    ///
    /// # Safety
    /// The header and payload must be fully initialized for `U`, and the
    /// allocation must have the layout `U`'s vtable deallocates with.
    unsafe fn link<U: Upcast<Header>>(self) -> Base<'v, U> {
        unsafe {
            let arena = self.arena_inner();
            let ptr = self.ptr.cast::<U>();
            let size = self.layout.size();
            mem::forget(self);
            Base::new_from_raw_inner(arena, ptr, size)
        }
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

impl<'v, H: Upcast<Header>, T: Collect + Boxable<H>> Embryo<'v, H, T>
where
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    /// Allocates a box for an annex described by `meta`, and records `meta`.
    unsafe fn new_meta(arena: &Arena<'v>, meta: Meta<T>) -> Self {
        unsafe {
            let this = Self::alloc_raw(arena, BoxedSized::<H, T>::layout(meta));
            (&raw mut (*this.boxed().as_ptr()).meta).write(meta);
            this
        }
    }

    fn boxed(&self) -> NonNull<BoxedSized<H, T>> {
        self.ptr.cast()
    }

    /// Writes the header and value and links the object. The annex must already
    /// be initialized.
    unsafe fn finish_head(self, header: H, value: T) -> Base<'v, Boxed<H, T>> {
        unsafe {
            debug_assert_eq!(self.arena_inner(), Self::arena_inner_from_header(&header));
            let boxed = self.boxed().as_ptr();
            (&raw mut (*boxed).value).write(UnsafeCell::new(value));
            (&raw mut (*boxed).header).write(header);
            self.link()
        }
    }
}

impl<'v, H: Upcast<Header>, T: Collect<Annex: Sized> + Boxable<H>> Embryo<'v, H, T>
where
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    /// # Safety
    /// See [`Embryo::finish_sized`].
    pub(crate) unsafe fn new_sized(arena: &Arena<'v>) -> Self {
        unsafe { Self::new_meta(arena, Default::default()) }
    }

    /// Initializes and links the object. Does not allocate.
    ///
    /// # Safety
    /// `header` must belong to the embryo's arena and carry `T`'s vtable.
    pub(crate) unsafe fn finish_sized(
        self,
        header: H,
        value: T,
        annex: T::Annex,
    ) -> Base<'v, Boxed<H, T>> {
        unsafe {
            BoxedSized::annex_ptr(self.boxed()).write(annex);
            self.finish_head(header, value)
        }
    }
}

impl<'v, H: Upcast<Header>, T, P, E> Embryo<'v, H, T>
where
    T: Collect<Annex = Tail<P, [E]>> + Boxable<H>,
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    /// Allocates an object whose annex tail holds exactly `len` elements.
    ///
    /// # Safety
    /// See [`Embryo::finish_tail`].
    pub(crate) unsafe fn new_tail(arena: &Arena<'v>, len: usize) -> Self {
        unsafe { Self::new_meta(arena, len) }
    }

    /// Initializes the annex prefix, the tail elements from `iter` in order, and
    /// the value, then links the object. Does not allocate, unless `iter` does.
    ///
    /// # Panics
    /// If `iter` yields fewer or more elements than the embryo was allocated
    /// for. The elements already written are dropped.
    ///
    /// # Safety
    /// `header` must belong to the embryo's arena and carry `T`'s vtable.
    pub(crate) unsafe fn finish_tail(
        self,
        header: H,
        value: T,
        prefix: P,
        iter: impl IntoIterator<Item = E>,
    ) -> Base<'v, Boxed<H, T>> {
        unsafe {
            let annex = BoxedSized::annex_ptr(self.boxed());
            write_elements(
                (&raw mut (*annex).tail).cast::<E>(),
                (*self.boxed().as_ptr()).meta,
                iter,
            );
            (&raw mut (*annex).prefix).write(prefix);
            self.finish_head(header, value)
        }
    }
}

/// Writes exactly `len` elements from `iter` to `values`.
///
/// # Panics
/// If `iter` yields fewer or more than `len` elements. The elements already
/// written are dropped.
///
/// # Safety
/// `values` must be valid for writing `len` elements.
unsafe fn write_elements<T>(values: *mut T, len: usize, iter: impl IntoIterator<Item = T>) {
    /// Drops the written prefix if writing the elements panics.
    struct Prefix<T> {
        values: *mut T,
        written: usize,
    }

    impl<T> Drop for Prefix<T> {
        fn drop(&mut self) {
            unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.values, self.written)) }
        }
    }

    let mut prefix = Prefix { values, written: 0 };
    let mut iter = iter.into_iter();
    while prefix.written < len {
        let Some(value) = iter.next() else {
            panic!("embryo finished with {} of {len} elements", prefix.written)
        };
        unsafe { prefix.values.add(prefix.written).write(value) };
        prefix.written += 1;
    }
    assert!(
        iter.next().is_none(),
        "embryo finished with more than {len} elements"
    );
    mem::forget(prefix);
}

impl<'v, H: Upcast<Header>, T> Embryo<'v, H, T>
where
    T: Collect<Annex = Tail<(), [u8]>> + Boxable<H>,
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    /// Allocates a growable byte tail with room for `cap` bytes and none
    /// initialized.
    pub(crate) unsafe fn from_arena_capacity(arena: &Arena<'v>, cap: usize) -> Self {
        unsafe {
            let this = Self::alloc_raw(arena, BoxedSized::<H, T>::layout(cap));
            this.len_ptr().write(0);
            this
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

    /// Shrinks the tail to its initialized bytes and links the object.
    ///
    /// # Safety
    /// `header` must belong to the embryo's arena and carry `T`'s vtable.
    pub(crate) unsafe fn finalize(mut self, header: H, value: T) -> Base<'v, Boxed<H, T>> {
        self.shrink_to_fit();
        unsafe { self.finish_head(header, value) }
    }

    /// Like [`Self::finalize`], but finishes the object as `S`, whose annex holds
    /// the bytes as a `str`.
    ///
    /// # Safety
    /// The initialized bytes must be valid UTF-8, and `header` must belong to the
    /// embryo's arena and carry `S`'s vtable.
    pub(crate) unsafe fn finalize_str_unchecked<S>(
        mut self,
        header: H,
        value: S,
    ) -> Base<'v, Boxed<H, S>>
    where
        S: Collect<Annex = Tail<(), str>> + Boxable<H>,
        Boxed<H, S>: Upcast<Header>,
        BoxedSized<H, S>: Upcast<Header>,
    {
        const {
            assert!(BoxedSized::<H, T>::ANNEX_OFFSET == BoxedSized::<H, S>::ANNEX_OFFSET);
            assert!(align_of::<BoxedSized<H, T>>() == align_of::<BoxedSized<H, S>>());
        }
        self.shrink_to_fit();
        let embryo = Embryo::<H, S> {
            ptr: self.ptr,
            layout: self.layout,
            phantom: PhantomData,
        };
        mem::forget(self);
        unsafe { embryo.finish_head(header, value) }
    }

    pub(crate) fn data_ptr(&self) -> *mut u8 {
        unsafe { self.ptr.as_ptr().add(BoxedSized::<H, T>::ANNEX_OFFSET) }
    }

    /// Replaces `self[start..end]` with `replacement`, shifting the remaining tail left or
    /// right within the existing allocation as needed. No allocation is performed here; the
    /// caller must have already reserved enough spare capacity to cover any growth
    /// (`replacement.len().saturating_sub(end - start)`).
    ///
    /// # Safety
    /// `start <= end <= self.len()`, and spare capacity must be at least
    /// `replacement.len().saturating_sub(end - start)`.
    pub(crate) unsafe fn splice(&mut self, start: usize, end: usize, replacement: &[u8]) {
        debug_assert!(start <= end && end <= self.len());
        let old_len = self.len();
        let tail_len = old_len - end;
        let new_tail_start = start + replacement.len();
        unsafe {
            let ptr = self.data_ptr();
            ptr::copy(ptr.add(end), ptr.add(new_tail_start), tail_len);
            ptr::copy_nonoverlapping(replacement.as_ptr(), ptr.add(start), replacement.len());
            self.set_len(new_tail_start + tail_len);
        }
    }

    fn realloc(&mut self, new_cap: usize) {
        let old_layout = self.layout;
        let new_layout = BoxedSized::<H, T>::layout(new_cap);
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

    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        unsafe { self.len_ptr().write(len) }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.layout.size() - BoxedSized::<H, T>::ANNEX_OFFSET
    }

    /// Until the embryo is finished, `meta` records the initialized length.
    fn len_ptr(&self) -> *mut usize {
        unsafe { &raw mut (*self.boxed().as_ptr()).meta }
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

impl<H: Upcast<Header>, T: Sized + Collect> Boxable<H> for T
where
    BoxedSized<H, T>: Upcast<Header>,
{
    type Inner = BoxedSized<H, T>;
    const VTBL: &Vtbl = &Vtbl {
        cyclic: T::CYCLIC,
        is_strand: T::STRAND,
        name: || type_name::<T>(),
        drop: |this| unsafe {
            T::finalize(this);
            let boxed = this.cast::<BoxedSized<H, T>>();
            let annex = BoxedSized::annex_ptr(boxed);
            ptr::drop_in_place(boxed.as_ptr());
            ptr::drop_in_place(annex);
        },
        dealloc: |this| unsafe {
            // `meta` is `Copy`, so it is still readable after `drop`
            let layout =
                BoxedSized::<H, T>::layout((*this.cast::<BoxedSized<H, T>>().as_ptr()).meta);
            alloc::dealloc(this.as_ptr() as *mut _, layout);
            layout.size()
        },
        trace: |this, visit| unsafe {
            let boxed = this.cast::<BoxedSized<H, T>>();
            (*boxed.as_ref().value.get()).accept(visit)?;
            BoxedSized::annex(boxed).accept(visit)
        },
        clear: |this| unsafe {
            let boxed = this.cast::<BoxedSized<H, T>>();
            T::finalize(this);
            (*boxed.as_ref().value.get()).clear();
            BoxedSized::annex(boxed).clear()
        },
    };

    unsafe fn deref_inner(inner: NonNull<<Self as Boxable<H>>::Inner>) -> NonNull<Self> {
        unsafe { NonNull::new_unchecked((*inner.as_ptr()).value.get()) }
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

impl<'v, H: Upcast<Header>, T: Collect<Annex: Sized> + Boxable<Header>> Base<'v, Boxed<H, T>>
where
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    pub(crate) unsafe fn from_parts(arena: &Arena<'v>, header: H, value: T) -> Self
    where
        T::Annex: Default,
    {
        unsafe { Self::from_parts_annex(arena, header, value, Default::default()) }
    }

    pub(crate) unsafe fn from_parts_annex(
        arena: &Arena<'v>,
        header: H,
        value: T,
        annex: T::Annex,
    ) -> Self {
        unsafe { Embryo::<H, T>::new_sized(arena).finish_sized(header, value, annex) }
    }
}

impl<'v, H: Upcast<Header>, T> Base<'v, Boxed<H, T>>
where
    T: Collect<Annex = Tail<(), [u8]>> + Boxable<H>,
    Boxed<H, T>: Upcast<Header>,
    BoxedSized<H, T>: Upcast<Header>,
{
    /// Creates an object whose byte tail is a copy of `bytes`.
    ///
    /// # Safety
    /// `header` must belong to `arena` and carry `T`'s vtable.
    pub(crate) unsafe fn from_bytes(arena: &Arena<'v>, header: H, value: T, bytes: &[u8]) -> Self {
        unsafe {
            let mut embryo = Embryo::<H, T>::from_arena_capacity(arena, bytes.len());
            embryo.extend(bytes);
            embryo.finalize(header, value)
        }
    }
}

impl<'v, H: Upcast<Header>, S> Base<'v, Boxed<H, S>>
where
    S: Collect<Annex = Tail<(), str>> + Boxable<H>,
    Boxed<H, S>: Upcast<Header>,
    BoxedSized<H, S>: Upcast<Header>,
{
    /// Creates an object whose `str` tail is a copy of `value`.
    ///
    /// # Safety
    /// `header` must belong to `arena` and carry `S`'s vtable.
    pub(crate) unsafe fn from_str(arena: &Arena<'v>, header: H, value: S, str: &str) -> Self {
        unsafe {
            let embryo = Embryo::<H, S>::new_meta(arena, str.len());
            ptr::copy_nonoverlapping(
                str.as_ptr(),
                embryo.ptr.as_ptr().add(BoxedSized::<H, S>::ANNEX_OFFSET),
                str.len(),
            );
            embryo.finish_head(header, value)
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

pub(crate) type BoxWeak<'v, H, T> = BaseWeak<'v, Boxed<H, T>>;

pub(crate) type Gc<'v, T> = Box<'v, Header, T>;

impl<'v, T: Collect<Annex: Sized> + Boxable<Header>> Gc<'v, T> {
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

    pub(crate) fn new_with_annex(arena: &Arena<'v>, value: T, annex: T::Annex) -> Self {
        unsafe {
            Gc::from_parts_annex(
                arena,
                Header::new(arena, NonNull::from_ref(<T as Boxable<Header>>::VTBL)),
                value,
                annex,
            )
        }
    }
}

impl<'v, T, P, E> Gc<'v, T>
where
    T: Collect<Annex = Tail<P, [E]>> + Boxable<Header>,
    Boxed<Header, T>: Upcast<Header>,
    BoxedSized<Header, T>: Upcast<Header>,
{
    /// Creates an object whose annex holds `prefix` and the elements of `iter`.
    #[cfg(test)]
    pub(crate) fn new_tail(
        arena: &Arena<'v>,
        value: T,
        prefix: P,
        iter: impl IntoIterator<Item = E, IntoIter: ExactSizeIterator>,
    ) -> Self {
        let iter = iter.into_iter();
        unsafe {
            Embryo::<Header, T>::new_tail(arena, iter.len()).finish_tail(
                Header::new(arena, NonNull::from_ref(<T as Boxable<Header>>::VTBL)),
                value,
                prefix,
                iter,
            )
        }
    }
}

impl<'v, H: Upcast<Header>, T, A: ?Sized + Annex> Box<'v, H, T>
where
    T: Boxable<H, Inner = BoxedSized<H, T>> + Collect<Annex = alias::Box<A>>,
    Boxed<H, T>: Upcast<Header>,
{
    /// The contents of an annex stored out of line, which need no pin to reach.
    pub(crate) fn annex(&self) -> &A {
        self.project(|p| p.annex())
    }
}

unsafe impl<T: ImmState> ImmState for alias::Box<T> {
    const CYCLIC: bool = T::CYCLIC;
}

impl<'v, H: Upcast<Header>, T: ImmState> Box<'v, H, Imm<alias::Box<T>>>
where
    Boxed<H, Imm<alias::Box<T>>>: Upcast<Header>,
    BoxedSized<H, Imm<alias::Box<T>>>: Upcast<Header>,
{
    /// The state of an immutable object stored out of line, which needs no pin to
    /// reach.
    pub(crate) fn get(&self) -> &T {
        self.project(|p| p.get())
    }
}

impl<A: ?Sized + Annex> Annex for alias::Box<A> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        (**self).accept(visit)
    }

    fn clear(&self) {
        (**self).clear()
    }
}

pub(crate) type Weak<'v, T> = BoxWeak<'v, Header, T>;

pub(crate) type Pin<'v, 'p, H, T> = BasePin<'v, 'p, Boxed<H, T>>;

impl<'v, 'p, H: Upcast<Header>, T: Boxable<H, Inner = BoxedSized<H, T>> + Collect>
    Pin<'v, 'p, H, T>
{
    pub(crate) fn annex(&self) -> &'p T::Annex
    where
        T: Collect,
    {
        unsafe { BoxedSized::annex(self.base.cast::<BoxedSized<H, T>>()) }
    }
}

impl<'v, 'p, H: Upcast<Header>, T: ?Sized + Boxable<H>> Pin<'v, 'p, H, T> {
    /// # Safety
    /// As for [`BasePin::new_unchecked`].
    pub(crate) unsafe fn from_raw(ptr: NonNull<<T as Boxable<H>>::Inner>) -> Self {
        Self {
            base: ptr.cast(),
            phantom: PhantomData,
        }
    }

    pub(crate) fn as_header(self) -> NonNull<H> {
        self.base.cast()
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
        unsafe { BoxedSized::annex(this.ptr.cast::<BoxedSized<H, T>>()) }
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
        unsafe { BoxedSized::annex(this.ptr.cast::<BoxedSized<H, T>>()) }
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

impl<'v, 'p, H: Upcast<Header>, T: ImmState> Pin<'v, 'p, H, Imm<T>>
where
    BoxedSized<H, Imm<T>>: Upcast<Header>,
{
    /// The object's state.
    pub(crate) fn get(&self) -> &'p T {
        self.annex()
    }
}

impl<'v, H: Upcast<Header>, T: ImmState> BoxWeak<'v, H, Imm<T>>
where
    Boxed<H, Imm<T>>: Upcast<Header>,
    BoxedSized<H, Imm<T>>: Upcast<Header>,
{
    /// The object's state.
    ///
    /// # Safety
    /// The object must still be alive, and no collection may happen before the
    /// result's last use.
    pub(crate) unsafe fn get_unchecked(&self) -> &T {
        debug_assert_ne!(self.strong_count(), 0);
        unsafe { BoxedSized::annex(self.ptr.cast::<BoxedSized<H, Imm<T>>>()) }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        panic::{self, AssertUnwindSafe},
        rc::Rc,
    };

    use super::*;

    /// Counts its drops; `id` checks element order.
    struct Counted {
        id: usize,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.drops.update(|n| n + 1);
        }
    }

    // SAFETY: holds no GC references.
    unsafe impl Collect for Counted {
        const CYCLIC: bool = false;
        type Annex = ();

        fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&mut self) {}
    }

    /// An immutable object whose children sit behind interior mutability, so
    /// clearing must release them.
    struct Node<'v> {
        children: RefCell<Vec<Gc<'v, Imm<Node<'v>>>>>,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for Node<'_> {
        fn drop(&mut self) {
            self.drops.update(|n| n + 1);
        }
    }

    // SAFETY: accept visits every child.
    unsafe impl ImmState for Node<'_> {
        const CYCLIC: bool = true;
    }

    impl Annex for Node<'_> {
        fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
            for child in &*self.children.borrow() {
                child.accept(visit)?;
            }
            ControlFlow::Continue(())
        }

        fn clear(&self) {
            self.children.take();
        }
    }

    fn header<'v, T: ?Sized + Boxable<Header>>(arena: &Arena<'v>) -> Header {
        unsafe { Header::new(arena, NonNull::from_ref(<T as Boxable<Header>>::VTBL)) }
    }

    fn counted(drops: &Rc<Cell<usize>>, ids: std::ops::Range<usize>) -> Vec<Counted> {
        ids.map(|id| Counted {
            id,
            drops: drops.clone(),
        })
        .collect()
    }

    #[test]
    fn unfinished_embryos_are_not_counted() {
        let arena = Arena::new();
        drop(unsafe { Embryo::<Header, Counted>::new_sized(&arena) });
        drop(unsafe { Embryo::<Header, Tailed>::new_tail(&arena, 3) });
        assert_eq!(arena.0.balance.get(), 0);
        assert_eq!(arena.allocated(), 0);
    }

    #[test]
    fn finished_sized_embryo_is_linked_and_collectable() {
        let arena = Arena::new();
        let drops = Rc::new(Cell::new(0));
        let embryo = unsafe { Embryo::<Header, Imm<Node>>::new_sized(&arena) };
        let node = Node {
            children: RefCell::new(Vec::new()),
            drops: drops.clone(),
        };
        let node: Gc<Imm<Node>> =
            unsafe { embryo.finish_sized(header::<Imm<Node>>(&arena), Imm::UNIT, node) };
        assert_eq!(arena.0.balance.get(), 1);
        assert_ne!(arena.allocated(), 0);
        unsafe { node.pin_unchecked() }
            .get()
            .children
            .borrow_mut()
            .push(unsafe { node.dup() });
        drop(node);
        assert_eq!(drops.get(), 0);
        arena.collect_full();
        assert_eq!(drops.get(), 1);
    }

    /// An object whose annex is a counted prefix and a tail of counted elements.
    struct Tailed;

    // SAFETY: holds no GC references.
    unsafe impl Collect for Tailed {
        const CYCLIC: bool = false;
        type Annex = Tail<Counted, [Counted]>;

        fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&mut self) {}
    }

    impl TailAnnex<Counted> for Counted {
        fn accept(&self, _tail: &[Counted], _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&self, _tail: &[Counted]) {}
    }

    #[test]
    fn tail_embryo_finishes_in_order() {
        let arena = Arena::new();
        let drops = Rc::new(Cell::new(0));
        for len in [0, 3] {
            let embryo = unsafe { Embryo::<Header, Tailed>::new_tail(&arena, len) };
            let prefix = Counted {
                id: 99,
                drops: drops.clone(),
            };
            let obj: Gc<Tailed> = unsafe {
                embryo.finish_tail(
                    header::<Tailed>(&arena),
                    Tailed,
                    prefix,
                    counted(&drops, 0..len),
                )
            };
            assert_eq!(unsafe { obj.pin_unchecked() }.annex().prefix.id, 99);
            let ids: Vec<_> = unsafe { obj.pin_unchecked() }
                .annex()
                .tail
                .iter()
                .map(|c| c.id)
                .collect();
            assert_eq!(ids, (0..len).collect::<Vec<_>>());
            drops.set(0);
            drop(obj);
            assert_eq!(drops.get(), len + 1);
        }
        assert_eq!(arena.0.balance.get(), 0);
        assert_eq!(arena.allocated(), 0);
    }

    #[test]
    fn tail_embryo_length_mismatch_drops_written_prefix() {
        let arena = Arena::new();
        for (len, given) in [(3, 2), (2, 3)] {
            let drops = Rc::new(Cell::new(0));
            let embryo = unsafe { Embryo::<Header, Tailed>::new_tail(&arena, len) };
            let prefix = Counted {
                id: 99,
                drops: drops.clone(),
            };
            let values = counted(&drops, 0..given);
            let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                embryo.finish_tail(header::<Tailed>(&arena), Tailed, prefix, values)
            }));
            assert!(result.is_err());
            // The written elements, the rest of the iterator and the prefix
            assert_eq!(drops.get(), given + 1, "len {len}, given {given}");
        }
        assert_eq!(arena.0.balance.get(), 0);
    }

    /// A cyclic object whose references live in its annex tail.
    struct Ring<'v>(PhantomData<Gc<'v, Ring<'v>>>);

    struct RingPrefix {
        drops: Rc<Cell<usize>>,
    }

    impl Drop for RingPrefix {
        fn drop(&mut self) {
            self.drops.update(|n| n + 1);
        }
    }

    type RingLink<'v> = RefCell<Option<Gc<'v, Ring<'v>>>>;

    // SAFETY: the annex visits and clears every link.
    unsafe impl<'v> Collect for Ring<'v> {
        const CYCLIC: bool = true;
        type Annex = Tail<RingPrefix, [RingLink<'v>]>;

        fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&mut self) {}
    }

    impl<'v> TailAnnex<RingLink<'v>> for RingPrefix {
        fn accept(&self, tail: &[RingLink<'v>], visit: &mut dyn Visit) -> ControlFlow<()> {
            for link in tail {
                if let Some(node) = &*link.borrow() {
                    node.accept(visit)?;
                }
            }
            ControlFlow::Continue(())
        }

        fn clear(&self, tail: &[RingLink<'v>]) {
            for link in tail {
                link.take();
            }
        }
    }

    #[test]
    fn tail_cycle_is_collected() {
        let arena = Arena::new();
        let drops = Rc::new(Cell::new(0));
        let ring = |drops: &Rc<Cell<usize>>| {
            Gc::new_tail(
                &arena,
                Ring(PhantomData),
                RingPrefix {
                    drops: drops.clone(),
                },
                [RefCell::new(None), RefCell::new(None)],
            )
        };
        let a = ring(&drops);
        let b = ring(&drops);
        *unsafe { a.pin_unchecked() }.annex().tail[0].borrow_mut() = Some(unsafe { b.dup() });
        *unsafe { b.pin_unchecked() }.annex().tail[1].borrow_mut() = Some(unsafe { a.dup() });
        drop((a, b));
        assert_eq!(drops.get(), 0);
        arena.collect_full();
        assert_eq!(drops.get(), 2);
        assert_eq!(arena.0.balance.get(), 0);
    }

    struct Bytes;

    // SAFETY: holds no GC references.
    unsafe impl Collect for Bytes {
        const CYCLIC: bool = false;
        type Annex = Tail<(), [u8]>;

        fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&mut self) {}
    }

    struct Chars;

    // SAFETY: holds no GC references.
    unsafe impl Collect for Chars {
        const CYCLIC: bool = false;
        type Annex = Tail<(), str>;

        fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
            ControlFlow::Continue(())
        }

        fn clear(&mut self) {}
    }

    #[test]
    fn byte_embryo_grows_and_finishes() {
        let arena = Arena::new();
        let mut embryo = unsafe { Embryo::<Header, Bytes>::from_arena_capacity(&arena, 2) };
        for chunk in [&b"hello"[..], b", ", b"world"] {
            embryo.reserve(chunk.len());
            unsafe { embryo.extend(chunk) };
        }
        unsafe { embryo.splice(0, 5, b"goodbye") };
        assert_eq!(embryo.as_slice(), b"goodbye, world");
        let bytes: Gc<Bytes> = unsafe { embryo.finalize(header::<Bytes>(&arena), Bytes) };
        assert_eq!(
            &unsafe { bytes.pin_unchecked() }.annex().tail,
            b"goodbye, world"
        );

        let mut embryo = unsafe { Embryo::<Header, Bytes>::from_arena_capacity(&arena, 16) };
        unsafe { embryo.extend("héllo".as_bytes()) };
        let chars: Gc<Chars> =
            unsafe { embryo.finalize_str_unchecked(header::<Chars>(&arena), Chars) };
        assert_eq!(&unsafe { chars.pin_unchecked() }.annex().tail, "héllo");

        let empty: Gc<Chars> = unsafe { Gc::from_str(&arena, header::<Chars>(&arena), Chars, "") };
        assert_eq!(&unsafe { empty.pin_unchecked() }.annex().tail, "");
        let copied: Gc<Bytes> =
            unsafe { Gc::from_bytes(&arena, header::<Bytes>(&arena), Bytes, b"abc") };
        assert_eq!(&unsafe { copied.pin_unchecked() }.annex().tail, b"abc");

        drop((bytes, chars, empty, copied));
        assert_eq!(arena.0.balance.get(), 0);
        assert_eq!(arena.allocated(), 0);
    }
}
