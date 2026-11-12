use std::{
    borrow::Cow,
    cell::{Cell, OnceCell, RefCell},
    future::Future,
    marker::PhantomData,
    mem::{self, MaybeUninit},
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
        strand::{Completion, Handle},
    },
    sym::{self, Sym},
    value::{Input, Output, Slot, Value, ValueCell},
    vm::{Alloc, Vm},
};

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
    /// it is never swapped by [`Strand::with_interrupt_token`], so canceling it
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
            input: ValueCell::new(strand, &strand.inner.input),
            output: ValueCell::new(strand, &strand.inner.output),
            errors: ErrorTable::new(),
            error_focus: Cell::new(None),
            locals: locals.into(),
            local_roots: strand
                .inner
                .local_roots
                .iter()
                .map(|cell| ValueCell::new(strand, cell))
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
    /// [`Strand::with_interrupt_token`].
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
        mem::replace(unsafe { self.input.slot() }.into_inner(), value)
    }

    /// Replace the strand's output, returning the old value for the caller to drop.
    #[must_use]
    pub(crate) fn replace_output(&self, value: Value<'v>) -> Value<'v> {
        // Safety: see the field
        mem::replace(unsafe { self.output.slot() }.into_inner(), value)
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
                        // Scan loaded program and upvars
                        frame.program.accept(visitor)?;
                        if let Some(ref upvars) = frame.upvars {
                            upvars.accept(visitor)?;
                        }
                        // Scan active slots
                        let sp = frame.sp.get();
                        for i in 0..sp {
                            frame.slots.get_unchecked(i).accept(visitor)?;
                        }
                        // Scan scratch slots
                        frame.scratch1.accept(visitor)?;
                        frame.scratch2.accept(visitor)?;
                        frame.scratch3.accept(visitor)?;
                        // Scan items
                        for (sym, value) in &*frame.items.get() {
                            if let Some(sym) = sym {
                                sym.accept(visitor)?;
                            }
                            value.accept(visitor)?;
                        }
                        ptr = frame.parent;
                    }
                    frame::Ptr::Slots(slots_ptr) => {
                        let slots = slots_ptr.as_ref();
                        let slice = &*slots.slots;
                        for cell in slice {
                            cell.accept(visitor)?;
                        }
                        ptr = slots.parent;
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
                    pc: frame.pc as u32,
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
            frame::Ptr::Slots(frame) => {
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
/// - When first polled, registers with the interrupt notifier
/// - On subsequent polls, checks if interruption has been requested
/// - When the future completes, unregisters from the interrupt notifier
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
        match unsafe { Pin::new_unchecked(&mut self.inner) }.poll(cx) {
            Poll::Ready(res) => {
                // Unregister from interrupt token if we were registered
                if let Some((interrupt, id)) = self.registration.take() {
                    interrupt.unregister(id);
                    self.strand.interrupt_registered.set(false);
                }
                Poll::Ready(res)
            }
            Poll::Pending => {
                if self.strand.sync_depth.get() != 0 {
                    return Poll::Ready(Err(Error::runtime_raw(
                        self.strand,
                        "attempt to suspend in sync context",
                    )));
                }
                // FIXME: probably should move interrupt mask testing into one check here to
                // avoid waking up and going to sleep uselessly
                let interrupt = self.strand.interrupt.borrow().clone();
                if self.registration.is_none() && !self.strand.interrupt_registered.get() {
                    // This strand is not registered with the interrupt token, so do it now
                    let mask = self.strand.interrupt_mask.get();
                    if let Some(id) = interrupt.register(cx.waker(), mask) {
                        self.registration = Some((interrupt, id));
                        self.strand.interrupt_registered.set(true);
                        // If the interrupt token is triggered, it will wake the waker so
                        // we can re-check interrupt status
                        return Poll::Pending;
                    } else {
                        return Poll::Ready(Err(self.strand.interrupt_error()));
                    }
                } else if interrupt.inner.pending(self.strand.interrupt_mask.get()) {
                    // Interrupt token was triggered and interruption is not masked
                    if let Some((interrupt, id)) = self.registration.take() {
                        interrupt.unregister(id);
                        self.strand.interrupt_registered.set(false);
                    }
                    return Poll::Ready(Err(self.strand.interrupt_error()));
                }
                Poll::Pending
            }
        }
    }
}

/// Execution context.
pub struct Strand<'v, 's> {
    pub(crate) inner: &'s StrandInner<'v>,
    pub(crate) fp: frame::Ptr<'v>,
    phantom: PhantomData<&'s mut &'s ()>,
}

impl<'v, 's> Strand<'v, 's> {
    pub(crate) unsafe fn from_frame(
        inner: &'s StrandInner<'v>,
        frame: &frame::CallFrame<'v>,
    ) -> Self {
        Self {
            inner,
            fp: frame::Ptr::Do(NonNull::from_ref(frame)),
            phantom: PhantomData,
        }
    }

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

    pub(crate) unsafe fn for_frame_infallible<R>(
        inner: &'s StrandInner<'v>,
        frame: &frame::CallFrame<'v>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        f(&mut unsafe { Self::from_frame(inner, frame) })
    }

    pub(crate) unsafe fn for_frame<R>(
        inner: &'s StrandInner<'v>,
        frame: &frame::CallFrame<'v>,
        f: impl FnOnce(&mut Self) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let _guard = unsafe { inner.push_sp(frame::Ptr::Do(NonNull::from_ref(frame))) };
        match f(&mut unsafe { Self::from_frame(inner, frame) }) {
            Ok(v) => Ok(v),
            Err(mut e) => {
                e.push_backtrace(
                    inner,
                    UnwindEntry::Do {
                        program: frame.program.clone(),
                        function_index: frame.func as u32,
                        pc: frame.pc as u32,
                    },
                );
                Err(e)
            }
        }
    }

    pub(crate) fn for_native_frame<R>(
        base: &mut Strand<'v, 's>,
        module: Cow<'v, str>,
        receiver: Cow<'v, str>,
        method: Option<Cow<'v, str>>,
        f: impl FnOnce(&mut Self) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let _depth_guard = base.inner.push_call_depth()?;
        let frame = Native {
            module,
            receiver,
            method,
            parent: Some(base.fp),
        };
        match f(&mut unsafe { Self::from_native_frame(base.inner, &frame) }) {
            Ok(v) => Ok(v),
            Err(mut e) => {
                e.push_backtrace(
                    base.inner,
                    UnwindEntry::Native {
                        module: frame.module.clone(),
                        receiver: frame.receiver.clone(),
                        method: frame.method.clone(),
                    },
                );
                Err(e)
            }
        }
    }

    pub(crate) async unsafe fn async_for_frame<R>(
        inner: &'s StrandInner<'v>,
        frame: &frame::CallFrame<'v>,
        f: impl AsyncFnOnce(&mut Self) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let _guard = unsafe { inner.push_sp(frame::Ptr::Do(NonNull::from_ref(frame))) };
        match f(&mut unsafe { Self::from_frame(inner, frame) }).await {
            Ok(v) => Ok(v),
            Err(mut e) => {
                e.push_backtrace(
                    inner,
                    UnwindEntry::Do {
                        program: frame.program.clone(),
                        function_index: frame.func as u32,
                        pc: frame.pc as u32,
                    },
                );
                Err(e)
            }
        }
    }

    pub(crate) async fn async_for_native_frame<R>(
        base: &mut Strand<'v, 's>,
        module: Cow<'v, str>,
        receiver: Cow<'v, str>,
        method: Option<Cow<'v, str>>,
        f: impl AsyncFnOnce(&mut Self) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let _depth_guard = base.inner.push_call_depth()?;
        let frame = Native {
            module,
            receiver,
            method,
            parent: Some(base.fp),
        };
        // Keep `sp` at the innermost frame so a suspended strand's backtrace
        // includes the native frame it's blocked in.  The GC skips native frames.
        let _sp_guard = unsafe {
            base.inner
                .push_sp(frame::Ptr::Native(NonNull::from_ref(&frame)))
        };
        match f(&mut unsafe { Self::from_native_frame(base.inner, &frame) }).await {
            Ok(v) => Ok(v),
            Err(mut e) => {
                e.push_backtrace(
                    base.inner,
                    UnwindEntry::Native {
                        module: frame.module.clone(),
                        receiver: frame.receiver.clone(),
                        method: frame.method.clone(),
                    },
                );
                Err(e)
            }
        }
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

    /// Call an async function with the requested number of scratch [`Slot`]s, usually inferred
    /// from the passed function.
    #[inline]
    pub async fn with_slots<const N: usize, R>(
        &mut self,
        f: impl for<'b> AsyncFnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R,
    ) -> R {
        let values: [ValueCell<'v>; N] = std::array::from_fn(|_| ValueCell::default());
        let mut slots = MaybeUninit::<[Slot<'v, '_>; N]>::uninit();

        unsafe {
            for (i, value) in values.iter().enumerate() {
                (slots.as_mut_ptr() as *mut Slot<'v, '_>)
                    .add(i)
                    .write(value.slot())
            }
            let frame = frame::Slots {
                parent: Some(self.fp),
                slots: values.as_slice(),
            };
            let _guard = self
                .inner
                .push_sp(frame::Ptr::Slots(NonNull::from_ref(&frame)));
            f(
                &mut Strand {
                    inner: self.inner,
                    fp: frame::Ptr::Slots(NonNull::from_ref(&frame)),
                    phantom: PhantomData,
                },
                slots.assume_init(),
            )
            .await
        }
    }

    /// Call an async function with a runtime-sized scratch [`Slots`] allocation.
    #[inline]
    pub(crate) async fn with_slots_dynamic<R>(
        &mut self,
        len: usize,
        f: impl for<'b> AsyncFnOnce(&mut Strand<'v, 's>, crate::value::Slots<'v, 'b>) -> R,
    ) -> R {
        let values: Vec<_> = (0..len).map(|_| ValueCell::default()).collect();

        unsafe {
            let frame = frame::Slots {
                parent: Some(self.fp),
                slots: values.as_slice(),
            };
            let _guard = self
                .inner
                .push_sp(frame::Ptr::Slots(NonNull::from_ref(&frame)));
            f(
                &mut Strand {
                    inner: self.inner,
                    fp: frame::Ptr::Slots(NonNull::from_ref(&frame)),
                    phantom: PhantomData,
                },
                crate::value::Slots::new(values.as_slice()),
            )
            .await
        }
    }

    /// Call a sync function with the requested number of scratch [`Slot`]s, usually inferred
    /// from the passed function.
    #[inline]
    pub fn with_slots_sync<const N: usize, R>(
        &mut self,
        f: impl for<'b> FnOnce(&mut Strand<'v, 's>, [Slot<'v, 'b>; N]) -> R,
    ) -> R {
        let values: [ValueCell<'v>; N] = std::array::from_fn(|_| ValueCell::default());
        let mut slots = MaybeUninit::<[Slot<'v, '_>; N]>::uninit();

        unsafe {
            for (i, value) in values.iter().enumerate() {
                (slots.as_mut_ptr() as *mut Slot<'v, '_>)
                    .add(i)
                    .write(value.slot())
            }
            let frame = frame::Slots {
                parent: Some(self.fp),
                slots: values.as_slice(),
            };
            let _guard = self
                .inner
                .push_sp(frame::Ptr::Slots(NonNull::from_ref(&frame)));
            f(
                &mut Strand {
                    inner: self.inner,
                    fp: frame::Ptr::Slots(NonNull::from_ref(&frame)),
                    phantom: PhantomData,
                },
                slots.assume_init(),
            )
        }
    }

    /// Run function with interrupt mask changed.
    ///
    /// ## Interrupt Semantics
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
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In a cleanup handler
    /// strand.with_interrupt_mask(InterruptMask::all(), async |strand| {
    ///     // This will complete even if the strand is interrupted
    ///     file.delete().await?;
    ///     conn.rollback().await?;
    /// }).await?;
    /// ```
    #[inline]
    pub async fn with_interrupt_mask<R>(
        &mut self,
        mask: InterruptMask,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
    ) -> R {
        let orig = self.inner.interrupt_mask.replace(mask);
        let res = f(self).await;
        self.inner.interrupt_mask.set(orig);
        res
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

    /// Run `f` one call level deeper.
    #[inline]
    pub async fn recursion_guard<R>(
        &mut self,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let _depth = self.inner.push_call_depth()?;
        self.pin_future_call(f).await
    }

    /// Run `f` one call level deeper, synchronous variant.
    #[inline]
    pub fn recursion_guard_sync<R>(
        &mut self,
        f: impl FnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let _depth = self.inner.push_call_depth()?;
        f(self)
    }

    /// Run `f` with `interrupt` as the strand's selected token.
    ///
    /// The selection is dynamically scoped: interrupt polling and futures
    /// registered while `f` runs use this token, and the prior token is
    /// restored when `f` finishes.
    pub async fn with_interrupt_token<R>(
        &mut self,
        interrupt: InterruptToken<'v>,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let orig = self.inner.interrupt.replace(interrupt);
        self.inner.interrupt_registered.set(false);
        let res = self.pin_future_call(f).await;
        self.inner.interrupt.replace(orig);
        self.inner.interrupt_registered.set(false);
        res
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

            strand
                .with_slots(
                    async |strand, [mut out, mut tmp, mut poison_err, mut poison_bt]| {
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
                                backtrace::create(
                                    strand,
                                    backtrace_entries.clone(),
                                    &mut poison_bt,
                                );
                                poison_err.store(value.dup());
                                true
                            }
                            Completion::Ok(_) => false,
                        };

                        // Store result in Handle via weak reference
                        if let Some(handle) = weak_handle.upgrade() {
                            handle
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
                                let _ = method!(strand, &tmp, close, &mut out).await;
                            }
                            tmp.store(strand.inner.replace_output(Value::NIL));
                            if !tmp.is_nil() {
                                let _ = if poisoned {
                                    method!(
                                        strand,
                                        &tmp,
                                        close,
                                        &mut out,
                                        &poison_err,
                                        backtrace_key: &poison_bt
                                    )
                                    .await
                                } else {
                                    method!(strand, &tmp, close, &mut out).await
                                };
                            }
                        }
                    },
                )
                .await
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

    /// Call function with strand-local iterator/sink redirected
    pub async fn enter<R>(
        self,
        f: impl for<'b> AsyncFnOnce(&'b mut Strand<'v, 's>) -> Result<'v, 's, R>,
    ) -> Result<'v, 's, R> {
        let strand = self.strand;
        let input = self.input.map(|input| strand.inner.replace_input(input));
        let output = self
            .output
            .map(|output| strand.inner.replace_output(output));
        let res = f(strand).await;
        if let Some(input) = input {
            drop(strand.inner.replace_input(input));
        }
        if let Some(output) = output {
            drop(strand.inner.replace_output(output));
        }
        res
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
