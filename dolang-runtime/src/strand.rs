use std::{
    borrow::Cow,
    cell::{Cell, RefCell, UnsafeCell},
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

use crate::{
    error::{Error, OwnedBacktraceIter, Result, UnwindEntry},
    frame::{self, FrameIter, Native},
    gc::arena::Visit,
    method,
    object::{
        BuiltinTypes, Singletons, backtrace,
        protocol::GcObj,
        strand::{Completion, Handle},
    },
    sym::{self, Sym},
    value::{Input, Output, Slot, Value},
    vm::{Alloc, Vm},
};

use crate::call;

pub(crate) type InterruptId = u64;

struct InterruptInner {
    canceled: Cell<bool>,
    timed_out: Cell<bool>,
    next_id: Cell<InterruptId>,
    wakers: RefCell<Vec<(InterruptId, Waker)>>,
    children: RefCell<Vec<Weak<InterruptInner>>>,
}

impl InterruptInner {
    fn cancel(&self) {
        self.canceled.set(true);
        self.wake();
    }

    fn timeout(&self) {
        self.timed_out.set(true);
        self.wake();
    }

    fn wake(&self) {
        for (_, waker) in mem::take(&mut *self.wakers.borrow_mut()).into_iter() {
            waker.wake()
        }
        for child in self.children.borrow().iter() {
            if let Some(child) = child.upgrade() {
                if self.canceled.get() {
                    child.cancel()
                }
                if self.timed_out.get() {
                    child.timeout()
                }
            }
        }
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

    pub(crate) fn register(&self, waker: &Waker) -> Option<InterruptId> {
        if self.inner.canceled.get() || self.inner.timed_out.get() {
            return None;
        }
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id.strict_add(1));
        self.inner.wakers.borrow_mut().push((id, waker.clone()));
        Some(id)
    }

    pub(crate) fn unregister(&self, id: InterruptId) {
        self.inner.wakers.borrow_mut().retain(|(i, _)| *i != id);
    }

    /// Creates a nested interrupt token. Parent cancellation/timeout propagates
    /// to all nested tokens created this way.
    pub fn nested(&self) -> Self {
        let child = Self::new();
        if self.inner.canceled.get() {
            child.inner.canceled.set(true);
        }
        if self.inner.timed_out.get() {
            child.inner.timed_out.set(true);
        }
        self.inner
            .children
            .borrow_mut()
            .push(Rc::downgrade(&child.inner));
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
        unsafe { Slot::new(&mut *strand.inner.local_roots.get_unchecked(self.index).get()) }
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

pub(crate) struct StrandMut<'v> {
    input: Value<'v>,
    output: Value<'v>,
    pub(crate) floating_roots: Vec<Option<Value<'v>>>,
    pub(crate) handled_backtrace: Option<Vec<UnwindEntry<'v>>>,
}

#[repr(C)]
pub(crate) struct StrandInner<'v> {
    group_link: Link<StrandInner<'static>>,
    vm: &'v Vm<'v>,
    mutable: RefCell<StrandMut<'v>>,
    locals: alias::Box<[NonNull<()>]>,
    local_roots: alias::Box<[UnsafeCell<Value<'v>>]>,
    arena: Arena,
    pub(crate) interrupt: RefCell<InterruptToken<'v>>,
    interrupt_registered: Cell<bool>,
    interrupt_mask: Cell<bool>,
    // Nested synchronous calls depth
    sync_depth: Cell<u32>,
    // Logical callable frame depth (Do + native frames only)
    call_depth: Cell<u32>,
    sp: Cell<Option<frame::Ptr<'v>>>,
    pub(crate) start: Value<'v>,
    is_leader: Cell<bool>,
    leader: Cell<*const StrandGroup>,
}

struct SpGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
    prior: Option<frame::Ptr<'v>>,
}

pub(crate) struct CallDepthGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
}

pub(crate) struct HandledBacktraceGuard<'v, 'a> {
    inner: &'a StrandInner<'v>,
    prior: Option<Vec<UnwindEntry<'v>>>,
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

impl<'v, 'a> Drop for HandledBacktraceGuard<'v, 'a> {
    fn drop(&mut self) {
        self.inner.mutable.borrow_mut().handled_backtrace = self.prior.take();
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
        if !self.leader.get().is_null() {
            unsafe { self.group_link.remove() }
        }
        for (vtbl, local) in self.vm.locals.iter().zip(self.locals.iter()) {
            unsafe { (vtbl.drop)(*local) }
        }
    }
}

const ARENA_DEFAULT_SIZE: usize = 1024 * 16;
#[cfg(debug_assertions)]
const MAX_CALL_DEPTH: u32 = 64;
#[cfg(not(debug_assertions))]
const MAX_CALL_DEPTH: u32 = 1000;

impl<'v> StrandInner<'v> {
    pub(crate) fn interrupt_error(&self) -> Error<'v, '_> {
        let interrupt = self.interrupt.borrow();
        if interrupt.is_canceled() {
            Error::canceled_raw(self)
        } else if interrupt.is_timed_out() {
            Error::timed_out_raw(self)
        } else {
            unreachable!("interrupt_error without pending interrupt")
        }
    }

    pub(crate) fn new(vm: &'v Vm<'v>, interrupt: Option<InterruptToken<'v>>) -> Self {
        Self {
            group_link: Link::new(),
            vm,
            mutable: RefCell::new(StrandMut {
                input: Value::NIL,
                output: Value::NIL,
                floating_roots: Vec::new(),
                handled_backtrace: None,
            }),
            locals: vm
                .locals
                .iter()
                .map(|vtbl| (vtbl.init)())
                .collect::<Vec<_>>()
                .into(),
            local_roots: (0..vm.local_root_count)
                .map(|_| UnsafeCell::new(Value::NIL))
                .collect::<Vec<_>>()
                .into(),
            arena: Arena::new(ARENA_DEFAULT_SIZE),
            interrupt: RefCell::new(interrupt.unwrap_or_else(InterruptToken::new)),
            interrupt_registered: Cell::new(false),
            interrupt_mask: Cell::new(false),
            call_depth: Cell::new(0),
            sp: Cell::new(None),
            start: Value::NIL,
            is_leader: Cell::new(false),
            leader: Cell::new(std::ptr::null()),
            sync_depth: Cell::new(0),
        }
    }

    unsafe fn push_sp(&self, sp: frame::Ptr<'v>) -> SpGuard<'v, '_> {
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
        let borrow = strand.inner.mutable.borrow();
        Self {
            group_link: Link::new(),
            vm: strand.inner.vm,
            mutable: RefCell::new(StrandMut {
                input: borrow.input.dup(),
                output: borrow.output.dup(),
                floating_roots: Vec::new(),
                handled_backtrace: None,
            }),
            locals: locals.into(),
            local_roots: strand
                .inner
                .local_roots
                .iter()
                .map(|cell| UnsafeCell::new(unsafe { &*cell.get() }.dup()))
                .collect::<Vec<_>>()
                .into(),
            arena: Arena::new(ARENA_DEFAULT_SIZE),
            interrupt: RefCell::new(
                interrupt.unwrap_or_else(|| strand.inner.interrupt.borrow().clone()),
            ),
            interrupt_registered: Cell::new(false),
            interrupt_mask: Cell::new(false),
            call_depth: Cell::new(strand.inner.call_depth.get()),
            sp: Cell::new(None),
            start: Value::NIL,
            is_leader: Cell::new(false),
            leader: Cell::new(null()),
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
        unsafe {
            (*leader)
                .descendants
                .push_front(NonNull::from_ref(self).cast());
        }
    }

    pub(crate) fn vm(&self) -> &'v Vm<'v> {
        self.vm
    }

    pub(crate) fn input(&self) -> Value<'v> {
        self.mutable.borrow().input.dup()
    }

    pub(crate) fn output(&self) -> Value<'v> {
        self.mutable.borrow().output.dup()
    }

    pub(crate) fn alloc_floating_root(&self, value: Value<'v>) -> usize {
        let mut m = self.mutable.borrow_mut();
        if let Some(idx) = m.floating_roots.iter().position(|s| s.is_none()) {
            m.floating_roots[idx] = Some(value);
            idx
        } else {
            let idx = m.floating_roots.len();
            m.floating_roots.push(Some(value));
            idx
        }
    }

    pub(crate) fn mutable_ptr(&self) -> NonNull<RefCell<StrandMut<'v>>> {
        NonNull::from_ref(&self.mutable)
    }

    pub(crate) fn handled_backtrace(&self) -> Option<Vec<UnwindEntry<'v>>> {
        self.mutable.borrow().handled_backtrace.clone()
    }

    pub(crate) fn push_handled_backtrace(
        &self,
        backtrace: Vec<UnwindEntry<'v>>,
    ) -> HandledBacktraceGuard<'v, '_> {
        let prior = self
            .mutable
            .borrow_mut()
            .handled_backtrace
            .replace(backtrace);
        HandledBacktraceGuard { inner: self, prior }
    }

    /// Walk all GC-reachable values held by this strand's stack and mutable state.
    ///
    /// # Safety
    /// The frame chain from `self.sp` must be valid (all pointers must be live).
    pub(crate) unsafe fn scan_stack(&self, visitor: &mut dyn Visit) -> ControlFlow<()> {
        unsafe {
            // Scan start value
            self.start.accept(visitor)?;

            // Scan input/output and floating roots
            {
                let m = self.mutable.borrow();
                m.input.accept(visitor)?;
                m.output.accept(visitor)?;
                for val in m.floating_roots.iter().flatten() {
                    val.accept(visitor)?;
                }
            }

            // Scan local roots
            for cell in self.local_roots.iter() {
                (*cell.get()).accept(visitor)?;
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
                            (*frame.slots.get_unchecked(i).get()).accept(visitor)?;
                        }
                        // Scan scratch slots
                        (*frame.scratch1.get()).accept(visitor)?;
                        (*frame.scratch2.get()).accept(visitor)?;
                        (*frame.scratch3.get()).accept(visitor)?;
                        // Scan items
                        for (sym, value) in &*frame.items.get() {
                            if let Some(sym) = sym {
                                sym.accept(visitor)?;
                            }
                            (*value.get()).accept(visitor)?;
                        }
                        ptr = frame.parent;
                    }
                    frame::Ptr::Slots(slots_ptr) => {
                        let slots = slots_ptr.as_ref();
                        let slice = &*slots.slots;
                        for cell in slice {
                            (*cell.get()).accept(visitor)?;
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
/// If `interrupt_mask` is set on the strand, interruption checks are skipped
/// and the future runs to completion normally. This is used for cleanup
/// operations that must complete even during interruption.
pub(crate) struct Pinned<'v, 's, 'a, R> {
    inner: dolang_util::pin::Pinned<'a, Result<'v, 's, R>>,
    strand: &'s StrandInner<'v>,
    interrupt: InterruptToken<'v>,
    /// Interrupt registration ID. None if not yet registered or already cleaned up.
    id: Option<InterruptId>,
}

impl<'v, 's, 'a, R> Future for Pinned<'v, 's, 'a, R> {
    type Output = Result<'v, 's, R>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match unsafe { Pin::new_unchecked(&mut self.inner) }.poll(cx) {
            Poll::Ready(res) => {
                // Unregister from interrupt token if we were registered
                if let Some(id) = self.id.take() {
                    self.interrupt.unregister(id);
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
                if self.id.is_none() && !self.strand.interrupt_registered.get() {
                    // This strand is not registered with the interrupt token, so do it now
                    if let Some(id) = self.interrupt.register(cx.waker()) {
                        self.id = Some(id);
                        self.strand.interrupt_registered.set(true);
                        // If the interrupt token is triggered, it will wake the waker so
                        // we can re-check interrupt status
                        return Poll::Pending;
                    } else if !self.strand.interrupt_mask.get() {
                        return Poll::Ready(Err(self.strand.interrupt_error()));
                    }
                } else if !self.strand.interrupt_mask.get()
                    && (self.interrupt.is_canceled() || self.interrupt.is_timed_out())
                {
                    // Interrupt token was triggered and interruption is not masked
                    if let Some(id) = &self.id {
                        self.interrupt.unregister(*id);
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
                        loaded_id: frame.program.id,
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
                        loaded_id: frame.program.id,
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
            .handled_backtrace()
            .map(|entries| OwnedBacktraceIter::new(self.inner.vm(), entries))
    }

    pub(crate) fn backtrace_entries(&self) -> Vec<UnwindEntry<'v>> {
        let mut head = Some(self.fp);
        let mut out = Vec::new();
        while let Some(ptr) = head {
            match ptr {
                frame::Ptr::Do(frame) => {
                    let frame = unsafe { frame.as_ref() };
                    out.push(UnwindEntry::Do {
                        loaded_id: frame.program.id,
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
        let mut values = [const { UnsafeCell::new(Value::NIL) }; N];
        let mut slots = MaybeUninit::<[Slot<'v, '_>; N]>::uninit();

        unsafe {
            for (i, value) in values.iter_mut().enumerate() {
                (slots.as_mut_ptr() as *mut Slot<'v, '_>)
                    .add(i)
                    .write(Slot::new(&mut *value.get()))
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
        let values: Vec<_> = (0..len).map(|_| UnsafeCell::new(Value::NIL)).collect();

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
        let mut values = [const { UnsafeCell::new(Value::NIL) }; N];
        let mut slots = MaybeUninit::<[Slot<'v, '_>; N]>::uninit();

        unsafe {
            for (i, value) in values.iter_mut().enumerate() {
                (slots.as_mut_ptr() as *mut Slot<'v, '_>)
                    .add(i)
                    .write(Slot::new(&mut *value.get()))
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
    /// When `mask` is `true`, pending interruption is temporarily suppressed:
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
    /// strand.with_interrupt_mask(true, async |strand| {
    ///     // This will complete even if the strand is interrupted
    ///     file.delete().await?;
    ///     conn.rollback().await?;
    /// }).await?;
    /// ```
    #[inline]
    pub async fn with_interrupt_mask<R>(
        &mut self,
        mask: bool,
        f: impl AsyncFnOnce(&mut Strand<'v, 's>) -> R,
    ) -> R {
        let orig = self.inner.interrupt_mask.replace(mask);
        let res = f(self).await;
        self.inner.interrupt_mask.set(orig);
        res
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
    /// otherwise it will inherit [`Self::interrupt_token`].
    pub async fn spawn_scoped<R>(
        &self,
        interrupt: Option<InterruptToken<'v>>,
        f: impl for<'ss> AsyncFnOnce(&mut Strand<'v, 'ss>) -> Result<'v, 'ss, R>,
    ) -> Result<'v, 's, R> {
        let strand = StrandInner::derived(self, interrupt, InheritKind::Scoped);
        // Safety: strand is at a stable address (async fn generator state is heap-allocated)
        // and the leader's StrandGroup outlives this scoped strand.
        unsafe { strand.init_group_member(self.inner) };
        let native = frame::Native {
            module: Cow::Borrowed("strand"),
            receiver: Cow::Borrowed("<scoped>"),
            method: None,
            parent: Some(self.fp),
        };
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

    /// Spawn a background strand that calls `callable` and returns a `Handle`.
    ///
    /// The background strand runs independently of the spawning strand and is managed
    /// by the VM's event loop. The returned `Handle` can be used to join, cancel,
    /// or check completion of the background strand.
    ///
    /// If `stream` is `Some((strand_input, strand_output))`, the strand's input and output
    /// iterators are set to those values before the callable runs, and both are closed when
    /// the callable returns (regardless of success or failure).
    ///
    /// Returns an error if the spawn channel is not available (e.g. the VM is shutting down).
    pub(crate) fn spawn_background_raw(
        &mut self,
        callable: Value<'v>,
        interrupt: InterruptToken<'v>,
        stream: Option<(Value<'v>, Value<'v>)>,
    ) -> Result<'v, 's, GcObj<'v, Handle<'v>>> {
        let close_on_exit = stream.is_some();

        // Create StrandInner
        let mut inner =
            StrandInner::derived(self, Some(interrupt.clone()), InheritKind::Background);
        inner.call_depth.set(0);
        inner.start = callable;

        // Set strand-side I/O if this is a stream strand
        if let Some((recv, send)) = stream {
            let mut m = inner.mutable.borrow_mut();
            m.input = recv;
            m.output = send;
        }

        let inner = Rc::new(inner);
        let vm = self.vm();

        // Create JoinHandle GcObj
        let handle = GcObj::new(
            vm.arena(),
            vm.builtin_types().strand_handle,
            Handle::new(inner.clone(), interrupt),
        );

        // Create weak ref for the future (doesn't participate in cycles)
        let weak_handle = GcObj::downgrade(&handle);

        // Create the background strand future
        let future = Box::pin(async move {
            let native = frame::Native {
                module: Cow::Borrowed("strand"),
                receiver: Cow::Borrowed("spawn"),
                method: None,
                parent: None,
            };
            // Initialize as group leader
            let group = StrandGroup::new();
            let _guard = unsafe { inner.init_group_leader(&group) };
            // Safety: native outlives strand
            let mut strand = unsafe { Strand::from_native_frame(&inner, &native) };

            strand
                .with_slots(async |strand, [mut out, mut tmp]| {
                    let result = match call!(strand, &inner.start, &mut out).await {
                        Ok(()) => Completion::Ok(out.take()),
                        Err(e) => Completion::Err(e.into_pair(strand)),
                    };
                    let poison = match &result {
                        Completion::Err((value, backtrace_entries)) => {
                            backtrace::create(strand, backtrace_entries.clone(), &mut out);
                            Some((value.dup(), out.take()))
                        }
                        Completion::Ok(_) => None,
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
                        tmp.store(strand.inner.mutable.borrow_mut().input.take());
                        if !tmp.is_nil() {
                            let _ =
                                method!(strand, &tmp, close, &mut out).await;
                        }
                        tmp.store(strand.inner.mutable.borrow_mut().output.take());
                        if !tmp.is_nil() {
                            let _ = if let Some((err, backtrace)) = &poison {
                                method!(strand, &tmp, close, &mut out, err, backtrace_key: backtrace)
                                    .await
                            } else {
                                method!(strand, &tmp, close, &mut out).await
                            };
                        }
                    }
                })
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
        let interrupt = self.interrupt_token();
        Pinned {
            inner: unsafe { inner.arena.pin_future_unchecked(f(self)) },
            strand: inner,
            interrupt,
            id: None,
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
    fn alloc_vm(&mut self, _: crate::vm::private::Sealed) -> &Vm<'v> {
        self
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
        let (input, output) = {
            let mut borrow = strand.inner.mutable.borrow_mut();
            (
                self.input.map(|mut input| {
                    mem::swap(&mut borrow.input, &mut input);
                    input
                }),
                self.output.map(|mut output| {
                    mem::swap(&mut borrow.output, &mut output);
                    output
                }),
            )
        };
        let res = f(strand).await;
        let mut borrow = strand.inner.mutable.borrow_mut();
        if let Some(input) = input {
            borrow.input = input;
        }
        if let Some(output) = output {
            borrow.output = output;
        }
        res
    }
}
