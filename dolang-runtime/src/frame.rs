use std::{
    borrow::Cow,
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    ops::ControlFlow,
    ptr::{self, NonNull},
};

use bitvec::{array::BitArray, bitbox, slice::BitSlice};
pub use dolang_bytecode::span::{Pos, Span};

use crate::{
    Program,
    arg::{Arg, Args, OwnedItem},
    bytecode::{Rest, Variadic},
    error::{Error, Result},
    gc::{
        self, Boxable, Collect, Gc,
        arena::{Header, Visit},
    },
    object::{pack, tuple},
    sig,
    strand::Strand,
    value::{Input, Output, Slot, Value, ValueCell},
    vm::Alloc,
};

pub trait Frame {
    /// Source file and the span of the code running in this frame
    fn source(&self) -> Option<(Cow<'_, str>, Span)>;
    fn receiver(&self) -> Cow<'_, str>;
    fn method(&self) -> Option<Cow<'_, str>>;
    fn module(&self) -> Cow<'_, str>;
}

pub(crate) struct CallFrame<'v> {
    pub(crate) parent: Option<Ptr<'v>>,
    pub(crate) program: Gc<'v, Program<'v>>,
    upvars: UpvarsCell<'v>,
    pub(crate) func: usize,
    pub(crate) pc: Cell<usize>,
    pub(crate) sp: Cell<usize>,
    pub(crate) slots: Vec<ValueCell<'v>>,
    /// Owned arguments of the call being marshaled or made, rooted by [`Self::scan`]
    pub(crate) items: UnsafeCell<Vec<OwnedItem<'v>>>,
    phantom: PhantomData<&'v ()>,
}

impl<'v> CallFrame<'v> {
    /// Unpack function arguments into the frame's local slots according to the function signature.
    ///
    /// # Argument Unpacking Algorithm
    ///
    /// This function processes the provided arguments and assigns them to the correct
    /// local variable slots based on the function's signature (unpack spec):
    ///
    /// 1. **Track assignment**: Slots start as `nil`; positional assignment is tracked by a
    ///    count and key assignment by a bitset with one bit per key parameter
    /// 2. **Handle arguments**:
    ///     - Positional: add to required slots, then optional
    ///     - Key: match against declared keyword parameters
    ///     - Collect excess args into the rest parameters (`...name`, `*name`, `**name`)
    /// 3. **Apply defaults**: Fill unset optional/keyword slots with default values
    /// 4. **Validate**: Ensure all required args were provided
    ///
    /// Slots hold valid values throughout, so an early error or panic (including one
    /// from a collection triggered by error construction) needs no cleanup.
    ///
    /// # Safety
    ///
    /// - Frame must be freshly created by [`CallFrame::new`], so argument slots hold `nil`
    /// - Frame must be properly initialized with valid `loaded` and `func` indices
    /// - Argument slots must be within bounds (verified by the assert)
    /// - The function signature (unpack spec) must match the compiled code
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Required positional argument is missing
    /// - Unexpected positional argument (no rest parameter takes it)
    /// - Unexpected keyword argument (no rest parameter takes it)
    /// - A named keyword argument is repeated
    /// - Required keyword argument without default is missing
    pub(crate) unsafe fn unpack_unchecked<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe {
            let (func, slot_max) = &self.program.funcs().get_unchecked(self.func);
            let unpack = self.program.unpacktab().get_unchecked(func.sig);
            let slots = &self.slots;
            let mut pos = 0;
            let offset = func.locals;
            let pos_count = unpack.required + unpack.optional.len();
            // Leftovers stay in their argument slots, which root them, until the rest's
            // object is allocated
            let mut rest = (unpack.variadic == Variadic::Capture).then(Vec::new);
            // Leftovers for split rests (`*name` and `**name`)
            let mut pos_rest = (unpack.variadic.positional() == Rest::Capture
                && unpack.variadic != Variadic::Capture)
                .then(Vec::new);
            let mut key_rest = (unpack.variadic.keyed() == Rest::Capture
                && unpack.variadic != Variadic::Capture)
                .then(Vec::new);
            // Which key parameters have been assigned, relative to the first one
            let key_count = unpack.keys.len();
            let mut inline = BitArray::<[usize; 1]>::ZERO;
            let mut spilled;
            let assigned: &mut BitSlice = if key_count <= usize::BITS as usize {
                &mut inline[..key_count]
            } else {
                spilled = bitbox![0; key_count];
                &mut spilled
            };

            assert!(*slot_max >= func.locals + unpack.len());

            for arg in args {
                match arg {
                    Arg::Pos(value) => {
                        if pos == pos_count {
                            match unpack.variadic {
                                Variadic::Capture => {
                                    // Capture extra positional arguments in rest
                                    rest.as_mut().unwrap().push((None, value));
                                }
                                variadic => match variadic.positional() {
                                    Rest::None => {
                                        return Err(Error::unexpected_positional(strand, pos));
                                    }
                                    // Allow but discard extra positional arguments
                                    Rest::Discard => {}
                                    // Capture extra positional arguments in `*name`
                                    Rest::Capture => pos_rest.as_mut().unwrap().push(value),
                                },
                            }
                        } else {
                            Output::set(strand, slots.get_unchecked(offset + pos).slot(), value);
                            pos += 1
                        }
                    }
                    Arg::Key(sym, value) => {
                        if let Some(i) = unpack.sym_offset(sym) {
                            if !assigned.replace(i - pos_count, true) {
                                Output::set(strand, slots.get_unchecked(offset + i).slot(), value);
                                continue;
                            }
                            if unpack.variadic.keyed() == Rest::None {
                                return Err(Error::duplicate_key(strand, sym));
                            }
                        }
                        match unpack.variadic {
                            Variadic::Capture => {
                                // Capture extra key arguments in rest
                                rest.as_mut().unwrap().push((Some(sym), value));
                            }
                            variadic => match variadic.keyed() {
                                Rest::None => {
                                    return Err(Error::unexpected_key(strand, sym));
                                }
                                // Allow but discard extra key arguments
                                Rest::Discard => {}
                                // Capture extra key arguments in `**name`
                                Rest::Capture => key_rest.as_mut().unwrap().push((sym, value)),
                            },
                        }
                    }
                }
            }
            if pos < unpack.required {
                return Err(Error::missing_positional(strand, pos));
            }
            for (i, default) in unpack
                .optional
                .get_unchecked((pos - unpack.required)..)
                .iter()
                .enumerate()
            {
                Output::set(
                    strand,
                    slots.get_unchecked(offset + unpack.required + i).slot(),
                    default,
                )
            }

            for (i, key) in unpack.keys.iter().enumerate() {
                if assigned[i] {
                    continue;
                }
                if let Some(default) = &key.default {
                    Output::set(
                        strand,
                        slots.get_unchecked(offset + pos_count + i).slot(),
                        default,
                    )
                } else {
                    return Err(match &key.kind {
                        sig::UnpackKeyKind::Sym(sym) => Error::missing_key(strand, *sym),
                        sig::UnpackKeyKind::Const(_) => unreachable!(),
                    });
                }
            }
            let mut rest_slot = offset + pos_count + unpack.keys.len();
            if let Some(rest) = rest {
                let embryo = strand.builtin_types().pack.embryo(strand);
                let items = rest
                    .into_iter()
                    .map(|(sym, mut value)| {
                        (sym.map(|sym| strand.sym_obj(sym)), value.take(strand))
                    })
                    .collect();
                let record = pack::Pack::new(items);
                embryo.finish_imm(strand, record, slots.get_unchecked(rest_slot).slot());
            }
            if let Some(values) = pos_rest {
                let embryo = tuple::embryo(strand, values.len());
                embryo.finish_moving(strand, values, slots.get_unchecked(rest_slot).slot());
                rest_slot += 1;
            }
            if let Some(entries) = key_rest {
                let embryo = strand.builtin_types().pack.embryo(strand);
                let entries = entries
                    .into_iter()
                    .map(|(sym, mut value)| (strand.sym_obj(sym), value.take(strand)));
                let record = pack::Pack::from_sym_entries(entries);
                embryo.finish_imm(strand, record, slots.get_unchecked(rest_slot).slot());
            }
            self.sp.set(offset + unpack.len());
            Ok(())
        }
    }
}

pub(crate) struct Native<'v> {
    pub(crate) module: Cow<'v, str>,
    pub(crate) receiver: Cow<'v, str>,
    pub(crate) method: Option<Cow<'v, str>>,
    pub(crate) parent: Option<Ptr<'v>>,
}

/// A rooting frame: native-held GC references visited by a closure or held in cells.
///
/// [`Strand::enter_scan`](crate::strand::Strand::enter_scan) pushes one;
/// [`Strand::scan_stack`](crate::strand::StrandInner::scan_stack) visits it, and
/// backtraces skip it.
pub(crate) struct Scan<'v> {
    pub(crate) parent: Option<Ptr<'v>>,
    data: *const (),
    visit: unsafe fn(*const (), &mut dyn Visit) -> ControlFlow<()>,
}

impl<'v> Scan<'v> {
    /// A frame that visits through `visit`.
    ///
    /// The frame borrows `visit` without a lifetime, so `visit` must outlive
    /// every use of the frame, including its time on the frame chain.
    pub(crate) fn new<F: Fn(&mut dyn Visit) -> ControlFlow<()>>(
        parent: Option<Ptr<'v>>,
        visit: &F,
    ) -> Self {
        unsafe fn glue<F: Fn(&mut dyn Visit) -> ControlFlow<()>>(
            data: *const (),
            visitor: &mut dyn Visit,
        ) -> ControlFlow<()> {
            unsafe { (*data.cast::<F>())(visitor) }
        }
        Scan {
            parent,
            data: (visit as *const F).cast(),
            visit: glue::<F>,
        }
    }

    /// A frame that visits each cell of `cells`.
    ///
    /// The frame borrows `cells` without a lifetime, as [`Self::new`] borrows its
    /// closure, and keeps the address of `cells` itself, so a `Vec` stays put while its
    /// contents may grow between collections.
    pub(crate) fn cells<T: AsRef<[ValueCell<'v>]>>(parent: Option<Ptr<'v>>, cells: &T) -> Self {
        unsafe fn glue<'v, T: AsRef<[ValueCell<'v>]>>(
            data: *const (),
            visitor: &mut dyn Visit,
        ) -> ControlFlow<()> {
            let cells = unsafe { &*data.cast::<T>() };
            cells
                .as_ref()
                .iter()
                .try_for_each(|cell| cell.accept(visitor))
        }
        Scan {
            parent,
            data: ptr::from_ref(cells).cast(),
            visit: glue::<T>,
        }
    }

    /// Visit the frame's roots.
    ///
    /// # Safety
    ///
    /// The closure or cells passed to [`Self::new`] or [`Self::cells`] must still be live.
    pub(crate) unsafe fn scan(&self, visitor: &mut dyn Visit) -> ControlFlow<()> {
        unsafe { (self.visit)(self.data, visitor) }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Ptr<'v> {
    Do(NonNull<CallFrame<'v>>),
    Native(NonNull<Native<'v>>),
    Boundary(NonNull<Native<'v>>),
    Scan(NonNull<Scan<'v>>),
}

impl<'v> CallFrame<'v> {
    pub(crate) unsafe fn new(
        loaded: Gc<'v, Program<'v>>,
        func: usize,
        upvars: Option<Gc<'v, Upvars<'v>>>,
        parent: Option<Ptr<'v>>,
    ) -> Self {
        let info = &loaded.funcs()[func];
        let locals = info.0.locals;
        let slots = info.1;
        CallFrame {
            func,
            // SAFETY: the frame is scanned once the caller links it in, before anything
            // allocates
            upvars: unsafe { gc::GcCell::new(upvars) },
            parent,
            program: loaded,
            pc: Cell::new(0),
            phantom: PhantomData,
            sp: Cell::new(locals),
            slots: (0..slots).map(|_| ValueCell::default()).collect(),
            items: UnsafeCell::new(Default::default()),
        }
    }
}

/// A frame of upvars. Its annex holds the parent frame and the variables.
pub(crate) struct Upvars<'v>(PhantomData<Value<'v>>);

/// The fixed part of an [`Upvars`] annex.
pub(crate) struct UpvarsPrefix<'v> {
    pub(crate) parent: Option<Gc<'v, Upvars<'v>>>,
    /// Set once the closure of a non-local branch guard returns.
    pub(crate) stale: Cell<bool>,
}

pub(crate) type UpvarsAnnex<'v> = gc::Tail<UpvarsPrefix<'v>, [ValueCell<'v>]>;

/// A frame's mutable upvar chain head.
type UpvarsCell<'v> = gc::GcCell<'v, gc::Boxed<gc::arena::Header, Upvars<'v>>>;

impl<'v> Gc<'v, Upvars<'v>> {
    /// Copy variable `index` into `out`.
    ///
    /// # Safety
    ///
    /// `index` must be in bounds.
    pub(crate) unsafe fn var(
        &self,
        alloc: &mut impl Alloc<'v>,
        index: usize,
        out: impl Output<'v>,
    ) {
        // SAFETY: a transient pin; duplicating a cell cannot collect
        let cell = unsafe { self.pin_unchecked().annex().tail.get_unchecked(index) };
        Output::set(alloc, out, cell)
    }

    /// Store `value` into variable `index`.  The old value is dropped after the write.
    ///
    /// # Safety
    ///
    /// `index` must be in bounds, and nothing may borrow the variable.
    pub(crate) unsafe fn set_var(
        &self,
        alloc: &mut impl Alloc<'v>,
        index: usize,
        value: impl Input<'v>,
    ) {
        // SAFETY: a transient pin; dropping the old value may run a finalizer, which
        // has no strand and cannot collect
        unsafe {
            let var = self
                .pin_unchecked()
                .annex()
                .tail
                .get_unchecked(index)
                .slot();
            Output::set(alloc, var, value)
        }
    }
}

unsafe impl<'v> Collect for Upvars<'v> {
    const CYCLIC: bool = true;
    type Annex = UpvarsAnnex<'v>;

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> gc::TailAnnex<ValueCell<'v>> for UpvarsPrefix<'v> {
    fn accept(&self, vars: &[ValueCell<'v>], visit: &mut dyn Visit) -> ControlFlow<()> {
        if let Some(parent) = self.parent.as_ref() {
            parent.accept(visit)?
        }
        for var in vars {
            var.accept(visit)?
        }
        ControlFlow::Continue(())
    }

    // The parent is not cleared: a parent frame always exists before its
    // children, so any cycle through it passes through a variable.
    fn clear(&self, vars: &[ValueCell<'v>]) {
        for var in vars {
            // Safety: the collector clears only unreachable frames, so nothing
            // else accesses the cell.
            unsafe { var.clear() }
        }
    }
}

impl<'v> CallFrame<'v> {
    /// The frame's current upvar chain.
    ///
    /// # Safety
    ///
    /// The returned reference must not be held across [`Self::push_upvars`] or
    /// [`Self::pop_upvars`], which write the cell.  Every use stays inside the methods
    /// below.
    unsafe fn upvar_chain(&self) -> Option<&Gc<'v, Upvars<'v>>> {
        unsafe { self.upvars.as_ref() }
    }

    /// Upvar frame at `depth` levels up the chain: 0 is the innermost.
    ///
    /// The frame is reached through transient pins, so nothing may collect while the
    /// result is held.  Upvar reads and writes never do.
    ///
    /// # Safety
    ///
    /// The chain must be at least `depth + 1` frames long.
    unsafe fn upvars_at(&self, mut depth: usize) -> &UpvarsAnnex<'v> {
        unsafe {
            let mut upvars = self
                .upvar_chain()
                .unwrap_unchecked()
                .pin_unchecked()
                .annex();
            while depth != 0 {
                upvars = upvars
                    .prefix
                    .parent
                    .as_ref()
                    .unwrap_unchecked()
                    .pin_unchecked()
                    .annex();
                depth -= 1;
            }
            upvars
        }
    }

    /// Copy the upvar at `index` in the frame `depth` levels up into `out`.
    ///
    /// # Safety
    ///
    /// - The chain must be at least `depth + 1` frames long
    /// - `index` must be valid in the target upvar frame
    pub(crate) unsafe fn get_upvar(
        &self,
        alloc: &mut impl Alloc<'v>,
        index: usize,
        depth: usize,
        out: Slot<'v, '_>,
    ) {
        let cell = unsafe { self.upvars_at(depth).tail.get_unchecked(index) };
        Output::set(alloc, out, cell)
    }

    /// Move the value in `src` into the upvar at `index` in the frame `depth` levels up.
    ///
    /// # Safety
    ///
    /// - The chain must be at least `depth + 1` frames long
    /// - `index` must be valid in the target upvar frame
    pub(crate) unsafe fn set_upvar(
        &self,
        alloc: &mut impl Alloc<'v>,
        index: usize,
        depth: usize,
        src: Slot<'v, '_>,
    ) {
        unsafe {
            // The slot drops the old value only after the write: a drop can run
            // native code
            let var = self.upvars_at(depth).tail.get_unchecked(index).slot();
            Output::set(alloc, var, src)
        }
    }

    /// Push a new upvar frame of `count` nil variables.
    pub(crate) fn push_upvars(&self, alloc: &mut dyn Alloc<'v>, count: usize) {
        let arena = alloc.alloc_vm(crate::vm::private::Sealed).arena();
        // Allocate first, so the parent is duplicated straight into the new frame
        // SAFETY: finished below with `Upvars`' own vtable and header
        let embryo = unsafe { gc::Embryo::<Header, Upvars<'v>>::new_tail(arena, count) };
        // SAFETY: no write to the chain is in progress, and the parent moves into
        // the new frame without an allocation in between
        let parent = unsafe { self.upvars.get() };
        let upvars = unsafe {
            embryo.finish_tail(
                Header::new(
                    arena,
                    NonNull::from_ref(<Upvars<'v> as Boxable<Header>>::VTBL),
                ),
                Upvars(PhantomData),
                UpvarsPrefix {
                    parent,
                    stale: Cell::new(false),
                },
                (0..count).map(|_| ValueCell::default()),
            )
        };
        // SAFETY: nothing borrows the chain across this write
        unsafe { self.upvars.slot() }.store(alloc, Some(upvars))
    }

    /// Pop the innermost upvar frame, returning it.
    ///
    /// # Safety
    ///
    /// The chain must not be empty. The result is loose, as for [`gc::Base::dup`]: it
    /// must be dropped, or moved into a scanned location, before anything allocates.
    pub(crate) unsafe fn pop_upvars(&self, alloc: &mut dyn Alloc<'v>) -> Gc<'v, Upvars<'v>> {
        unsafe {
            // SAFETY: nothing borrows the chain across these writes; the caller takes
            // over the popped frame's looseness
            let mut slot = self.upvars.slot();
            let popped = slot.take(alloc).unwrap_unchecked();
            // SAFETY: the parent moves straight into the chain
            let parent = popped
                .pin_unchecked()
                .annex()
                .prefix
                .parent
                .as_ref()
                .map(|p| p.dup());
            slot.store(alloc, parent);
            popped
        }
    }

    /// The current upvar chain, for a closure to capture.
    ///
    /// # Safety
    /// The result is loose, as for [`gc::Base::dup`].
    pub(crate) unsafe fn upvars(&self) -> Option<Gc<'v, Upvars<'v>>> {
        // SAFETY: no write to the chain is in progress
        unsafe { self.upvars.get() }
    }

    /// Target of a non-local branch to the upvar frame `depth` levels up, or `None` if
    /// its guard has already finished.
    ///
    /// # Safety
    ///
    /// The chain must be at least `depth + 1` frames long.
    pub(crate) unsafe fn nl_target(&self, mut depth: usize) -> Option<gc::Weak<'v, Upvars<'v>>> {
        unsafe {
            let mut current = self.upvar_chain().unwrap_unchecked();
            while depth != 0 {
                current = current
                    .pin_unchecked()
                    .annex()
                    .prefix
                    .parent
                    .as_ref()
                    .unwrap_unchecked();
                depth -= 1;
            }
            if current.pin_unchecked().annex().prefix.stale.get() {
                None
            } else {
                Some(Gc::downgrade(current))
            }
        }
    }

    /// Scan the frame's GC roots: program, upvars, live slots and items.
    ///
    /// # Safety
    ///
    /// Must not run while the frame is mutating its upvar chain or item buffer.
    pub(crate) unsafe fn scan(&self, visitor: &mut dyn Visit) -> ControlFlow<()> {
        unsafe {
            self.program.accept(visitor)?;
            self.upvars.accept(visitor)?;
            for i in 0..self.sp.get() {
                self.slots.get_unchecked(i).accept(visitor)?;
            }
            for (sym, value) in &*self.items.get() {
                if let Some(sym) = sym {
                    sym.accept(visitor)?;
                }
                value.accept(visitor)?;
            }
            ControlFlow::Continue(())
        }
    }
}

/// Information about a VM frame in a backtrace.
pub struct FrameInfo<'v, 'a>(Ptr<'v>, PhantomData<&'a ()>);

impl<'v, 'a> FrameInfo<'v, 'a> {
    /// Source file, and the span of the code running in this frame
    ///
    /// This may be unavailable if the frame represents a native or built-in function,
    /// or if debug information is not present.
    ///
    /// Note that the filename may be a lossy approximation of the native path
    /// on the system on which the code was originally compiled to bytecode.
    pub fn source(&self) -> Option<(&str, Span)> {
        match self.0 {
            Ptr::Do(head) => {
                let head = unsafe { head.as_ref() };
                let loaded = &head.program;
                if let Some(debug) = loaded.funcdebugs().get(head.func) {
                    let file = &loaded.debug_strtab()[debug.file.clone()];
                    Some((file, debug.locate_frame(head.pc.get())))
                } else {
                    None
                }
            }
            Ptr::Native(_) | Ptr::Boundary(_) => None,
            Ptr::Scan(_) => unreachable!(),
        }
    }

    /// Function name or method receiver
    ///
    /// This may be synthetic in some circumstances:
    ///
    /// - Anonymous functions
    /// - Some native functions
    /// - The top level of a module or script
    /// - Functions for which debug information is unavailable
    pub fn receiver(&self) -> &str {
        match self.0 {
            Ptr::Do(head) => {
                let head = unsafe { head.as_ref() };
                let loaded = &head.program;
                loaded
                    .funcdebugs()
                    .get(head.func)
                    .map(|debug| &loaded.debug_strtab()[debug.name.clone()])
                    .unwrap_or(if head.func == 0 { "<main>" } else { "?" })
            }
            Ptr::Native(head) | Ptr::Boundary(head) => {
                let head = unsafe { head.as_ref() };
                head.receiver.as_ref()
            }
            Ptr::Scan(_) => unreachable!(),
        }
    }

    /// Method name.
    ///
    /// `None` for ordinary function calls.  This may be synthetic for certain internal operations.
    pub fn method(&self) -> Option<&str> {
        match self.0 {
            Ptr::Do(_) => None,
            Ptr::Native(head) | Ptr::Boundary(head) => {
                let head = unsafe { head.as_ref() };
                head.method.as_deref()
            }
            Ptr::Scan(_) => unreachable!(),
        }
    }

    /// Module name.  This may be synthetic in some cases.
    pub fn module(&self) -> &str {
        match self.0 {
            Ptr::Do(head) => {
                let head = unsafe { head.as_ref() };
                head.program.module_name().unwrap_or("<program>")
            }
            Ptr::Native(head) | Ptr::Boundary(head) => {
                let head = unsafe { head.as_ref() };
                head.module.as_ref()
            }
            Ptr::Scan(_) => unreachable!(),
        }
    }
}

impl<'v, 'a> Frame for FrameInfo<'v, 'a> {
    fn source(&self) -> Option<(Cow<'_, str>, Span)> {
        FrameInfo::source(self).map(|(path, line)| (Cow::Borrowed(path), line))
    }

    fn receiver(&self) -> Cow<'_, str> {
        Cow::Borrowed(FrameInfo::receiver(self))
    }

    fn method(&self) -> Option<Cow<'_, str>> {
        FrameInfo::method(self).map(Cow::Borrowed)
    }

    fn module(&self) -> Cow<'_, str> {
        Cow::Borrowed(FrameInfo::module(self))
    }
}

/// Iterator over live VM frames
pub struct FrameIter<'v, 'a> {
    pub(crate) head: Option<Ptr<'v>>,
    pub(crate) phantom: PhantomData<&'a ()>,
}

impl<'v, 'a> Iterator for FrameIter<'v, 'a> {
    type Item = FrameInfo<'v, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.head.take() {
                None => break None,
                Some(ptr @ Ptr::Do(head)) => {
                    let head = unsafe { head.as_ref() };
                    self.head = head.parent;
                    break Some(FrameInfo(ptr, PhantomData));
                }
                Some(ptr @ (Ptr::Native(head) | Ptr::Boundary(head))) => {
                    let head = unsafe { head.as_ref() };
                    self.head = head.parent;
                    break Some(FrameInfo(ptr, PhantomData));
                }
                Some(Ptr::Scan(head)) => {
                    let head = unsafe { head.as_ref() };
                    self.head = head.parent;
                }
            }
        }
    }
}
