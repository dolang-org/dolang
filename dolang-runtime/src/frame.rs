use std::{
    borrow::Cow,
    cell::{Cell, UnsafeCell},
    collections::VecDeque,
    marker::PhantomData,
    ops::{ControlFlow, DerefMut},
    ptr::NonNull,
};

use bitvec::{array::BitArray, bitbox, slice::BitSlice};
pub use dolang_bytecode::span::{Pos, Span};
use dolang_util::alias;

use crate::{
    Program,
    arg::{Arg, Args, OwnedItem},
    bytecode::{Rest, Variadic},
    error::{Error, Result},
    gc::{Collect, Gc, arena::Visit},
    object::{pack, protocol::GcObj, tuple},
    sig,
    strand::Strand,
    value::{Output, Value, ValueCell},
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
    pub(crate) upvars: Option<Gc<'v, Upvars<'v>>>,
    pub(crate) func: usize,
    pub(crate) pc: usize,
    pub(crate) sp: Cell<usize>,
    pub(crate) slots: Vec<ValueCell<'v>>,
    pub(crate) scratch1: ValueCell<'v>,
    pub(crate) scratch2: ValueCell<'v>,
    pub(crate) scratch3: ValueCell<'v>,
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
        &mut self,
        strand: &mut Strand<'v, 's>,
        args: Args<'v, '_>,
    ) -> Result<'v, 's, ()> {
        unsafe {
            let inner = strand.inner;
            let program = self.program.annex();
            let (func, slot_max) = &program.funcs.get_unchecked(self.func);
            let unpack = program.unpacktab.get_unchecked(func.sig);
            let slots = &self.slots;
            let mut pos = 0;
            let offset = func.locals;
            let pos_count = unpack.required + unpack.optional.len();
            let mut rest = if unpack.variadic == Variadic::Capture {
                Some(VecDeque::new())
            } else {
                None
            };
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
                                    rest.as_mut()
                                        .unwrap()
                                        .push_back(Some((None, Value::from_input(strand, value))));
                                }
                                variadic => match variadic.positional() {
                                    Rest::None => {
                                        return Err(Error::unexpected_positional_raw(inner, pos));
                                    }
                                    // Allow but discard extra positional arguments
                                    Rest::Discard => {}
                                    // Capture extra positional arguments in `*name`
                                    Rest::Capture => pos_rest
                                        .as_mut()
                                        .unwrap()
                                        .push(Value::from_input(strand, value)),
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
                                return Err(Error::duplicate_key_raw(inner, sym));
                            }
                        }
                        match unpack.variadic {
                            Variadic::Capture => {
                                // Capture extra key arguments in rest
                                rest.as_mut().unwrap().push_back(Some((
                                    Some(inner.vm().sym_obj(sym)),
                                    Value::from_input(strand, value),
                                )));
                            }
                            variadic => match variadic.keyed() {
                                Rest::None => {
                                    return Err(Error::unexpected_key_raw(inner, sym));
                                }
                                // Allow but discard extra key arguments
                                Rest::Discard => {}
                                // Capture extra key arguments in `**name`
                                Rest::Capture => key_rest.as_mut().unwrap().push((
                                    inner.vm().sym_obj(sym),
                                    Value::from_input(strand, value),
                                )),
                            },
                        }
                    }
                }
            }
            if pos < unpack.required {
                return Err(Error::missing_positional_raw(inner, pos));
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
                        sig::UnpackKeyKind::Sym(sym) => Error::missing_key_raw(inner, *sym),
                        sig::UnpackKeyKind::Const(_) => unreachable!(),
                    });
                }
            }
            if let Some(rest) = rest {
                let record = pack::Pack::new(rest.into_iter().flatten().collect());
                let record = GcObj::new(strand.arena(), strand.builtin_types().pack, record);
                Output::set(
                    strand,
                    slots
                        .get_unchecked(offset + pos_count + unpack.keys.len())
                        .slot(),
                    Value::from_object(record),
                );
            }
            let mut rest_slot = offset + pos_count + unpack.keys.len();
            if let Some(values) = pos_rest {
                let tuple = Value::from_object(tuple::tuple(strand, values));
                Output::set(strand, slots.get_unchecked(rest_slot).slot(), tuple);
                rest_slot += 1;
            }
            if let Some(entries) = key_rest {
                let record = pack::Pack::from_sym_entries(entries);
                let record = GcObj::new(strand.arena(), strand.builtin_types().pack, record);
                Output::set(
                    strand,
                    slots.get_unchecked(rest_slot).slot(),
                    Value::from_object(record),
                );
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

pub(crate) struct Slots<'v> {
    pub(crate) parent: Option<Ptr<'v>>,
    pub(crate) slots: *const [ValueCell<'v>],
}

#[derive(Clone, Copy)]
pub(crate) enum Ptr<'v> {
    Do(NonNull<CallFrame<'v>>),
    Native(NonNull<Native<'v>>),
    Boundary(NonNull<Native<'v>>),
    Slots(NonNull<Slots<'v>>),
}

impl<'v> CallFrame<'v> {
    pub(crate) unsafe fn new(
        loaded: Gc<'v, Program<'v>>,
        func: usize,
        upvars: Option<Gc<'v, Upvars<'v>>>,
        parent: Option<Ptr<'v>>,
    ) -> Self {
        let info = &loaded.annex().funcs[func];
        let locals = info.0.locals;
        let slots = info.1;
        CallFrame {
            func,
            upvars,
            parent,
            program: loaded,
            pc: 0,
            phantom: PhantomData,
            sp: Cell::new(locals),
            slots: (0..slots).map(|_| ValueCell::default()).collect(),
            scratch1: ValueCell::default(),
            scratch2: ValueCell::default(),
            scratch3: ValueCell::default(),
            items: UnsafeCell::new(Default::default()),
        }
    }
}

#[derive(Default)]
pub(crate) struct Upvars<'v> {
    pub(crate) parent: Option<Gc<'v, Upvars<'v>>>,
    pub(crate) vars: alias::Box<[Value<'v>]>,
    pub(crate) stale: bool,
}

unsafe impl<'v> Collect for Upvars<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        if let Some(parent) = self.parent.as_ref() {
            parent.accept(visit)?
        }
        for var in self.vars.iter() {
            var.accept(visit)?
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        self.parent = None;
        for var in self.vars.iter_mut() {
            *var = Value::NIL;
        }
    }
}

impl<'v> Upvars<'v> {
    /// Traverse the upvar chain to find the upvar frame at the given depth.
    ///
    /// ## Upvar Chain Structure
    ///
    /// Upvars form a linked list where each closure's upvars reference its parent.
    /// The depth indicates how many levels up to traverse:
    /// - Depth 0: The current function's upvars
    /// - Depth 1: The parent function's upvars
    /// - Depth N: The Nth ancestor's upvars
    ///
    /// ## Safety
    ///
    /// - `this` must be Some (not None)
    /// - `depth` must not exceed the actual chain length
    ///
    /// ## Panics
    ///
    /// Panics if there is a conflicting borrow.  The interpreter never
    /// holds long-term borrows, nor do other objects that can access
    /// upvars (e.g. `Module`), so this could be converted to an unchecked
    /// unwrap in principle.
    pub(crate) unsafe fn at_depth(
        this: &Option<Gc<'v, Upvars<'v>>>,
        mut depth: usize,
    ) -> Gc<'v, Upvars<'v>> {
        unsafe {
            let mut current = this.as_ref().unwrap_unchecked().clone();
            while depth != 0 {
                let parent = current
                    .borrow()
                    .unwrap()
                    .parent
                    .as_ref()
                    .unwrap_unchecked()
                    .clone();
                current = parent;
                depth -= 1;
            }
            current
        }
    }

    /// Get the value of an upvar at the given index and depth.
    ///
    /// # Safety
    ///
    /// - `this` must be Some (not None)
    /// - `depth` must not exceed the upvar chain length
    /// - `index` must be a valid index in the target upvar frame
    pub(crate) unsafe fn get_unchecked<'a>(
        this: &'a Option<Gc<'v, Upvars<'v>>>,
        index: usize,
        mut depth: usize,
    ) -> Value<'v> {
        unsafe {
            let mut upvars: *const Upvars<'v> =
                this.as_ref().unwrap_unchecked().borrow().unwrap().as_ref() as *const _;
            while depth != 0 {
                upvars = (*upvars)
                    .parent
                    .as_ref()
                    .unwrap_unchecked()
                    .borrow()
                    .unwrap()
                    .as_ref() as *const _;
                depth -= 1;
            }
            let upvars = &*upvars;
            upvars.vars.get_unchecked(index).dup()
        }
    }

    /// Set the value of an upvar at the given index and depth.
    ///
    /// # Safety
    ///
    /// - `this` must be Some (not None)
    /// - `depth` must not exceed the upvar chain length
    /// - `index` must be a valid index in the target upvar frame
    pub(crate) unsafe fn set_unchecked<'a>(
        this: &'a Option<Gc<'v, Upvars<'v>>>,
        index: usize,
        mut depth: usize,
        value: Value<'v>,
    ) {
        unsafe {
            let mut upvars: *mut Upvars<'v> = this
                .as_ref()
                .unwrap_unchecked()
                .borrow_mut()
                .unwrap()
                .deref_mut() as *mut _;
            while depth != 0 {
                upvars = (*upvars)
                    .parent
                    .as_mut()
                    .unwrap_unchecked()
                    .borrow_mut()
                    .unwrap()
                    .deref_mut() as *mut _;
                depth -= 1;
            }
            let upvars = &mut *upvars;
            *upvars.vars.get_unchecked_mut(index) = value;
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
                let loaded = head.program.annex();
                if let Some(debug) = loaded.funcdebugs.get(head.func) {
                    let file = &loaded.debug_strtab()[debug.file.clone()];
                    Some((file, debug.locate(head.pc - 1)))
                } else {
                    None
                }
            }
            Ptr::Native(_) | Ptr::Boundary(_) => None,
            Ptr::Slots(_) => unreachable!(),
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
                let loaded = head.program.annex();
                loaded
                    .funcdebugs
                    .get(head.func)
                    .map(|debug| &loaded.debug_strtab()[debug.name.clone()])
                    .unwrap_or(if head.func == 0 { "<main>" } else { "?" })
            }
            Ptr::Native(head) | Ptr::Boundary(head) => {
                let head = unsafe { head.as_ref() };
                head.receiver.as_ref()
            }
            Ptr::Slots(_) => unreachable!(),
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
            Ptr::Slots(_) => unreachable!(),
        }
    }

    /// Module name.  This may be synthetic in some cases.
    pub fn module(&self) -> &str {
        match self.0 {
            Ptr::Do(head) => {
                let head = unsafe { head.as_ref() };
                let loaded = head.program.annex();
                match &loaded.module_name {
                    Some(range) => &loaded.debug_strtab()[range.clone()],
                    None => "<program>",
                }
            }
            Ptr::Native(head) | Ptr::Boundary(head) => {
                let head = unsafe { head.as_ref() };
                head.module.as_ref()
            }
            Ptr::Slots(_) => unreachable!(),
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
                Some(Ptr::Slots(head)) => {
                    let head = unsafe { head.as_ref() };
                    self.head = head.parent;
                }
            }
        }
    }
}
