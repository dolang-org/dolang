use std::{cell::Cell, marker::PhantomData, ops::ControlFlow, ptr::NonNull};

use dolang_util::{alias, ring, ring::Link};

/// # Safety
/// Only implement this for a type `T` if `U` is a castable prefix of it in memory
/// (`T` begins with a `U` field and both are `#[repr(C)]`, and the same holds for the
/// types' respective vtbls
pub(crate) unsafe trait Upcast<U> {}

pub(crate) trait Visit {
    fn visit(&mut self, raw: NonNull<Header>) -> ControlFlow<()>;
}

impl<F: FnMut(NonNull<Header>) -> ControlFlow<()>> Visit for F {
    fn visit(&mut self, raw: NonNull<Header>) -> ControlFlow<()> {
        self(raw)
    }
}

/// Base vtbl for a GC allocation
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Vtbl {
    pub(crate) name: fn() -> &'static str,
    pub(crate) cyclic: bool,
    pub(crate) immutable: bool,
    pub(crate) is_strand: bool,
    pub(crate) drop: unsafe fn(NonNull<Header>),
    pub(crate) dealloc: unsafe fn(NonNull<Header>) -> usize,
    pub(crate) trace: unsafe fn(NonNull<Header>, &mut dyn Visit) -> ControlFlow<()>,
    pub(crate) clear: unsafe fn(NonNull<Header>),
}

unsafe impl Upcast<Vtbl> for Vtbl {}

type BorrowCount = u32;
const BORROW_MUT: BorrowCount = 1;
const BORROW_INC: BorrowCount = 2;

/// Header for a GC allocation
///
/// ## Memory Layout
///
/// Each GC-managed object has a header containing metadata and link fields for
/// collection. The header is a prefix of the overall object.
///
/// ## Reference Counting
///
/// - `strong`: Number of strong references; destroy object contents (e.g. Drop impls) at 0
/// - `weak`: Number of weak references + 1 (held collectively by strong count); dealloc at 0
/// - `trial`: Temporary reference count used during cycle collection
///
/// ## Collection Lists
///
/// - `cyclic`: Links all potentially-cyclic objects into the arena's cyclic ring
/// - `queue`: Temporary list during collection (alive/trash/join_handle_trash)
#[repr(C)]
pub(crate) struct Header {
    // FIXME: maybe move this into a thread-local
    pub(super) arena: NonNull<ArenaInner>,
    pub(super) vtbl: NonNull<Vtbl>,
    /// Strong reference count. When this reaches 0, the object is released.
    pub(super) strong: Cell<usize>,
    /// Weak reference count plus one implicit reference held in common by strong
    /// references. When this reaches 0, the memory is deallocated.
    pub(super) weak: Cell<usize>,
    /// Borrow tracking: 0 = not borrowed, 1 = mutably borrowed,
    /// 2+ = count of immutable borrows (actual count is borrow / 2)
    pub(super) borrow: Cell<BorrowCount>,
    /// Trial reference count used during cycle collection.
    /// Copy of strong count in Pass 1, decremented for internal refs in Pass 2.
    /// If 0 after Pass 2, object is candidate for collection.
    // FIXME: union this with one of the cyclic list links
    pub(super) trial: Cell<usize>,
    /// Links this object into the arena's ring of potentially-cyclic objects.
    /// Objects are added here when their first strong reference is created.
    // FIXME: it's more efficient to only place objects here when their strong
    // count has been decremented (but not to 0) since the last cycle collection.
    // This will require separate tracking of strand handles so they can still be
    // globally enumerated during VM shutdown.
    pub(super) cyclic: Link<Header>,
    /// Temporary queue link used during collection to build alive/trash lists.
    pub(super) queue: Link<Header>,
}

impl Header {
    /// Construct header for the given class.
    ///
    /// # Safety
    /// The vtbl must be correct for the ultimate object and its allocation
    pub(crate) unsafe fn new<'v, V: Upcast<Vtbl>>(arena: &Arena<'v>, vtbl: NonNull<V>) -> Self {
        Self {
            arena: NonNull::from_ref(&arena.0),
            vtbl: vtbl.cast(),
            strong: Cell::new(1),
            weak: Cell::new(1),
            trial: Cell::new(0),
            borrow: Cell::new(0),
            cyclic: Link::new(),
            queue: Link::new(),
        }
    }

    pub(crate) fn vtbl(&self) -> NonNull<Vtbl> {
        self.vtbl
    }

    /// # Safety
    /// This is an unchecked downcast (unless `V` is `Vtbl`)
    pub(crate) unsafe fn vtbl_downcast_unchecked<V: Upcast<Vtbl>>(&self) -> &V {
        unsafe { self.vtbl.cast().as_ref() }
    }

    pub(super) unsafe fn release(&self) -> bool {
        let strong = self.strong.get();
        assert!(strong > 0);
        if strong == usize::MAX {
            return false;
        }
        self.strong.set(strong - 1);
        strong == 1
    }

    pub(super) unsafe fn release_weak(&self) -> bool {
        let weak = self.weak.get();
        assert!(weak > 0);
        if weak == usize::MAX {
            return false;
        }
        self.weak.set(weak - 1);
        weak == 1
    }

    pub(crate) fn retain(&self) {
        let strong = self.strong.get();
        assert!(strong > 0);
        self.strong.set(strong.saturating_add(1));
    }

    pub(crate) fn retain_weak(&self) {
        let weak = self.weak.get();
        assert!(weak > 0);
        self.weak.set(weak.saturating_add(1));
    }

    pub(crate) fn try_retain(&self) -> bool {
        let strong = self.strong.get();
        if strong != 0 {
            self.strong.set(strong.saturating_add(1));
            true
        } else {
            false
        }
    }

    pub(crate) fn strong_count(&self) -> usize {
        self.strong.get()
    }

    pub(super) unsafe fn released_weak(this: NonNull<Self>) {
        unsafe {
            let arena = this.as_ref().arena;
            let dealloc = this.as_ref().vtbl.as_ref().dealloc;
            let freed = dealloc(this);
            arena.as_ref().balance.update(|b| b - 1);
            arena
                .as_ref()
                .adjust_allocated(0isize.strict_sub_unsigned(freed));
        }
    }

    #[must_use]
    pub(super) fn borrow_imm(&self) -> bool {
        if self.borrow.get() == BORROW_MUT {
            return false;
        }
        self.borrow.update(|b| b.strict_add(BORROW_INC));
        true
    }

    pub(super) fn unborrow_imm(&self) {
        assert_ne!(self.borrow.get(), BORROW_MUT);
        assert_ne!(self.borrow.get(), 0);
        self.borrow.update(|b| b - BORROW_INC)
    }

    #[must_use]
    pub(super) fn borrow_mut(&self) -> bool {
        if self.borrow.get() != 0 {
            return false;
        }
        if unsafe { self.vtbl.as_ref().immutable } {
            panic!("mutable borrow of immutable GC object")
        }
        self.borrow.set(BORROW_MUT);
        true
    }

    pub(super) fn unborrow_mut(&self) {
        assert!(self.borrow.get() == BORROW_MUT);
        self.borrow.set(0)
    }

    /// Release a GC object after its strong count reached zero.
    ///
    /// ## Safety
    ///
    /// - `this` must point to a valid Header
    /// - The strong count must have just been decremented to 0
    /// - No references to this object exist (weak refs are ok)
    ///
    /// ## Process
    ///
    /// 1. Remove from cycle collector rings (if possibly cyclic)
    /// 2. Drop the object contents
    /// 3. Release the implicit weak reference
    /// 4. If weak count also reached 0, deallocate memory
    pub(super) unsafe fn released(this: NonNull<Self>) {
        unsafe {
            let vtbl = (*this.as_ptr()).vtbl;
            let cyclic = (*vtbl.as_ptr()).cyclic;
            if cyclic {
                (*this.as_ptr()).cyclic.remove();
                (*this.as_ptr()).queue.remove();
            }
            let drop = (*vtbl.as_ptr()).drop;
            drop(this);
            if this.as_ref().release_weak() {
                Self::released_weak(this);
            }
        }
    }
}

unsafe impl Upcast<Header> for Header {}

// Really put the cycle collector through the ringer during miri testing
#[cfg(miri)]
const COLLECT_THRESHOLD: isize = 10;
#[cfg(not(miri))]
const COLLECT_THRESHOLD: isize = 10000;

pub(super) struct ArenaInner {
    pub(super) cyclic: ring!(Header, cyclic),
    pub(super) balance: Cell<isize>,
    pub(super) allocated: Cell<usize>,
}

pub(crate) struct Arena<'v>(
    pub(super) alias::Box<ArenaInner>,
    PhantomData<&'v mut &'v ()>,
);

impl ArenaInner {
    pub(crate) fn clear(&self) {
        #[cfg(feature = "debug")]
        eprintln!("GC CLEAR: begin");

        unsafe {
            while let Some(item) = self.cyclic.pop_front() {
                let vtbl = item.as_ref().vtbl;
                #[cfg(feature = "debug")]
                eprintln!("GC DROP: trash: {}@{:?}", (vtbl.as_ref().name)(), item);
                // Give item an extra reference so it doesn't get deleted during
                // clear routine
                if item.as_ref().borrow.get() != 0 {
                    panic!("GC object has unexpected outstanding borrow")
                }
                item.as_ref().retain();
                (vtbl.as_ref().clear)(item);
                // Drop extra reference
                if item.as_ref().release() {
                    Header::released(item)
                }
            }
        }

        #[cfg(feature = "debug")]
        eprintln!("GC CLEAR: end");
    }

    #[inline]
    pub(crate) fn adjust_allocated(&self, delta: isize) {
        self.allocated
            .update(|value| value.strict_add_signed(delta));
    }
}

impl<'v> Arena<'v> {
    pub(crate) fn new() -> Self {
        let this = alias::Box::new(ArenaInner {
            cyclic: Default::default(),
            balance: Cell::new(0),
            allocated: Cell::new(0),
        });
        this.cyclic.init();
        Self(this, PhantomData)
    }

    #[inline]
    pub(crate) fn collect(&self) -> bool {
        if self.0.balance.get() >= COLLECT_THRESHOLD {
            self.collect_full();
            true
        } else {
            false
        }
    }

    /// ## Algorithm Overview
    ///
    /// This implements a 5-pass trial deletion algorithm to identify garbage cycles.
    /// It is a naive/simplified version of the Bacon-Rajan algorithm.
    ///
    /// **Pass 1**: Initialize trial reference counts  
    /// Copy the actual reference count to the trial field for each potentially-cyclic object.
    /// Abort collection if any object has an outstanding mutable borrow.  This should
    /// hopefully be very rare, as most native object implementations go to great lengths
    /// to avoid holding mutable borrows across potential GC points, but it is unavoidable
    /// in some cases (e.g. if an object's equality vtbl op allocates during dict insertion).
    ///
    /// **Pass 2**: Subtract internal references
    /// For each cyclic object, decrement the trial count of its cyclic children.
    /// After this pass, trial count == external references for each object.
    ///
    /// **Pass 3**: Partition into alive/trash candidates  
    /// Objects with trial count > 0 are definitely alive (have external refs).  
    /// Objects with trial count == 0 are potential garbage.  
    /// Join handles are tracked separately to allow special handling.
    ///
    /// **Pass 4**: Propagate liveness  
    /// Objects marked alive may reference other cyclic objects. We trace from
    /// definitely-alive objects and mark all reachable cyclic objects as alive.
    ///
    /// **Pass 4.5**: Rescue join-handle children  
    /// Join handles (strands) are canceled and allowed to unwind so they have a chance to perform
    /// cleanup (e.g. pending finally blocks).  This is essentially a form of finalization.  Since
    /// the program can't be expected to do this when objects it references have been mysteriously
    /// cleared to `nil`, transitive children of strands must be rescued from the trash list.
    ///
    /// **Pass 5**: Clear trash  
    /// Any object still in the trash list is garbage. Clear each object's
    /// children to break cycles and allow reference counts to reach 0.
    pub(crate) fn collect_full(&self) {
        let this = &*self.0;
        #[cfg(feature = "debug")]
        eprintln!("COLLECT: begin");
        let trash: ring!(Header, queue) = Default::default();
        let alive: ring!(Header, queue) = Default::default();
        let jh_trash: ring!(Header, queue) = Default::default();
        trash.init();
        alive.init();
        jh_trash.init();

        unsafe {
            // Pass 1: set trial reference count, check for borrow
            for item in this.cyclic.iter() {
                #[cfg(feature = "debug")]
                eprintln!(
                    "COLLECT: possible cyclic: {}@{:?}",
                    (item.as_ref().vtbl.as_ref().name)(),
                    item
                );
                if item.as_ref().borrow.get() == BORROW_MUT {
                    #[cfg(feature = "debug")]
                    eprintln!(
                        "COLLECT: deferred due to oustanding mutable borrow: {}@{:?}",
                        (item.as_ref().vtbl.as_ref().name)(),
                        item
                    );
                    return;
                }
                item.as_ref().trial.set(item.as_ref().strong.get());
            }
            // Pass 2: remove trial references for children
            for item in this.cyclic.iter() {
                let vtbl = item.as_ref().vtbl;
                let _ = (vtbl.as_ref().trace)(item, &mut |child: NonNull<Header>| {
                    if child.as_ref().vtbl.as_ref().cyclic {
                        child.as_ref().trial.update(|r| r - 1)
                    }
                    ControlFlow::Continue(())
                });
            }
            // Pass 3: sort items into definitely alive, join_handle trash, and
            // (possibly) trash
            for item in this.cyclic.iter() {
                let count = item.as_ref().trial.get();
                #[cfg(feature = "debug")]
                eprintln!(
                    "COLLECT: trial count: {}@{:?}: {count}",
                    (item.as_ref().vtbl.as_ref().name)(),
                    item
                );
                if count == 0 {
                    if item.as_ref().vtbl.as_ref().is_strand {
                        jh_trash.push_front(item);
                    } else {
                        trash.push_front(item);
                    }
                } else {
                    alive.push_front(item);
                }
            }
            // Pass 4: propagate liveness to children
            while let Some(item) = alive.pop_front() {
                let vtbl = item.as_ref().vtbl;
                #[cfg(feature = "debug")]
                eprintln!("COLLECT: living: {}@{:?}", (vtbl.as_ref().name)(), item);
                let _ = (vtbl.as_ref().trace)(item, &mut |child: NonNull<Header>| {
                    if child.as_ref().vtbl.as_ref().cyclic && child.as_ref().trial.get() == 0 {
                        child.as_ref().trial.set(1);
                        child.as_ref().queue.remove();
                        alive.push_front(child);
                    }
                    ControlFlow::Continue(())
                });
            }
            // Pass 4.5: rescue non-join-handle children of join-handle trash,
            // then move join-handle trash into the main trash list
            for item in jh_trash.iter() {
                let vtbl = item.as_ref().vtbl;
                #[cfg(feature = "debug")]
                eprintln!(
                    "COLLECT: join_handle trash: {}@{:?}",
                    (vtbl.as_ref().name)(),
                    item
                );
                let _ = (vtbl.as_ref().trace)(item, &mut |child: NonNull<Header>| {
                    if child.as_ref().vtbl.as_ref().cyclic
                        && child.as_ref().trial.get() == 0
                        && !child.as_ref().vtbl.as_ref().is_strand
                    {
                        child.as_ref().trial.set(1);
                        child.as_ref().queue.remove();
                        alive.push_front(child);
                    }
                    ControlFlow::Continue(())
                });
            }
            // Propagate liveness from rescued children
            while let Some(item) = alive.pop_front() {
                let vtbl = item.as_ref().vtbl;
                #[cfg(feature = "debug")]
                eprintln!(
                    "COLLECT: living (rescued): {}@{:?}",
                    (vtbl.as_ref().name)(),
                    item
                );
                let _ = (vtbl.as_ref().trace)(item, &mut |child: NonNull<Header>| {
                    let vtbl = child.as_ref().vtbl;
                    if vtbl.as_ref().cyclic
                        && child.as_ref().trial.get() == 0
                        && !vtbl.as_ref().is_strand
                    {
                        child.as_ref().trial.set(1);
                        child.as_ref().queue.remove();
                        alive.push_front(child);
                    }
                    ControlFlow::Continue(())
                });
            }
            // Move join-handle trash into main trash list for clearing
            jh_trash.migrate(&trash);

            // Pass 5: clear anything that's still trash
            while let Some(item) = trash.pop_front() {
                let vtbl = item.as_ref().vtbl;
                if item.as_ref().borrow.get() != 0 {
                    // This should really be impossible: if something has an outstanding
                    // borrow, it should also have an outstanding strong reference from outside
                    // the cycle and should never have made it here.  However,
                    // strands could potentially cross an await point with an outstanding borrow
                    // (although this is strongly not recommended), so we have to back off in
                    // this case.
                    // FIXME: resolve this by allowing certain types to mark themselves as
                    // clearable even with outstanding borrows; `ObjectInner` could do this, for
                    // example, and strand handles could be made to do so as well.  This requires
                    // moving the "annex" feature of `Object` into the GC proper so that there is
                    // an immutable subset of the object state that can always be accessed
                    // independently of the main (runtime borrowable) state.
                    #[cfg(feature = "debug")]
                    eprintln!(
                        "COLLECT: clear deferred due to oustanding borrow: {}@{:?}",
                        (item.as_ref().vtbl.as_ref().name)(),
                        item
                    );
                    continue;
                }
                #[cfg(feature = "debug")]
                eprintln!("COLLECT: trash: {}@{:?}", (vtbl.as_ref().name)(), item);
                // Give item an extra reference so it doesn't get deleted during
                // clear routine
                item.as_ref().retain();
                (vtbl.as_ref().clear)(item);
                // Drop extra reference
                if item.as_ref().release() {
                    Header::released(item)
                }
            }
        }
        #[cfg(feature = "debug")]
        eprintln!("COLLECT: end");
        this.balance.set(0);
    }

    pub(crate) fn clear(&self) {
        self.0.clear()
    }

    /// Cancel all join handle objects in the arena.
    ///
    /// This walks all cyclic objects and clears any that have `join_handle = true`.
    /// Used before `enter()` drains background tasks so that orphaned background
    /// strands get canceled and can unwind.
    pub(crate) fn cancel_join_handles(&self) {
        let visited: ring!(Header, cyclic) = Default::default();
        unsafe {
            visited.init();
            while let Some(item) = self.0.cyclic.pop_front() {
                visited.push_front(item);
                let vtbl = item.as_ref().vtbl;
                if vtbl.as_ref().is_strand {
                    assert_eq!(
                        item.as_ref().borrow.get(),
                        0,
                        "join handle had outstanding borrow"
                    );
                    item.as_ref().retain();
                    (vtbl.as_ref().clear)(item);
                    if item.as_ref().release() {
                        Header::released(item);
                    }
                }
            }
            visited.migrate(&self.0.cyclic);
        }
    }

    #[inline]
    pub(crate) fn allocated(&self) -> usize {
        self.0.allocated.get()
    }
}

impl Drop for ArenaInner {
    fn drop(&mut self) {
        self.clear()
    }
}
