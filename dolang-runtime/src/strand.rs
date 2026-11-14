use std::{
    borrow::Cow,
    cell::{Cell, OnceCell, RefCell},
    future::Future,
    marker::PhantomData,
    ops::{ControlFlow, Deref, DerefMut},
    pin::Pin,
    ptr::{self, NonNull, null},
    rc::{Rc, Weak},
    task::{self, Poll, Waker},
};

use dolang_util::{alias, pin::Arena, ring, ring::Link};

bitflags::bitflags! {
    /// Interrupt causes blocked by a strand scope or token boundary.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct InterruptMask: u8 {
        /// Block cancellation.
        const CANCELED = 1 << 0;
        /// Block timeout.
        const TIMED_OUT = 1 << 1;
    }
}

use crate::{
    error::{BacktraceIter, Error, ErrorSlot, ErrorTable, Result, UnwindEntry},
    frame::{self, FrameIter, Native},
    gc::arena::Visit,
    method,
    object::{
        BuiltinTypes, Singletons, backtrace,
        protocol::GcObj,
        range,
        strand::{Completion, Handle},
        tuple,
    },
    sym::{self, Sym},
    value::{Input, Output, Slot, Value, ValueCell},
    vm::{Alloc, Vm},
};

/// Run a block with scratch [`Slot`](crate::value::Slot)s rooted for its duration.
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{with_slots, strand::Strand, value::{Output, Slot}};
/// # async fn example<'v, 's>(strand: &mut Strand<'v, 's>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// with_slots!(strand, |mut a, mut b| {
///     Output::set(strand, Slot::reborrow(&mut a), 1i64);
///     Output::set(strand, Slot::reborrow(&mut b), 2i64);
///     Ok(())
/// })
/// # }
/// ```
///
/// The closure-like parameters bind the slots, and their count gives the number of
/// slots. `strand` must be the name of the current [`Strand`]; the body sees the same name
/// rebound to a handle on the same strand whose frame pointer is the slots' frame. The
/// macro evaluates to the body's value.
///
/// The body runs inline in the enclosing function, so `.await` works in it, and `?` and
/// `return` leave the enclosing function, not just the body. The slots stay rooted
/// until the body finishes or the enclosing future is dropped. To inspect the body's
/// result instead of propagating it, wrap the body in `async { … }.await`.
#[macro_export]
macro_rules! with_slots {
    ($strand:ident, |$($slot:pat_param),+ $(,)?| $body:expr) => {{
        let frame = $crate::strand::SlotFrame::new();
        // Safety: the scope is a local of this block that nothing can move or forget
        #[allow(unused_unsafe)]
        let (mut scope, [$($slot),+]) = unsafe { $strand.enter_slot_frame(&frame) };
        let $strand = &mut *scope;
        $body
    }};
}

/// Run a block with the strand's interrupt mask replaced.
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{with_interrupt_mask, strand::{InterruptMask, Strand}};
/// # async fn example<'v, 's>(strand: &mut Strand<'v, 's>) {
/// with_interrupt_mask!(strand, InterruptMask::all(), {
///     // Cleanup that must finish even if the strand is interrupted
/// })
/// # }
/// ```
///
/// The body runs inline, as for [`with_slots!`](crate::with_slots). The mask is restored
/// when the body finishes, including when it leaves by `?` or `return`, or the enclosing
/// future is dropped.
///
/// # Interrupt Semantics
///
/// By default, when a strand is interrupted, all Do-related futures will return
/// a `Canceled` or `TimedOut` error on the next poll, and the deepest future
/// in the stack may be dropped entirely. This is the normal interruption behavior.
///
/// Causes present in `mask` are temporarily suppressed:
/// - Do-related futures will continue to work normally
/// - Futures won't be dropped on the floor
/// - The strand won't see interrupt errors
///
/// # Use Cases
///
/// Interrupt masking is intended for cleanup paths where you must perform
/// certain operations even when the strand is being interrupted:
/// - Deleting temporary files
/// - Closing file handles
/// - Rolling back database transactions
/// - Releasing locks
///
/// # Important Warning
///
/// Interrupted strands are expected to unwind in a timely manner. Do NOT use
/// interrupt masking to perform arbitrary long-running operations.
/// Keep masked operations short and focused on cleanup.
#[macro_export]
macro_rules! with_interrupt_mask {
    ($strand:ident, $mask:expr, $body:expr) => {{
        let _guard = $strand.interrupt_mask_guard($mask);
        $body
    }};
}

/// Run a block with `interrupt` as the strand's selected interrupt token.
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{with_interrupt_token, strand::{InterruptMask, Strand}};
/// # async fn example<'v, 's>(strand: &mut Strand<'v, 's>) {
/// let interrupt = strand.interrupt_token().nested(InterruptMask::empty());
/// with_interrupt_token!(strand, interrupt, {
///     // Suspensions here are interrupted through `interrupt`
/// })
/// # }
/// ```
///
/// The body runs inline, as for [`with_slots!`](crate::with_slots). While it runs, interrupt
/// checks and the futures it suspends on use `interrupt`. The prior token is restored when
/// the body finishes, including when it leaves by `?` or `return`, or the enclosing future
/// is dropped.
#[macro_export]
macro_rules! with_interrupt_token {
    ($strand:ident, $interrupt:expr, $body:expr) => {{
        let _guard = $strand.interrupt_token_guard($interrupt);
        $body
    }};
}

/// Anchor bounding a pin minted by [`with_instance!`](crate::with_instance),
/// [`with_str!`](crate::with_str), [`with_slice!`](crate::with_slice), or the crate's
/// internal pinning macros to a block.
#[doc(hidden)]
pub struct PinScope;

/// Scratch slots for [`with_slots!`](crate::with_slots), rooted while entered.
#[doc(hidden)]
pub struct SlotFrame<'v, const N: usize> {
    values: [ValueCell<'v>; N],
    scan: OnceCell<frame::Scan<'v>>,
}

impl<'v, const N: usize> SlotFrame<'v, N> {
    #[inline]
    pub fn new() -> Self {
        Self {
            values: std::array::from_fn(|_| ValueCell::default()),
            scan: OnceCell::new(),
        }
    }
}

impl<'v, const N: usize> Default for SlotFrame<'v, N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// A [`Strand`] handle whose frame pointer is an entered [`SlotFrame`], which pops the frame
/// when dropped.
#[doc(hidden)]
pub struct SlotScope<'v, 's, 'f> {
    strand: Strand<'v, 's>,
    prior: Option<frame::Ptr<'v>>,
    _borrow: PhantomData<&'f mut Strand<'v, 's>>,
}

impl<'v, 's> Deref for SlotScope<'v, 's, '_> {
    type Target = Strand<'v, 's>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.strand
    }
}

impl<'v, 's> DerefMut for SlotScope<'v, 's, '_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.strand
    }
}

impl Drop for SlotScope<'_, '_, '_> {
    #[inline]
    fn drop(&mut self) {
        self.strand.inner.sp.set(self.prior);
    }
}

/// Restores a strand's interrupt mask when dropped; see
/// [`with_interrupt_mask!`](crate::with_interrupt_mask).
#[doc(hidden)]
#[must_use]
pub struct InterruptMaskGuard<'v, 's> {
    inner: &'s StrandInner<'v>,
    orig: InterruptMask,
}

impl Drop for InterruptMaskGuard<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        self.inner.interrupt_mask.set(self.orig);
    }
}

/// Restores a strand's interrupt token when dropped; see
/// [`with_interrupt_token!`](crate::with_interrupt_token).
#[doc(hidden)]
#[must_use]
pub struct InterruptTokenGuard<'v, 's> {
    inner: &'s StrandInner<'v>,
    orig: InterruptToken<'v>,
}

impl Drop for InterruptTokenGuard<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        std::mem::swap(&mut *self.inner.interrupt.borrow_mut(), &mut self.orig);
        self.inner.interrupt_registered.set(false);
    }
}

/// Scratch slots for [`with_slots_dynamic!`], sized at run time and rooted while entered.
pub(crate) struct DynSlotFrame<'v> {
    values: Vec<ValueCell<'v>>,
    scan: OnceCell<frame::Scan<'v>>,
}

impl<'v> DynSlotFrame<'v> {
    #[inline]
    pub(crate) fn new(len: usize) -> Self {
        Self {
            values: (0..len).map(|_| ValueCell::default()).collect(),
            scan: OnceCell::new(),
        }
    }
}

/// Run a block with a runtime-sized scratch [`Slots`](crate::value::Slots) allocation.
///
/// Like [`with_slots!`](crate::with_slots), the body runs inline with `strand` rebound to a
/// handle whose frame roots the slots: `?` and `return` leave the enclosing function.
macro_rules! with_slots_dynamic {
    ($strand:ident, $len:expr, |$slots:pat_param| $body:expr) => {{
        let frame = $crate::strand::DynSlotFrame::new($len);
        // Safety: the scope is a local of this block that nothing can move or forget
        #[allow(unused_unsafe)]
        let (mut scope, $slots) = unsafe { $strand.enter_dyn_slot_frame(&frame) };
        #[allow(unused_variables)]
        let $strand = &mut *scope;
        $body
    }};
}
pub(crate) use with_slots_dynamic;

/// The frame a [`FrameScope`] entered.
enum FrameEntry<'v, 'f> {
    Do(&'f frame::CallFrame<'v>),
    Native(&'f Native<'v>),
}

/// A [`Strand`] handle whose frame pointer is an entered Do or native frame, one call level
/// deeper.  Dropping it pops the frame and the call level.
pub(crate) struct FrameScope<'v, 's, 'f> {
    strand: Strand<'v, 's>,
    prior: Option<frame::Ptr<'v>>,
    entry: FrameEntry<'v, 'f>,
    _borrow: PhantomData<&'f mut Strand<'v, 's>>,
}

impl<'v, 's> FrameScope<'v, 's, '_> {
    /// Leave the frame, adding its backtrace entry to `res`'s error.
    ///
    /// A Do frame's entry records its pc as of this call.
    #[inline]
    pub(crate) fn finish<R>(self, res: Result<'v, 's, R>) -> Result<'v, 's, R> {
        res.map_err(|mut e| {
            let entry = match self.entry {
                FrameEntry::Do(frame) => UnwindEntry::Do {
                    program: frame.program.clone(),
                    function_index: frame.func as u32,
                    pc: frame.pc.get() as u32,
                },
                FrameEntry::Native(frame) => UnwindEntry::Native {
                    module: frame.module.clone(),
                    receiver: frame.receiver.clone(),
                    method: frame.method.clone(),
                },
            };
            e.push_backtrace(self.strand.inner, entry);
            e
        })
    }
}

impl<'v, 's> Deref for FrameScope<'v, 's, '_> {
    type Target = Strand<'v, 's>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.strand
    }
}

impl<'v, 's> DerefMut for FrameScope<'v, 's, '_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.strand
    }
}

impl Drop for FrameScope<'_, '_, '_> {
    #[inline]
    fn drop(&mut self) {
        let inner = self.strand.inner;
        inner.sp.set(self.prior);
        inner.call_depth.update(|depth| depth - 1);
    }
}

/// Run an expression in a native frame for `module`, `receiver` and `method`, one call level
/// deeper.
///
/// The expression runs inline with `strand` rebound to a handle on the native frame.  An
/// error it evaluates to gets the frame's backtrace entry; an error leaving through `?` or
/// `return` does not, so wrap such a body in `async { … }.await`.  Exceeding the call
/// depth returns the error from the enclosing function.
macro_rules! in_native_frame {
    ($strand:ident, $module:expr, $receiver:expr, $method:expr, $body:expr $(,)?) => {{
        let frame = $crate::frame::Native {
            module: $module,
            receiver: $receiver,
            method: $method,
            parent: Some($strand.fp),
        };
        // Safety: the scope is a local of this block that nothing can move or forget
        #[allow(unused_unsafe)]
        let mut scope = unsafe { $strand.enter_native_frame(&frame) }?;
        let $strand = &mut *scope;
        let res = $body;
        scope.finish(res)
    }};
}
pub(crate) use in_native_frame;

pub(crate) type InterruptId = u64;

struct InterruptInner {
    canceled: Cell<bool>,
    timed_out: Cell<bool>,
    next_id: Cell<InterruptId>,
    wakers: RefCell<Vec<(InterruptId, Waker, InterruptMask)>>,
    children: RefCell<Vec<(Weak<InterruptInner>, InterruptMask)>>,
}

impl InterruptInner {
    fn cancel(&self) {
        self.canceled.set(true);
        self.wake(InterruptMask::CANCELED);
    }

    fn timeout(&self) {
        self.timed_out.set(true);
        self.wake(InterruptMask::TIMED_OUT);
    }

    fn wake(&self, cause: InterruptMask) {
        self.wakers.borrow_mut().retain(|(_, waker, mask)| {
            if mask.contains(cause) {
                true
            } else {
                waker.wake_by_ref();
                false
            }
        });
        self.children.borrow_mut().retain(|(child, filter)| {
            let Some(child) = child.upgrade() else {
                return false;
            };
            if !filter.contains(cause) {
                if cause == InterruptMask::CANCELED {
                    child.cancel()
                } else {
                    child.timeout()
                }
            }
            true
        });
    }

    fn pending(&self, mask: InterruptMask) -> bool {
        (self.canceled.get() && !mask.contains(InterruptMask::CANCELED))
            || (self.timed_out.get() && !mask.contains(InterruptMask::TIMED_OUT))
    }
}

/// Interrupt token.
///
/// Interrupt tokens permit interrupting one or more strands, causing them to abort with
/// [`ErrorKind::Canceled`](crate::error::ErrorKind::Canceled) or
/// [`ErrorKind::TimedOut`](crate::error::ErrorKind::TimedOut) the next time they would
/// suspend. Every strand has an interrupt token set when it is created.
#[derive(Clone)]
pub struct InterruptToken<'v> {
    inner: Rc<InterruptInner>,
    phantom: PhantomData<&'v mut &'v ()>,
}

impl<'v> InterruptToken<'v> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Rc::new(InterruptInner {
                canceled: Cell::new(false),
                timed_out: Cell::new(false),
                next_id: Cell::new(0),
                wakers: Default::default(),
                children: Default::default(),
            }),
            phantom: PhantomData,
        }
    }

    pub(crate) fn register(&self, waker: &Waker, mask: InterruptMask) -> Option<InterruptId> {
        if self.inner.pending(mask) {
            return None;
        }
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id.strict_add(1));
        self.inner
            .wakers
            .borrow_mut()
            .push((id, waker.clone(), mask));
        Some(id)
    }

    pub(crate) fn unregister(&self, id: InterruptId) {
        self.inner.wakers.borrow_mut().retain(|(i, _, _)| *i != id);
    }

    /// Creates a nested interrupt token, blocking inherited causes in `filter`.
    /// Direct interruption of the returned token is not filtered.
    pub fn nested(&self, filter: InterruptMask) -> Self {
        let child = Self::new();
        if self.inner.canceled.get() && !filter.contains(InterruptMask::CANCELED) {
            child.inner.canceled.set(true);
        }
        if self.inner.timed_out.get() && !filter.contains(InterruptMask::TIMED_OUT) {
            child.inner.timed_out.set(true);
        }
        let mut children = self.inner.children.borrow_mut();
        // Prune dead children before the vector grows, so a long-lived token
        // that nests many short-lived ones stays bounded.
        if children.len() == children.capacity() {
            children.retain(|(child, _)| child.strong_count() != 0);
        }
        children.push((Rc::downgrade(&child.inner), filter));
        drop(children);
        child
    }

    /// Cancel all associated strands and nested interrupt tokens.
    /// The canceled state is permanent.
    pub fn cancel(&self) {
        self.inner.cancel()
    }

    /// Time out all associated strands and nested interrupt tokens.
    /// The timed out state is permanent.
    pub fn timeout(&self) {
        self.inner.timeout()
    }

    /// Is this interrupt token canceled?
    pub fn is_canceled(&self) -> bool {
        self.inner.canceled.get()
    }

    /// Is this interrupt token timed out?
    pub fn is_timed_out(&self) -> bool {
        self.inner.timed_out.get()
    }
}

/// Kind of strand receiving inherited local state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InheritKind {
    /// A child strand whose lifetime is scoped to its parent.
    Scoped,
    /// A detached strand managed by the VM event loop.
    Background,
}

/// Strand-local state.
pub trait Local<'v>: 'v {
    /// Initialize state for main strand.
    fn init() -> Self;
    /// Create inherited state for new strand from current strand.
    fn inherit(&self, strand: &Strand<'v, '_>, kind: InheritKind) -> Self;
}

pub(crate) struct LocalVtbl<'v> {
    drop: unsafe fn(NonNull<()>),
    init: fn() -> NonNull<()>,
    inherit:
        unsafe fn(this: NonNull<()>, strand: &Strand<'v, '_>, kind: InheritKind) -> NonNull<()>,
}

impl<'v> LocalVtbl<'v> {
    pub(crate) fn new<T: Local<'v>>() -> Self {
        Self {
            drop: |ptr| {
                let _ = unsafe { alias::Box::from_non_null(ptr.cast::<T>()) };
            },
            init: || alias::Box::into_non_null(alias::Box::new(T::init())).cast(),
            inherit: |this, strand, kind| unsafe {
                let this: &T = this.cast().as_ref();
                NonNull::new_unchecked(Box::into_raw(Box::new(this.inherit(strand, kind)))).cast()
            },
        }
    }
}

/// Key for accessing strand-local state
pub struct LocalKey<'v, T> {
    index: usize,
    phantom: PhantomData<(*mut T, &'v mut &'v ())>,
}

impl<T> Clone for LocalKey<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for LocalKey<'_, T> {}

impl<'v, T> LocalKey<'v, T> {
    // SAFETY: index must match position of associated vtbl in VmInner
    pub(crate) unsafe fn new(index: usize) -> Self {
        Self {
            index,
            phantom: PhantomData,
        }
    }

    /// Get state for strand.
    pub fn get<'a>(&self, strand: &'a Strand<'v, '_>) -> &'a T {
        unsafe {
            strand
                .inner
                .locals
                .get_unchecked(self.index)
                .cast()
                .as_ref()
        }
    }
}

/// Key for accessing a strand-local GC root.
///
/// Like [`Root`](crate::value::Root), but scoped to a strand rather than the
/// entire VM. The value is duplicated into derived strands when they are created.
pub struct LocalRootKey<'v> {
    index: usize,
    phantom: PhantomData<&'v mut &'v ()>,
}

impl<'v> LocalRootKey<'v> {
    pub(crate) fn new(index: usize) -> Self {
        Self {
            index,
            phantom: PhantomData,
        }
    }

    /// Get a [`Slot`] for reading/writing this root in the given strand.
    pub fn slot<'s>(&self, strand: &Strand<'v, 's>) -> Slot<'v, 's> {
        unsafe { strand.inner.local_roots.get_unchecked(self.index).slot() }
    }
}

/// Token permitting short-lived interior borrows of values.
///
/// This is only constructible through [`Strand::access`].
pub struct Access<'v, 's> {
    vm: &'v Vm<'v>,
    phantom: PhantomData<&'s mut &'v ()>,
}

impl<'v, 's> Deref for Access<'v, 's> {
    type Target = Vm<'v>;

    fn deref(&self) -> &Self::Target {
        self.vm
    }
}

/// Ring of descendant strands in a group.
/// Separate struct avoids infinite type recursion with StrandInner.
pub(crate) struct StrandGroup {
    descendants: ring!(StrandInner<'static>, group_link),
}

impl StrandGroup {
    pub(crate) fn new() -> Self {
        Self {
            descendants: Default::default(),
        }
    }
}

/// Ring of running top-level strands (group leaders), owned by the VM.
///
/// Leaders link through their otherwise unused `group_link`.
pub(crate) type StrandRing = ring!(StrandInner<'static>, group_link);

/// Lifecycle of the strand behind a [`StrandCell`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CellState {
    /// Spawned but not yet polled; still queued in the spawn channel.
    Pending,
    /// Running or suspended.
    Live,
    /// Exited; the [`StrandInner`] is gone.
    Exited,
}

/// Shared, non-owning reference to a strand used by handles.
///
/// The pointer is set once the strand is at a stable address and cleared when
/// its [`StrandInner`] drops, so it never dangles. `id` is unique within the VM
/// and never reused.
pub(crate) struct StrandCell {
    ptr: Cell<*const StrandInner<'static>>,
    state: Cell<CellState>,
    id: u64,
}

impl StrandCell {
    fn new(vm: &Vm<'_>) -> Rc<Self> {
        let id = vm.next_strand_id.get();
        vm.next_strand_id.set(id + 1);
        Rc::new(Self {
            ptr: Cell::new(null()),
            state: Cell::new(CellState::Pending),
            id,
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn state(&self) -> CellState {
        self.state.get()
    }

    /// Pointer to the live strand, if any.
    ///
    /// Dereferencing it is only valid until the caller next yields: any
    /// suspension can let the strand exit.
    pub(crate) fn get<'v>(&self) -> Option<NonNull<StrandInner<'v>>> {
        NonNull::new(self.ptr.get().cast_mut()).map(NonNull::cast)
    }

    fn attach(&self, inner: &StrandInner<'_>) {
        debug_assert_eq!(self.state.get(), CellState::Pending);
        self.ptr.set(ptr::from_ref(inner).cast());
        self.state.set(CellState::Live);
    }
}

#[repr(C)]
pub(crate) struct StrandInner<'v> {
    group_link: Link<StrandInner<'static>>,
    vm: &'v Vm<'v>,
    // Strand-local input and output.  A replaced value drops after the access
    // ends: a drop can run native code that touches the strand.
    input: ValueCell<'v>,
    output: ValueCell<'v>,
    errors: ErrorTable<'v>,
    /// Slot of the error being handled by the innermost `guard` call, which
    /// owns the error while the slot is focused.
    error_focus: Cell<Option<NonNull<ErrorSlot<'v>>>>,
    locals: alias::Box<[NonNull<()>]>,
    local_roots: alias::Box<[ValueCell<'v>]>,
    arena: Arena,
    pub(crate) interrupt: RefCell<InterruptToken<'v>>,
    /// The strand's own token, which `interrupt` starts as.  Unlike `interrupt`,
    /// it is never swapped by [`with_interrupt_token!`](crate::with_interrupt_token), so canceling it
    /// cancels the whole strand.
    base_interrupt: InterruptToken<'v>,
    interrupt_registered: Cell<bool>,
    interrupt_mask: Cell<InterruptMask>,
    // Nested synchronous calls depth
    sync_depth: Cell<u32>,
    // Logical callable frame depth (Do + native frames only)
    call_depth: Cell<u32>,
    sp: Cell<Option<frame::Ptr<'v>>>,
    pub(crate) start: Value<'v>,
    is_leader: Cell<bool>,
    leader: Cell<*const StrandGroup>,
    /// Scoped parent; null for group leaders.  A scoped parent always outlives
    /// its children.
    parent: Cell<*const StrandInner<'static>>,
    /// Handle cell, allocated the first time a handle is taken.
    cell: OnceCell<Rc<StrandCell>>,
}

pub(crate) struct SpGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
    prior: Option<frame::Ptr<'v>>,
}

pub(crate) struct CallDepthGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
}

pub(crate) struct ErrorFocusGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
    prior: Option<NonNull<ErrorSlot<'v>>>,
}

impl<'v, 'a> Drop for SpGuard<'v, 'a> {
    fn drop(&mut self) {
        self.inner.sp.set(self.prior)
    }
}

impl<'v, 'a> Drop for CallDepthGuard<'v, 'a> {
    fn drop(&mut self) {
        self.inner.call_depth.update(|depth| depth - 1);
    }
}

impl<'v, 'a> Drop for ErrorFocusGuard<'v, 'a> {
    fn drop(&mut self) {
        self.inner.error_focus.set(self.prior);
    }
}

#[must_use]
pub(crate) struct LeaderGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
}

impl<'v, 'a> Drop for LeaderGuard<'v, 'a> {
    fn drop(&mut self) {
        self.inner.leader.set(ptr::null());
    }
}

impl<'v> Drop for StrandInner<'v> {
    fn drop(&mut self) {
        // Leaders are linked into the VM ring, members into their leader's
        // `descendants` ring.  A leader's `leader` pointer is already cleared by
        // its `LeaderGuard`, so `is_leader` covers it.
        if self.is_leader.get() || !self.leader.get().is_null() {
            unsafe { self.group_link.remove() }
        }
        if let Some(cell) = self.cell.get() {
            cell.ptr.set(null());
            cell.state.set(CellState::Exited);
        }
        for (vtbl, local) in self.vm.locals.iter().zip(self.locals.iter()) {
            unsafe { (vtbl.drop)(*local) }
        }
    }
}

const ARENA_DEFAULT_SIZE: usize = 1024 * 16;
#[cfg(debug_assertions)]
const MAX_CALL_DEPTH: u32 = 128;
#[cfg(not(debug_assertions))]
const MAX_CALL_DEPTH: u32 = 1000;

impl<'v> StrandInner<'v> {
    pub(crate) fn interrupt_error(&self) -> Error<'v, '_> {
        let interrupt = self.interrupt.borrow();
        let mask = self.interrupt_mask.get();
        if interrupt.is_canceled() && !mask.contains(InterruptMask::CANCELED) {
            Error::canceled_raw(self)
        } else if interrupt.is_timed_out() && !mask.contains(InterruptMask::TIMED_OUT) {
            Error::timed_out_raw(self)
        } else {
            unreachable!("interrupt_error without pending interrupt")
        }
    }

    pub(crate) fn new(vm: &'v Vm<'v>, interrupt: Option<InterruptToken<'v>>) -> Self {
        let interrupt = interrupt.unwrap_or_else(InterruptToken::new);
        Self {
            group_link: Link::new(),
            vm,
            input: ValueCell::default(),
            output: ValueCell::default(),
            errors: ErrorTable::new(),
            error_focus: Cell::new(None),
            locals: vm
                .locals
                .iter()
                .map(|vtbl| (vtbl.init)())
                .collect::<Vec<_>>()
                .into(),
            local_roots: (0..vm.local_root_count.get())
                .map(|_| ValueCell::default())
                .collect::<Vec<_>>()
                .into(),
            arena: Arena::new(ARENA_DEFAULT_SIZE),
            base_interrupt: interrupt.clone(),
            interrupt: RefCell::new(interrupt),
            interrupt_registered: Cell::new(false),
            interrupt_mask: Cell::new(InterruptMask::empty()),
            call_depth: Cell::new(0),
            sp: Cell::new(None),
            start: Value::NIL,
            is_leader: Cell::new(false),
            leader: Cell::new(std::ptr::null()),
            parent: Cell::new(null()),
            cell: OnceCell::new(),
            sync_depth: Cell::new(0),
        }
    }

    pub(crate) unsafe fn push_sp(&self, sp: frame::Ptr<'v>) -> SpGuard<'v, '_> {
        let guard = SpGuard {
            inner: self,
            prior: self.sp.get(),
        };
        self.sp.set(Some(sp));
        guard
    }

    pub(crate) fn push_call_depth(&self) -> Result<'v, '_, CallDepthGuard<'v, '_>> {
        if self.call_depth.get() >= MAX_CALL_DEPTH {
            return Err(Error::call_depth_raw(self));
        }
        self.call_depth.update(|depth| depth.strict_add(1));
        Ok(CallDepthGuard { inner: self })
    }

    pub(crate) fn derived(
        strand: &Strand<'v, '_>,
        interrupt: Option<InterruptToken<'v>>,
        kind: InheritKind,
    ) -> Self {
        let locals = strand
            .locals
            .iter()
            .zip(strand.inner.locals.iter())
            .map(|(vtbl, local)| unsafe { (vtbl.inherit)(*local, strand, kind) })
            .collect::<Vec<_>>();
        let interrupt = interrupt.unwrap_or_else(|| strand.inner.interrupt.borrow().clone());
        Self {
            group_link: Link::new(),
            vm: strand.inner.vm,
            input: strand.inner.input().into(),
            output: strand.inner.output().into(),
            errors: ErrorTable::new(),
            error_focus: Cell::new(None),
            locals: locals.into(),
            local_roots: strand
                .inner
                .local_roots
                .iter()
                // Safety: see the field
                .map(|cell| unsafe { cell.get() }.into())
                .collect::<Vec<_>>()
                .into(),
            arena: Arena::new(ARENA_DEFAULT_SIZE),
            base_interrupt: interrupt.clone(),
            interrupt: RefCell::new(interrupt),
            interrupt_registered: Cell::new(false),
            interrupt_mask: Cell::new(InterruptMask::empty()),
            call_depth: Cell::new(strand.inner.call_depth.get()),
            sp: Cell::new(None),
            start: Value::NIL,
            is_leader: Cell::new(false),
            leader: Cell::new(null()),
            parent: Cell::new(null()),
            cell: OnceCell::new(),
            sync_depth: Cell::new(0),
        }
    }

    /// Initialize this strand as a group leader.
    /// Must be called after the strand is at a stable address.
    pub(crate) unsafe fn init_group_leader<'a>(
        &'a self,
        group: &'a StrandGroup,
    ) -> LeaderGuard<'v, 'a> {
        self.group_link.init();
        group.descendants.init();
        self.leader.set(group);
        self.is_leader.set(true);
        LeaderGuard { inner: self }
    }

    /// Initialize this strand as a group member and register with the leader's ring.
    ///
    /// # Safety
    /// The leader's `StrandGroup` (pointed to by `self.leader`) must be valid and at a
    /// stable address.  `self` must be at a stable address.
    pub(crate) unsafe fn init_group_member(&self, parent: &StrandInner<'v>) {
        self.group_link.init();
        let leader = parent.leader.get();
        self.leader.set(leader);
        self.parent.set(ptr::from_ref(parent).cast());
        unsafe {
            (*leader)
                .descendants
                .push_front(NonNull::from_ref(self).cast());
        }
    }

    /// Link this group leader into the VM's ring of top-level strands.
    ///
    /// # Safety
    /// Must follow [`Self::init_group_leader`], with `self` at a stable address
    /// until it drops (which unlinks it).
    pub(crate) unsafe fn link_top_level(&self) {
        debug_assert!(self.is_leader.get());
        unsafe { self.vm.strands.push_front(NonNull::from_ref(self).cast()) }
    }

    /// The handle cell for this strand, allocating it on first use.
    ///
    /// `self` must be at a stable address.
    pub(crate) fn cell(&self) -> Rc<StrandCell> {
        self.cell
            .get_or_init(|| {
                let cell = StrandCell::new(self.vm);
                cell.attach(self);
                cell
            })
            .clone()
    }

    /// The strand's own interrupt token, regardless of any token selected with
    /// [`with_interrupt_token!`](crate::with_interrupt_token).
    pub(crate) fn base_interrupt(&self) -> &InterruptToken<'v> {
        &self.base_interrupt
    }

    /// Scoped parent, if this is a scoped strand.
    pub(crate) fn parent(&self) -> Option<&StrandInner<'v>> {
        // Safety: a scoped parent outlives its children
        unsafe { self.parent.get().cast::<StrandInner<'v>>().as_ref() }
    }

    /// Immediate scoped children, in creation order.
    pub(crate) fn children(&self) -> Vec<&StrandInner<'v>> {
        let leader = self.leader.get();
        if leader.is_null() {
            return Vec::new();
        }
        let me = ptr::from_ref(self).cast::<StrandInner<'static>>();
        // Safety: the leader's group outlives every member, and members unlink
        // themselves before they go away.
        let mut children: Vec<_> = unsafe { (*leader).descendants.iter() }
            .map(|p| unsafe { &*p.cast::<StrandInner<'v>>().as_ptr() })
            .filter(|child| ptr::eq(child.parent.get(), me))
            .collect();
        // The ring is push-front
        children.reverse();
        children
    }

    /// All strands in this leader's group, in creation order.
    fn group_members(&self) -> Vec<&StrandInner<'v>> {
        let leader = self.leader.get();
        if !self.is_leader.get() || leader.is_null() {
            return Vec::new();
        }
        let mut members: Vec<_> = unsafe { (*leader).descendants.iter() }
            .map(|p| unsafe { &*p.cast::<StrandInner<'v>>().as_ptr() })
            .collect();
        members.reverse();
        members
    }

    /// Backtrace of this strand while it is suspended, walked from `sp`.
    ///
    /// Stops after the nearest `<scoped>` boundary unless `full` is set.
    ///
    /// # Safety
    /// The strand must be suspended (not the running strand), so `sp` is accurate.
    pub(crate) unsafe fn suspended_entries(&self, full: bool) -> Vec<UnwindEntry<'v>> {
        unsafe { walk_entries(self.sp.get(), full) }
    }

    pub(crate) fn vm(&self) -> &'v Vm<'v> {
        self.vm
    }

    pub(crate) fn input(&self) -> Value<'v> {
        // Safety: see the field
        unsafe { self.input.get() }
    }

    pub(crate) fn output(&self) -> Value<'v> {
        // Safety: see the field
        unsafe { self.output.get() }
    }

    /// Replace the strand's input, returning the old value for the caller to drop.
    #[must_use]
    pub(crate) fn replace_input(&self, value: Value<'v>) -> Value<'v> {
        // Safety: see the field
        unsafe { self.input.slot() }.replace(value)
    }

    /// Replace the strand's output, returning the old value for the caller to drop.
    #[must_use]
    pub(crate) fn replace_output(&self, value: Value<'v>) -> Value<'v> {
        // Safety: see the field
        unsafe { self.output.slot() }.replace(value)
    }

    pub(crate) fn errors(&self) -> &ErrorTable<'v> {
        &self.errors
    }

    /// Backtrace of the error being handled, if any.
    pub(crate) fn focused_backtrace(&self) -> Option<&[UnwindEntry<'v>]> {
        // Safety: the focusing `guard` owns the error until it restores the
        // prior focus, and nothing writes the slot meanwhile
        self.error_focus
            .get()
            .map(|slot| unsafe { ErrorSlot::backtrace(slot) })
    }

    pub(crate) fn focused_slot(&self) -> Option<NonNull<ErrorSlot<'v>>> {
        self.error_focus.get()
    }

    /// Focus `slot` as the error being handled until the guard drops.
    pub(crate) fn focus_error(
        &self,
        slot: Option<NonNull<ErrorSlot<'v>>>,
    ) -> ErrorFocusGuard<'v, '_> {
        let prior = self.error_focus.replace(slot);
        ErrorFocusGuard { inner: self, prior }
    }

    /// Walk all GC-reachable values held by this strand's stack and mutable state.
    ///
    /// # Safety
    /// The frame chain from `self.sp` must be valid (all pointers must be live).
    pub(crate) unsafe fn scan_stack(&self, visitor: &mut dyn Visit) -> ControlFlow<()> {
        unsafe {
            // Scan start value
            self.start.accept(visitor)?;

            // Scan input/output and error roots
            self.input.accept(visitor)?;
            self.output.accept(visitor)?;
            self.errors.scan(visitor)?;

            // Scan local roots
            for cell in self.local_roots.iter() {
                cell.accept(visitor)?;
            }

            // Walk frame chain
            let mut ptr = self.sp.get();
            while let Some(p) = ptr {
                match p {
                    frame::Ptr::Do(frame_ptr) => {
                        let frame = frame_ptr.as_ref();
                        frame.scan(visitor)?;
                        ptr = frame.parent;
                    }
                    frame::Ptr::Scan(scan_ptr) => {
                        let scan = scan_ptr.as_ref();
                        scan.scan(visitor)?;
                        ptr = scan.parent;
                    }
                    frame::Ptr::Native(native_ptr) => {
                        let native = native_ptr.as_ref();
                        ptr = native.parent;
                    }
                    frame::Ptr::Boundary(_) => break,
                }
            }

            // Scan descendant strands in the group
            if self.is_leader.get() && !self.leader.get().is_null() {
                for descendant in (*self.leader.get()).descendants.iter() {
                    let descendant = &*descendant.cast::<StrandInner<'v>>().as_ptr();
                    descendant.scan_stack(visitor)?;
                }
            }

            ControlFlow::Continue(())
        }
    }
}

/// One strand's state, as gathered by [`Strand::snapshot_raw`].
pub(crate) struct RawInfo<'v> {
    pub(crate) cell: Rc<StrandCell>,
    pub(crate) parent: Option<Rc<StrandCell>>,
    /// Backtrace up to the nearest boundary.
    pub(crate) entries: Vec<UnwindEntry<'v>>,
}

/// Collect backtrace entries from a frame chain, deepest first.
///
/// Records a `<scoped>` boundary and stops there unless `full` is set, in which
/// case the walk continues into the scoped parent's frames.
///
/// # Safety
/// Every frame reachable from `head` must be live.
unsafe fn walk_entries<'v>(mut head: Option<frame::Ptr<'v>>, full: bool) -> Vec<UnwindEntry<'v>> {
    let mut out = Vec::new();
    while let Some(ptr) = head {
        match ptr {
            frame::Ptr::Do(frame) => {
                let frame = unsafe { frame.as_ref() };
                out.push(UnwindEntry::Do {
                    program: frame.program.clone(),
                    function_index: frame.func as u32,
                    pc: frame.pc.get() as u32,
                });
                head = frame.parent;
            }
            frame::Ptr::Native(frame) | frame::Ptr::Boundary(frame) => {
                let frame = unsafe { frame.as_ref() };
                out.push(UnwindEntry::Native {
                    module: frame.module.clone(),
                    receiver: frame.receiver.clone(),
                    method: frame.method.clone(),
                });
                if !full && matches!(ptr, frame::Ptr::Boundary(_)) {
                    break;
                }
                head = frame.parent;
            }
            frame::Ptr::Scan(frame) => {
                let frame = unsafe { frame.as_ref() };
                head = frame.parent;
            }
        }
    }
    out
}

/// A pinned future that integrates with strand interrupts.
///
/// ## Interrupt Integration
///
/// This wrapper connects a future to the strand's interrupt system:
/// - When the future suspends, registers the waker with the strand's current
///   interrupt token, unless a nested `Pinned` already did; registering fails,
///   and the future returns an interrupt error, if an unmasked interrupt is pending
/// - When polled again, or dropped, unregisters first, so a registration only
///   lasts while the strand is suspended
///
/// The innermost `Pinned` around the suspension point registers, and therefore
/// returns the error; futures it encloses are dropped with it.
///
/// ## Interrupt Masking
///
/// Causes in the strand's interrupt mask are ignored. Registrations snapshot
/// the mask so a masked cause leaves them available for another cause.
pub(crate) struct Pinned<'v, 's, 'a, R> {
    inner: dolang_util::pin::Pinned<'a, Result<'v, 's, R>>,
    strand: &'s StrandInner<'v>,
    /// Interrupt registration ID. None if not yet registered or already cleaned up.
    registration: Option<(InterruptToken<'v>, InterruptId)>,
}

impl<'v, 's, 'a, R> Future for Pinned<'v, 's, 'a, R> {
    type Output = Result<'v, 's, R>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        // A registration covers a single suspension; drop it on wake, so the next one
        // snapshots the token and mask in effect when the strand suspends again
        self.unregister();
        let res = unsafe { Pin::new_unchecked(&mut self.inner) }.poll(cx);
        if res.is_ready() {
            return res;
        }
        if self.strand.sync_depth.get() != 0 {
            return Poll::Ready(Err(Error::runtime_raw(
                self.strand,
                "attempt to suspend in sync context",
            )));
        }
        if self.strand.interrupt_registered.get() {
            // A nested `Pinned` registered this suspension
            return Poll::Pending;
        }
        let interrupt = self.strand.interrupt.borrow().clone();
        let Some(id) = interrupt.register(cx.waker(), self.strand.interrupt_mask.get()) else {
            // Interrupted, and the cause isn't masked
            return Poll::Ready(Err(self.strand.interrupt_error()));
        };
        self.registration = Some((interrupt, id));
        self.strand.interrupt_registered.set(true);
        Poll::Pending
    }
}

impl<R> Pinned<'_, '_, '_, R> {
    #[inline]
    fn unregister(&mut self) {
        if let Some((interrupt, id)) = self.registration.take() {
            interrupt.unregister(id);
            self.strand.interrupt_registered.set(false);
        }
    }
}

impl<R> Drop for Pinned<'_, '_, '_, R> {
    #[inline]
    fn drop(&mut self) {
        self.unregister();
    }
}

/// Execution context.
pub struct Strand<'v, 's> {
    pub(crate) inner: &'s StrandInner<'v>,
    pub(crate) fp: frame::Ptr<'v>,
    phantom: PhantomData<&'s mut &'s ()>,
}

impl<'v, 's> Strand<'v, 's> {
    pub(crate) unsafe fn from_native_frame(
        inner: &'s StrandInner<'v>,
        frame: &frame::Native<'v>,
    ) -> Self {
        Self {
            inner,
            fp: frame::Ptr::Native(NonNull::from_ref(frame)),
            phantom: PhantomData,
        }
    }

    /// Enter the Do `frame` one call level deeper, returning a handle on this strand with
    /// `frame` as its frame pointer.
    ///
    /// Pass the frame's result through [`FrameScope::finish`] to add its backtrace entry.
    ///
    /// # Safety
    ///
    /// `frame` must be a live Do frame whose parent is this strand's frame, and the scope
    /// must be dropped, not forgotten, before `frame` is.
    #[inline]
    pub(crate) unsafe fn enter_do_frame<'f>(
        &'f mut self,
        frame: &'f frame::CallFrame<'v>,
    ) -> Result<'v, 's, FrameScope<'v, 's, 'f>> {
        unsafe { self.enter_frame(FrameEntry::Do(frame)) }
    }

    /// Enter the native `frame` one call level deeper, as for [`Self::enter_do_frame`].
    ///
    /// This backs [`in_native_frame!`].
    ///
    /// # Safety
    ///
    /// `frame`'s parent must be this strand's frame, and the scope must be dropped, not
    /// forgotten, before `frame` is.
    #[inline]
    pub(crate) unsafe fn enter_native_frame<'f>(
        &'f mut self,
        frame: &'f Native<'v>,
    ) -> Result<'v, 's, FrameScope<'v, 's, 'f>> {
        unsafe { self.enter_frame(FrameEntry::Native(frame)) }
    }

    #[inline]
    unsafe fn enter_frame<'f>(
        &'f mut self,
        entry: FrameEntry<'v, 'f>,
    ) -> Result<'v, 's, FrameScope<'v, 's, 'f>> {
        let inner = self.inner;
        if inner.call_depth.get() >= MAX_CALL_DEPTH {
            return Err(Error::call_depth_raw(inner));
        }
        inner.call_depth.update(|depth| depth.strict_add(1));
        let fp = match entry {
            FrameEntry::Do(frame) => frame::Ptr::Do(NonNull::from_ref(frame)),
            FrameEntry::Native(frame) => frame::Ptr::Native(NonNull::from_ref(frame)),
        };
        // Keep `sp` at the innermost frame so a suspended strand's backtrace includes the
        // frame it's blocked in.  The GC skips native frames.
        let prior = inner.sp.replace(Some(fp));
        Ok(FrameScope {
            strand: Strand {
                inner,
                fp,
                phantom: PhantomData,
            },
            prior,
            entry,
            _borrow: PhantomData,
        })
    }

    /// Iterate over live backtrace, deepest frame first.
    #[inline]
    pub fn backtrace(&self) -> impl Iterator<Item = impl frame::Frame> + '_ {
        FrameIter {
            head: Some(self.fp),
            phantom: PhantomData,
        }
    }

    pub fn error_backtrace(&self) -> Option<impl ExactSizeIterator<Item = impl frame::Frame> + '_> {
        self.inner
            .focused_backtrace()
            .map(|entries| BacktraceIter::new(entries.iter()))
    }

    pub(crate) fn backtrace_entries(&self) -> Vec<UnwindEntry<'v>> {
        unsafe { walk_entries(Some(self.fp), true) }
    }

    /// Live backtrace of this (running) strand, stopping after the nearest
    /// `<scoped>` boundary unless `full` is set.
    pub(crate) fn backtrace_entries_to_boundary(&self, full: bool) -> Vec<UnwindEntry<'v>> {
        unsafe { walk_entries(Some(self.fp), full) }
    }

    /// Backtrace of `target`, which may be this strand (walked from its live
    /// `fp`) or a suspended one (walked from its `sp`).
    ///
    /// # Safety
    /// `target` must be live and must not be used past the next yield.
    pub(crate) unsafe fn entries_of(
        &self,
        target: &StrandInner<'v>,
        full: bool,
    ) -> Vec<UnwindEntry<'v>> {
        if ptr::eq(target, self.inner) {
            self.backtrace_entries_to_boundary(full)
        } else {
            unsafe { target.suspended_entries(full) }
        }
    }

    /// Observe one strand without allocating GC objects.
    ///
    /// # Safety
    /// As for [`Self::entries_of`].
    pub(crate) unsafe fn observe(&self, target: &StrandInner<'v>) -> RawInfo<'v> {
        RawInfo {
            cell: target.cell(),
            parent: target.parent().map(StrandInner::cell),
            entries: unsafe { self.entries_of(target, false) },
        }
    }

    /// Observe every running strand in the VM, without allocating GC objects.
    ///
    /// Each top-level strand comes in creation order, followed by its scoped
    /// descendants in creation order.  Strands still queued in the spawn
    /// channel are not included.
    pub(crate) fn snapshot_raw(&self) -> Vec<RawInfo<'v>> {
        // Safety: the walk is synchronous, so every strand in the rings stays
        // live and suspended (except this one) throughout.
        unsafe {
            let mut leaders: Vec<_> = self
                .inner
                .vm
                .strands
                .iter()
                .map(|p| &*p.cast::<StrandInner<'v>>().as_ptr())
                .collect();
            leaders.reverse();
            let mut out = Vec::new();
            for leader in leaders {
                out.push(self.observe(leader));
                for member in leader.group_members() {
                    out.push(self.observe(member));
                }
            }
            out
        }
    }

    /// Get reference to underlying VM
    #[inline]
    pub fn vm(&self) -> &'v Vm<'v> {
        self.inner.vm
    }

    /// Access value interiors.
    ///
    /// The provided closure is invoked with an [`Access`] token which can be used to
    /// access the interiors of values such as [`Str`](crate::value::view::Str) and
    /// [`Bin`](crate::value::view::Bin)
    #[inline]
    pub fn access<R>(&mut self, f: impl for<'a> FnOnce(&'a Access<'v, 's>) -> R) -> R {
        let access = Access {
            vm: self.inner.vm,
            phantom: PhantomData,
        };
        f(&access)
    }

    /// Push `frame`, returning a handle on this strand with `frame` as its frame pointer, and
    /// the frame's slots.
    ///
    /// This backs [`with_slots!`](crate::with_slots).
    ///
    /// # Safety
    ///
    /// The scope must be dropped, not forgotten, before `frame` is.
    ///
    /// # Panics
    ///
    /// Panics if `frame` was entered before.
    #[doc(hidden)]
    #[inline]
    pub unsafe fn enter_slot_frame<'f, const N: usize>(
        &'f mut self,
        frame: &'f SlotFrame<'v, N>,
    ) -> (SlotScope<'v, 's, 'f>, [Slot<'v, 'f>; N]) {
        let scan = frame::Scan::cells(Some(self.fp), &frame.values);
        assert!(frame.scan.set(scan).is_ok(), "slot frame entered twice");
        // Safety: the caller's contract; the scan reads `frame.values`, which `'f` keeps live
        let scope = unsafe { self.enter_scan(frame.scan.get().unwrap()) };
        // Safety: the slots are the cells' only writers; the scan only reads
        let slots = frame.values.each_ref().map(|value| unsafe { value.slot() });
        (scope, slots)
    }

    /// Push `frame`, returning a handle on this strand with `frame` as its frame pointer, and
    /// the frame's slots.
    ///
    /// This backs [`with_slots_dynamic!`].
    ///
    /// # Safety
    ///
    /// The scope must be dropped, not forgotten, before `frame` is.
    ///
    /// # Panics
    ///
    /// Panics if `frame` was entered before.
    #[inline]
    pub(crate) unsafe fn enter_dyn_slot_frame<'f>(
        &'f mut self,
        frame: &'f DynSlotFrame<'v>,
    ) -> (SlotScope<'v, 's, 'f>, crate::value::Slots<'v, 'f>) {
        let scan = frame::Scan::cell_vec(Some(self.fp), &frame.values);
        assert!(frame.scan.set(scan).is_ok(), "slot frame entered twice");
        // Safety: the caller's contract; the scan reads `frame.values`, which `'f` keeps live
        let scope = unsafe { self.enter_scan(frame.scan.get().unwrap()) };
        // Safety: the slots are the cells' only writers; the scan only reads
        let slots = unsafe { crate::value::Slots::new(frame.values.as_slice()) };
        (scope, slots)
    }

    /// Push the rooting frame `scan`, returning a handle on this strand with `scan` as its
    /// frame pointer.
    ///
    /// # Safety
    ///
    /// The scope must be dropped, not forgotten, before `scan` is.  Until then, each visit
    /// must visit every GC reference the rooted data owns, exactly once, and nothing else:
    /// the collector counts visits against reference counts, so a missed, repeated, or
    /// foreign visit can free a live object.  The rooted data must not change while a
    /// collection runs, so every visit within one collection sees the same references.
    /// Data that changes between collections needs interior mutability, such as a
    /// [`Cell`] or [`RefCell`], since the scan holds a shared borrow of it.
    #[inline]
    pub(crate) unsafe fn enter_scan<'f>(
        &'f mut self,
        scan: &'f frame::Scan<'v>,
    ) -> SlotScope<'v, 's, 'f> {
        let fp = frame::Ptr::Scan(NonNull::from_ref(scan));
        let prior = self.inner.sp.replace(Some(fp));
        SlotScope {
            strand: Strand {
                inner: self.inner,
                fp,
                phantom: PhantomData,
            },
            prior,
            _borrow: PhantomData,
        }
    }

    /// Store a new tuple of `items` into `out`.
    ///
    /// Each item is rooted as it is taken, so taking an item may allocate.
    ///
    /// # Panics
    /// Panics if `items` yields a different number of items than its length.
    pub fn tuple<I>(&mut self, items: I, out: impl Output<'v>)
    where
        I: IntoIterator,
        I::Item: Input<'v>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut items = items.into_iter();
        let cells: Vec<ValueCell<'v>> = (0..items.len()).map(|_| ValueCell::default()).collect();
        let visit =
            |visitor: &mut dyn Visit| cells.iter().try_for_each(|cell| cell.accept(visitor));
        let scan = frame::Scan::new(Some(self.fp), &visit);
        // Safety: `visit` visits each cell, and each cell owns at most one reference
        let mut scope = unsafe { self.enter_scan(&scan) };
        let strand = &mut *scope;
        for cell in &cells {
            let mut item = items.next().expect("tuple items fewer than their length");
            // Safety: the cell is scanned and is written only through this slot
            let slot = unsafe { cell.slot() };
            item.input_take(strand, slot, crate::value::private::Sealed);
        }
        assert!(items.next().is_none(), "tuple items more than their length");
        // Safety: as above; finishing the embryo does not allocate
        let taken = cells.iter().map(|cell| unsafe { cell.slot() }.take());
        tuple::embryo(strand, cells.len()).finish(strand, taken, out)
    }

    /// Store a new range from `start` to `end` into `out`.
    pub fn range(&mut self, start: impl Input<'v>, end: impl Input<'v>, out: impl Output<'v>) {
        let strand = self;
        with_slots!(strand, |mut start_slot, mut end_slot| {
            Output::set(strand, Slot::reborrow(&mut start_slot), start);
            Output::set(strand, Slot::reborrow(&mut end_slot), end);
            let embryo = strand.builtin_types().range.embryo(strand);
            let range = range::Range::new(start_slot.take(), end_slot.take(), Value::NIL);
            embryo.finish_imm(strand, range, out)
        })
    }

    /// Replace the interrupt mask until the returned guard is dropped.
    ///
    /// This backs [`with_interrupt_mask!`](crate::with_interrupt_mask).
    #[doc(hidden)]
    #[inline]
    pub fn interrupt_mask_guard(&self, mask: InterruptMask) -> InterruptMaskGuard<'v, 's> {
        InterruptMaskGuard {
            inner: self.inner,
            orig: self.inner.interrupt_mask.replace(mask),
        }
    }

    /// Select `interrupt` as the strand's token until the returned guard is dropped.
    ///
    /// This backs [`with_interrupt_token!`](crate::with_interrupt_token).
    #[doc(hidden)]
    #[inline]
    pub fn interrupt_token_guard(
        &self,
        interrupt: InterruptToken<'v>,
    ) -> InterruptTokenGuard<'v, 's> {
        let orig = self.inner.interrupt.replace(interrupt);
        self.inner.interrupt_registered.set(false);
        InterruptTokenGuard {
            inner: self.inner,
            orig,
        }
    }

    /// Return the currently active interrupt mask.
    pub fn interrupt_mask(&self) -> InterruptMask {
        self.inner.interrupt_mask.get()
    }

    /// Run function with interruption interception. If this strand is interrupted, the [`Future`]
    /// returned by `f` may be dropped, but the [`Future`] returned by this function
    /// will return an interrupt error. This allows cleaning up using normal async
    /// error handling instead of drop guards.
    #[inline]
    pub async fn interrupt_guard<R>(
        &mut self,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        self.pin_future_call(f).await
    }

    /// Check trap without running garbage collection.
    /// Safe to call while holding GC object borrows.  Host-provided functions that
    /// perform CPU-bound work without significant allocation should use this.
    pub fn check_trap(&mut self) -> Result<'v, 's, ()> {
        self.vm().check_trap(self)
    }

    /// Check trap and run GC if the collection threshold is exceeded.
    /// Must be called with all GC object borrows released, since GC may collect
    /// objects whose reference counts drop to zero during trial deletion.
    /// Host-provided functions that allocate or iterate over potentially large
    /// structures should use this.
    pub fn check_trap_gc(&mut self) -> Result<'v, 's, ()> {
        self.vm().check_trap_gc(self)
    }

    /// Get current iterator.
    pub fn input(&self, mut out: impl Output<'v>) {
        Slot::from_output(&mut out).store(self.inner.input());
    }

    /// Get current sink.
    pub fn output(&self, mut out: impl Output<'v>) {
        Slot::from_output(&mut out).store(self.inner.output());
    }

    /// Spawn a new strand scoped to this strand which executes `f`.
    /// If `interrupt` is [`Some`], it will be the interrupt token for the new strand,
    /// otherwise it gets a fresh token nested under [`Self::interrupt_token`], so
    /// canceling the new strand never cancels this one.
    pub async fn spawn_scoped<R>(
        &self,
        interrupt: Option<InterruptToken<'v>>,
        f: impl for<'ss> AsyncFnOnce(&mut Strand<'v, 'ss>) -> Result<'v, 'ss, R>,
    ) -> Result<'v, 's, R> {
        let interrupt = interrupt
            .unwrap_or_else(|| self.inner.interrupt.borrow().nested(InterruptMask::empty()));
        let strand = StrandInner::derived(self, Some(interrupt), InheritKind::Scoped);
        // Safety: strand is at a stable address (async fn generator state is heap-allocated)
        // and the leader's StrandGroup outlives this scoped strand.
        unsafe { strand.init_group_member(self.inner) };
        let native = frame::Native {
            module: Cow::Borrowed("strand"),
            receiver: Cow::Borrowed("<scoped>"),
            method: None,
            parent: Some(self.fp),
        };
        // Seed `sp` with the boundary so a suspended walk can reach it.  The GC
        // stops at boundaries.
        strand
            .sp
            .set(Some(frame::Ptr::Boundary(NonNull::from_ref(&native))));
        let mut strand = Strand {
            inner: &strand,
            fp: frame::Ptr::Boundary(NonNull::from_ref(&native)),
            phantom: PhantomData,
        };
        match f(&mut strand).await {
            Ok(r) => Ok(r),
            Err(e) => {
                let mut e = e.migrate(self);
                e.push_backtrace(
                    self.inner,
                    UnwindEntry::Native {
                        module: native.module.clone(),
                        receiver: native.receiver,
                        method: None,
                    },
                );
                Err(e)
            }
        }
    }

    /// Spawn a background strand that runs `f` and returns a `Handle`.
    ///
    /// The background strand runs independently of the spawning strand and is managed
    /// by the VM's event loop. The returned `Handle` can be used to join, cancel,
    /// or check completion of the background strand.
    ///
    /// `arg` is an arbitrary GC value handed to `f` as its second parameter and kept
    /// rooted for the lifetime of the background strand (so it stays alive even before
    /// `f` starts running, e.g. while the future is queued in the spawn channel). Most
    /// callers pass a Do callable here and invoke it with `call!` inside `f`, but `f` is
    /// free to do anything with `arg` (or ignore it) — it's a plain Rust `AsyncFnOnce`,
    /// not hard-coded to a single call convention. `f` writes its result into `out`,
    /// exactly like the receiver of `call!`.
    ///
    /// If `stream` is `Some((strand_input, strand_output))`, the strand's input and output
    /// iterators are set to those values before `f` runs, and both are closed when
    /// `f` returns (regardless of success or failure).
    ///
    /// Returns an error if the spawn channel is not available (e.g. the VM is shutting down).
    pub(crate) fn spawn_background_raw<F>(
        &mut self,
        arg: Value<'v>,
        interrupt: InterruptToken<'v>,
        stream: Option<(Value<'v>, Value<'v>)>,
        f: F,
    ) -> Result<'v, 's, GcObj<'v, Handle<'v>>>
    where
        F: for<'ss> AsyncFnOnce(
                &mut Strand<'v, 'ss>,
                &Value<'v>,
                Slot<'v, '_>,
            ) -> Result<'v, 'ss, ()>
            + 'v,
    {
        let close_on_exit = stream.is_some();

        // Create StrandInner
        let mut inner =
            StrandInner::derived(self, Some(interrupt.clone()), InheritKind::Background);
        inner.call_depth.set(0);
        // Keeps `arg` rooted (via `scan_stack`) for as long as the strand exists,
        // including before `f` first runs.
        inner.start = arg;

        // Set strand-side I/O if this is a stream strand
        if let Some((recv, send)) = stream {
            *inner.input.get_mut() = recv;
            *inner.output.get_mut() = send;
        }

        let vm = self.vm();
        // The cell stays unattached until first poll, when `inner` reaches its
        // final address inside the pinned future.
        let cell = StrandCell::new(vm);

        // Create JoinHandle GcObj
        let handle = GcObj::new(
            vm.arena(),
            vm.builtin_types().strand_handle,
            Handle::new(cell.clone(), interrupt),
        );

        // Create weak ref for the future (doesn't participate in cycles)
        let weak_handle = GcObj::downgrade(&handle);

        // Create the background strand future
        //
        // `inner` is captured by value: nothing links to it before first poll, so
        // moving it is safe.  It drops when the future completes (or is dropped),
        // clearing the cell and unlinking it from the VM ring.
        let future = Box::pin(async move {
            // Safety (here and below): the future is pinned, so `inner` is at a
            // stable address from now on
            inner
                .cell
                .set(cell.clone())
                .ok()
                .expect("strand cell already set");
            cell.attach(&inner);
            drop(cell);
            let native = frame::Native {
                module: Cow::Borrowed("strand"),
                receiver: Cow::Borrowed("spawn"),
                method: None,
                parent: None,
            };
            // Initialize as group leader
            let group = StrandGroup::new();
            let _guard = unsafe { inner.init_group_leader(&group) };
            unsafe { inner.link_top_level() };
            let _sp = unsafe { inner.push_sp(frame::Ptr::Native(NonNull::from_ref(&native))) };
            // Safety: native outlives strand
            let mut strand = unsafe { Strand::from_native_frame(&inner, &native) };

            with_slots!(strand, |mut out, mut tmp, mut poison_err, mut poison_bt| {
                let result = strand
                    .pin_future_call(async |strand| {
                        f(strand, &inner.start, Slot::reborrow(&mut out)).await
                    })
                    .await;
                let result = match result {
                    Ok(()) => Completion::Ok(out.take()),
                    Err(e) => Completion::Err(e.into_pair(strand)),
                };
                // Rooted in slots across the close calls below
                let poisoned = match &result {
                    Completion::Err((value, backtrace_entries)) => {
                        backtrace::create(strand, backtrace_entries.clone(), &mut poison_bt);
                        poison_err.store(value.dup());
                        true
                    }
                    Completion::Ok(_) => false,
                };

                // Store result in Handle via weak reference
                if let Some(handle) = weak_handle.upgrade() {
                    // SAFETY: a transient pin; completing cannot collect
                    unsafe { handle.pin_unchecked() }
                        .borrow_mut()
                        .expect("strand handle had outstanding borrow")
                        .complete(result);
                } else {
                    drop(result)
                }

                if close_on_exit {
                    let close = Sym::well_known(sym::CLOSE);
                    let backtrace_key = Sym::well_known(sym::BACKTRACE);
                    tmp.store(strand.inner.replace_input(Value::NIL));
                    if !tmp.is_nil() {
                        let _ = method!(strand, &tmp, close, &mut out);
                    }
                    tmp.store(strand.inner.replace_output(Value::NIL));
                    if !tmp.is_nil() {
                        let _ = if poisoned {
                            method!(
                                strand,
                                &tmp,
                                close,
                                &poison_err,
                                backtrace_key: &poison_bt,
                                &mut out
                            )
                        } else {
                            method!(strand, &tmp, close, &mut out)
                        };
                    }
                }
            })
        });

        // Send future through spawn channel
        if let Some(tx) = vm.spawn_tx.borrow().as_ref() {
            tx.unbounded_send(future).expect("spawn channel closed");
        } else {
            return Err(Error::runtime(self, "cannot spawn: VM is shutting down"));
        }

        Ok(handle)
    }

    /// Spawn a background strand that runs `f`, independent of this strand and managed
    /// by the VM's event loop.
    ///
    /// `arg` is an arbitrary value handed to `f` as its second parameter and kept rooted
    /// for the lifetime of the background strand (so it stays alive even before `f` starts
    /// running).
    ///
    /// The resulting handle is written to `out` — store it somewhere reachable (e.g. a
    /// [`Slot`] your extension owns) to keep the background strand rooted; it is a full
    /// `strand.Task` handle with `join`/`cancel`/`wait` operations.
    ///
    /// Returns an error if the VM is shutting down.
    ///
    /// `interrupt` defaults (when `None`) to a fresh token.
    pub fn spawn_background<F>(
        &mut self,
        arg: impl Input<'v>,
        interrupt: Option<&InterruptToken<'v>>,
        mut out: impl Output<'v>,
        f: F,
    ) -> Result<'v, 's, ()>
    where
        F: for<'ss> AsyncFnOnce(
                &mut Strand<'v, 'ss>,
                &Value<'v>,
                Slot<'v, '_>,
            ) -> Result<'v, 'ss, ()>
            + 'v,
    {
        let arg = Value::from_input(self, arg);
        let interrupt = interrupt
            .map(|t| t.nested(InterruptMask::empty()))
            .unwrap_or_else(InterruptToken::new);
        let handle = self.spawn_background_raw(arg, interrupt, None, f)?;
        Output::set(self, &mut out, &Value::from_object(handle));
        Ok(())
    }

    /// Get the interrupt token for this strand
    pub fn interrupt_token(&self) -> InterruptToken<'v> {
        self.inner.interrupt.borrow().clone()
    }

    /// Import a module, running the same import logic as used for Do `import` statements.
    /// See [`Builder::importer`](crate::vm::Builder::importer) for details.
    pub async fn import(&mut self, name: &str, mut out: impl Output<'v>) -> Result<'v, 's, ()> {
        self.inner
            .vm
            .import_raw(self, name, Slot::from_output(&mut out))
            .await
    }

    #[inline]
    pub(crate) fn pin_future_call<'b, R>(
        &'b mut self,
        f: impl for<'c> AsyncFnOnce(&'c mut Strand<'v, 's>) -> Result<'v, 's, R> + 'b,
    ) -> Pinned<'v, 's, 'b, R> {
        let inner = self.inner;
        Pinned {
            inner: unsafe { inner.arena.pin_future_unchecked(f(self)) },
            strand: inner,
            registration: None,
        }
    }

    pub fn sync<R>(
        &mut self,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        use std::task::{Context, RawWaker, RawWakerVTable};

        // No-op waker: the future must complete in a single poll because
        // `Pinned::poll` turns any `Pending` into `Ready(Err(...))` when
        // `sync_depth != 0`.
        const VTABLE: RawWakerVTable =
            RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);

        self.inner.sync_depth.update(|x| x.strict_add(1));
        // Scope `fut` so it's dropped before we update sync_depth below.
        let Poll::Ready(res) = Pin::new(&mut self.pin_future_call(f)).poll(&mut cx) else {
            unreachable!("future suspended inside sync context")
        };
        self.inner.sync_depth.update(|x| x - 1);
        res
    }

    pub(crate) fn builtin_types(&self) -> &'v BuiltinTypes<'v> {
        self.vm().builtin_types()
    }

    pub(crate) fn singletons(&self) -> &'v Singletons<'v> {
        self.vm().singletons()
    }
}

impl<'v, 'a> Deref for Strand<'v, 'a> {
    type Target = Vm<'v>;

    fn deref(&self) -> &Self::Target {
        self.inner.vm()
    }
}

impl<'v, 'a> AsRef<Vm<'v>> for Strand<'v, 'a> {
    fn as_ref(&self) -> &Vm<'v> {
        self
    }
}

impl<'v, 'a> Alloc<'v> for Strand<'v, 'a> {
    fn alloc_vm(&mut self, _: crate::vm::private::Sealed) -> &'v Vm<'v> {
        self.vm()
    }
}

/// Run a block with the strand-local iterator and sink redirected by a [`Redirect`].
///
/// Invoke as:
///
/// ```rust,no_run
/// # use dolang_runtime::{redirect, strand::{Redirect, Strand}, value::Singleton};
/// # async fn example<'v, 's>(strand: &mut Strand<'v, 's>) -> dolang_runtime::error::Result<'v, 's, ()> {
/// redirect!(strand, Redirect::new(strand).input(Singleton::Null), {
///     Ok(())
/// })
/// # }
/// ```
///
/// `strand` names both the strand the
/// [`Redirect`] was built from and, inside the body, the redirected one, as for
/// [`with_slots!`](crate::with_slots). The body runs inline, as for `with_slots!`. The
/// iterator and sink are restored when the body finishes, including when it leaves by `?`
/// or `return`, or the enclosing future is dropped.
#[macro_export]
macro_rules! redirect {
    ($strand:ident, $redirect:expr, $body:expr) => {{
        let mut guard = $crate::strand::Redirect::guard($redirect);
        let $strand = &mut *guard;
        $body
    }};
}

/// Input/output redirect configuration
pub struct Redirect<'v, 'a, 's> {
    strand: &'a mut Strand<'v, 's>,
    input: Option<Value<'v>>,
    output: Option<Value<'v>>,
}

impl<'v, 'a, 's> Deref for Redirect<'v, 'a, 's> {
    type Target = Strand<'v, 's>;

    fn deref(&self) -> &Self::Target {
        self.strand
    }
}

impl<'v, 'a, 's> DerefMut for Redirect<'v, 'a, 's> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.strand
    }
}

impl<'v, 'a, 's> AsRef<Strand<'v, 's>> for Redirect<'v, 'a, 's> {
    fn as_ref(&self) -> &Strand<'v, 's> {
        self
    }
}

impl<'v, 'a, 's> AsMut<Strand<'v, 's>> for Redirect<'v, 'a, 's> {
    fn as_mut(&mut self) -> &mut Strand<'v, 's> {
        self
    }
}

impl<'v, 'a, 's> Redirect<'v, 'a, 's> {
    pub fn new(strand: &'a mut Strand<'v, 's>) -> Self {
        Self {
            strand,
            input: None,
            output: None,
        }
    }

    /// Set iterator
    pub fn input(mut self, input: impl Input<'v>) -> Self {
        self.input = Some(Value::from_input(self.strand, input));
        self
    }

    /// Set sink
    pub fn output(mut self, output: impl Input<'v>) -> Self {
        self.output = Some(Value::from_input(self.strand, output));
        self
    }

    /// Apply the redirection until the returned guard is dropped.
    ///
    /// This backs [`redirect!`](crate::redirect).
    #[doc(hidden)]
    #[inline]
    pub fn guard(self) -> RedirectGuard<'v, 'a, 's> {
        let strand = self.strand;
        let input = self.input.map(|input| strand.inner.replace_input(input));
        let output = self
            .output
            .map(|output| strand.inner.replace_output(output));
        RedirectGuard {
            strand,
            input,
            output,
        }
    }
}

/// A strand with its iterator and sink redirected, which restores them when dropped.
#[doc(hidden)]
pub struct RedirectGuard<'v, 'a, 's> {
    strand: &'a mut Strand<'v, 's>,
    input: Option<Value<'v>>,
    output: Option<Value<'v>>,
}

impl<'v, 's> Deref for RedirectGuard<'v, '_, 's> {
    type Target = Strand<'v, 's>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.strand
    }
}

impl<'v, 's> DerefMut for RedirectGuard<'v, '_, 's> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.strand
    }
}

impl Drop for RedirectGuard<'_, '_, '_> {
    fn drop(&mut self) {
        if let Some(input) = self.input.take() {
            drop(self.strand.inner.replace_input(input));
        }
        if let Some(output) = self.output.take() {
            drop(self.strand.inner.replace_output(output));
        }
    }
}

#[cfg(test)]
mod interrupt_tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Wake, Waker},
    };

    use super::{InterruptMask, InterruptToken};

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counter_waker() -> (Arc<Counter>, Waker) {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        (counter.clone(), counter.into())
    }

    #[test]
    fn nested_tokens_filter_initial_and_subsequent_causes() {
        let parent = InterruptToken::new();
        parent.timeout();
        let filtered = parent.nested(InterruptMask::TIMED_OUT);
        let inherited = parent.nested(InterruptMask::empty());
        assert!(!filtered.is_timed_out());
        assert!(inherited.is_timed_out());

        let parent = InterruptToken::new();
        let filtered = parent.nested(InterruptMask::CANCELED);
        let inherited = parent.nested(InterruptMask::empty());
        parent.cancel();
        assert!(!filtered.is_canceled());
        assert!(inherited.is_canceled());
    }

    #[test]
    fn masked_registration_remains_for_another_cause() {
        let token = InterruptToken::new();
        let (counter, waker) = counter_waker();
        assert!(token.register(&waker, InterruptMask::TIMED_OUT).is_some());
        token.timeout();
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);
        token.cancel();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn registration_rejects_pending_unmasked_cause() {
        let token = InterruptToken::new();
        token.timeout();
        let (_, waker) = counter_waker();
        assert!(token.register(&waker, InterruptMask::empty()).is_none());
        assert!(token.register(&waker, InterruptMask::TIMED_OUT).is_some());
    }

    #[test]
    fn filtered_child_can_be_interrupted_directly() {
        let parent = InterruptToken::new();
        let child = parent.nested(InterruptMask::TIMED_OUT);
        parent.timeout();
        assert!(!child.is_timed_out());
        child.timeout();
        assert!(child.is_timed_out());
    }
}

#[cfg(test)]
mod scan_tests {
    use std::{future, ops::ControlFlow, pin::pin, ptr::NonNull};

    use super::{Error, InterruptMask, Redirect, Strand, frame};
    use crate::{
        gc::arena::{Header, Visit},
        object::tuple,
        test_support::with_vm,
        value::{Slot, Value},
    };

    /// Headers visited by scanning `strand`'s stack.
    fn scanned(strand: &Strand<'_, '_>) -> Vec<NonNull<Header>> {
        let mut out = Vec::new();
        let mut visit = |header| {
            out.push(header);
            ControlFlow::Continue(())
        };
        // Safety: the frame chain is live while the strand runs
        let _ = unsafe { strand.inner.scan_stack(&mut visit) };
        out
    }

    /// The header of `value`, if it is on the heap.
    fn heap_header(value: &Value<'_>) -> Option<NonNull<Header>> {
        let mut out = None;
        let _ = value.accept(&mut |header| {
            out = Some(header);
            ControlFlow::Continue(())
        });
        out
    }

    /// The header of a heap value.
    fn header(value: &Value<'_>) -> NonNull<Header> {
        heap_header(value).expect("heap value")
    }

    /// Take a fresh one-element tuple out of `slot`, leaving the slot nil.
    fn make_tuple<'v>(strand: &mut Strand<'v, '_>, mut slot: Slot<'v, '_>) -> Value<'v> {
        let item = Value::from_i64(strand, 1);
        tuple::create(strand, [item], Slot::reborrow(&mut slot));
        slot.take()
    }

    #[test]
    fn scan_frame_visits_its_data_only_while_entered() {
        with_vm(async |strand, [tmp]| {
            let held = make_tuple(strand, tmp);
            let target = header(&held);
            assert!(!scanned(strand).contains(&target));
            let visit = |visitor: &mut dyn Visit| held.accept(visitor);
            let scan = frame::Scan::new(Some(strand.fp), &visit);
            {
                // Safety: `held` owns one reference, which `visit` visits
                let scope = unsafe { strand.enter_scan(&scan) };
                assert!(scanned(&scope).contains(&target));
            }
            assert!(!scanned(strand).contains(&target));
        });
    }

    #[test]
    fn dropped_scopes_restore_strand_state() {
        with_vm(async |strand, [tmp]| {
            let sink = make_tuple(strand, tmp);
            let mask = strand.interrupt_mask();
            let output = heap_header(&strand.inner.output());
            let mut rooted = None;
            {
                let mut body = pin!(async {
                    with_interrupt_mask!(strand, InterruptMask::all(), {
                        with_slots!(strand, |mut inner| {
                            let item = Value::from_i64(strand, 2);
                            tuple::create(strand, [item], Slot::reborrow(&mut inner));
                            rooted = Some(header(&inner));
                            redirect!(strand, Redirect::new(strand).output(&sink), {
                                assert_eq!(strand.interrupt_mask(), InterruptMask::all());
                                assert_eq!(
                                    heap_header(&strand.inner.output()),
                                    Some(header(&sink))
                                );
                                future::pending::<()>().await;
                                Ok::<_, Error<'_, '_>>(())
                            })
                        })
                    })
                });
                assert!(futures::poll!(body.as_mut()).is_pending());
            }
            assert_eq!(strand.interrupt_mask(), mask);
            assert_eq!(heap_header(&strand.inner.output()), output);
            assert!(!scanned(strand).contains(&rooted.unwrap()));
        });
    }

    #[test]
    fn scan_frame_visits_across_await_and_nested_slots() {
        with_vm(async |strand, [tmp]| {
            let held = make_tuple(strand, tmp);
            let target = header(&held);
            let visit = |visitor: &mut dyn Visit| held.accept(visitor);
            let frame = frame::Scan::new(Some(strand.fp), &visit);
            {
                // Safety: `held` owns one reference, which `visit` visits; the scope is
                // dropped before `frame`
                let mut scope = unsafe { strand.enter_scan(&frame) };
                let strand = &mut *scope;
                with_slots!(strand, |mut inner| {
                    let item = Value::from_i64(strand, 2);
                    tuple::create(strand, [item], Slot::reborrow(&mut inner));
                    let slot_target = header(&inner);
                    futures::future::ready(()).await;
                    let seen = scanned(strand);
                    assert!(seen.contains(&target));
                    assert!(seen.contains(&slot_target));
                })
            }
            assert!(!scanned(strand).contains(&target));
        });
    }
}

#[cfg(test)]
mod constructor_tests {
    use crate::{
        call,
        error::Result,
        strand::Strand,
        sym::{self, Sym},
        test_support::with_vm,
        unpack,
        value::{Output, Slot, Value},
        vm::AllocExt,
    };

    fn text<'v>(strand: &Strand<'v, '_>, value: &Value<'v>) -> String {
        String::from(value.as_str(strand.vm()).expect("a string"))
    }

    #[test]
    fn tuple_keeps_item_order() {
        with_vm(async |strand, [mut out, mut item]| {
            strand.tuple(["a", "b", "c"], Slot::reborrow(&mut out));
            let tuple = out.as_tuple(strand).expect("a tuple");
            assert_eq!(tuple.len(), 3);
            for (index, expected) in ["a", "b", "c"].into_iter().enumerate() {
                assert!(tuple.get(strand, index, Slot::reborrow(&mut item)).unwrap());
                assert_eq!(text(strand, &item), expected);
            }
        });
    }

    #[test]
    #[should_panic(expected = "tuple items more than their length")]
    fn tuple_rejects_extra_items() {
        struct Lying(std::vec::IntoIter<i64>);
        impl Iterator for Lying {
            type Item = i64;
            fn next(&mut self) -> Option<i64> {
                self.0.next()
            }
        }
        impl ExactSizeIterator for Lying {
            fn len(&self) -> usize {
                1
            }
        }
        with_vm(async |strand, [out]| {
            strand.tuple(Lying(vec![1, 2].into_iter()), out);
        });
    }

    #[test]
    fn call_restores_a_moved_slot_argument() {
        with_vm(async |strand, [mut func, mut arg, mut result]| {
            strand.native_function(
                async |strand: &mut Strand<'_, '_>, args, out| -> Result<'_, '_, ()> {
                    let ([value], []) = unpack!(strand, args, 1, 0)?;
                    Output::set(strand, out, &value);
                    Ok(())
                },
                Slot::reborrow(&mut func),
            );
            Output::set(strand, Slot::reborrow(&mut arg), "kept");
            call!(strand, &func, &mut arg, Slot::reborrow(&mut result)).unwrap();
            assert_eq!(text(strand, &result), "kept");
            assert_eq!(text(strand, &arg), "kept");
        });
    }

    #[test]
    fn output_set_moves_a_slot_input() {
        with_vm(async |strand, [mut from, mut to]| {
            Output::set(strand, Slot::reborrow(&mut from), "moved");
            Output::set(strand, Slot::reborrow(&mut to), &mut from);
            assert_eq!(text(strand, &to), "moved");
            assert!(from.is_nil());
        });
    }

    #[test]
    fn bound_function_receives_its_bound_value() {
        with_vm(async |strand, [mut func, mut result]| {
            strand.bound_function(
                async |strand: &mut Strand<'_, '_>, bound: &Value<'_>, _args, out| {
                    Output::set(strand, out, bound);
                    Ok(())
                },
                "bound",
                Slot::reborrow(&mut func),
            );
            call!(strand, &func, Slot::reborrow(&mut result)).unwrap();
            assert_eq!(text(strand, &result), "bound");
        });
    }

    #[test]
    fn range_takes_both_ends() {
        with_vm(async |strand, [mut out, mut item]| {
            strand.range(1i64, 3i64, Slot::reborrow(&mut out));
            for (field, expected) in [(sym::START, 1i128), (sym::END, 3)] {
                out.get(strand, Sym::well_known(field), Slot::reborrow(&mut item))
                    .unwrap();
                assert_eq!(item.as_int(strand), Some(expected));
            }
        });
    }
}
