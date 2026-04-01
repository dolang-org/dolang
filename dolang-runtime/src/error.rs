use std::{
    borrow::Cow,
    cell::RefCell,
    error,
    fmt::{self, Debug, Formatter},
    marker::PhantomData,
    mem,
    ptr::NonNull,
    result, slice,
};

use crate::{
    Program,
    frame::{self, Frame},
    gc::{self, Gc},
    object::{
        backtrace,
        error::Boxed,
        native::{Object, Type},
        protocol::GcObj,
    },
    strand::{Strand, StrandInner, StrandMut},
    sym::Sym,
    value::{Case, Input, Output, Value},
    vm::Vm,
};

use dolang_bytecode as bytecode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
/// Category of execution error
pub enum ErrorKind {
    /// Operation is unsupported by the receiver
    Unsupported,
    /// Attempt to modify an immutable structure
    Immutable,
    /// Concurrent access violation
    Concurrency,
    /// Value used in a way inappropriate to its type
    Type,
    /// Value has correct type but invalid contents or meaning
    Value,
    /// Operation is invalid for the current object or runtime state
    State,
    /// Index did not exist in index access or assignment operation
    Index,
    /// Field did not exist in field get or set operation
    Field,
    /// Unexpected positional argument (in call) or element (in destructuring bind)
    UnexpectedPos,
    /// Unexpected key argument (in call) or element (in destructuring bind)
    UnexpectedKey,
    /// Missing positional argument (in call) or element (in destructuring bind)
    MissingPos,
    /// Missing key argument (in call) or element (in destructuring bind)
    MissingKey,
    /// Numeric overflow
    Overflow,
    /// Integer zero divisor
    ZeroDiv,
    /// Output iterator stopped
    SinkStop,
    /// Input iterator stopped
    IterStop,
    /// Attempt to import a module already being imported
    CyclicImport,
    /// Failure resolving a module during an import
    Import,
    /// Compilation error when loading a module
    Compile,
    /// Error attempting to load Do bytecode
    Bytecode,
    /// Generic runtime error
    Runtime,
    /// Explicit interrupt
    Interrupt,
    /// Strand canceled
    Canceled,
}

#[derive(Clone)]
pub(crate) enum UnwindEntry<'v> {
    Do {
        loaded_id: u32,
        function_index: u32,
        pc: u32,
    },
    Native {
        module: Cow<'v, str>,
        receiver: Cow<'v, str>,
        method: Option<Cow<'v, str>>,
    },
}

struct ErrorMeta<'v> {
    pub(crate) vm: &'v Vm<'v>,
    pub(crate) backtrace: Vec<UnwindEntry<'v>>,
    // Keep loading modules from being unloaded until the backtrace
    // can be examined
    pub(crate) sticky: Vec<Gc<'v, Program<'v>>>,
}

impl<'v> ErrorMeta<'v> {
    fn new(vm: &'v Vm<'v>) -> Self {
        Self {
            vm,
            backtrace: Default::default(),
            sticky: Default::default(),
        }
    }
}

pub(crate) type ErrorPair<'v> = (Value<'v>, Vec<UnwindEntry<'v>>);

impl<'v> UnwindEntry<'v> {
    pub(crate) fn source(&self, vm: &Vm<'v>) -> Option<(Cow<'_, str>, u32)> {
        match self {
            UnwindEntry::Do {
                loaded_id,
                function_index,
                pc,
            } => {
                let loaded = vm.loaded_for_id(*loaded_id)?;
                if let Some(debug) = loaded.funcdebugs.get(*function_index as usize) {
                    let pc = *pc as usize - 1;
                    let sourcemap = &debug.sourcemap;
                    let index = match sourcemap.binary_search_by_key(&pc, |e| e.0) {
                        Ok(i) => i,
                        Err(i) => i - 1,
                    };
                    let entry = &sourcemap[index];
                    let file = &loaded.debug_strtab()[entry.2.clone()];
                    Some((Cow::Owned(file.to_owned()), entry.1))
                } else {
                    None
                }
            }
            UnwindEntry::Native { .. } => None,
        }
    }

    pub(crate) fn receiver(&self, vm: &Vm<'v>) -> Cow<'_, str> {
        match self {
            UnwindEntry::Do {
                loaded_id,
                function_index,
                ..
            } => {
                let loaded = match vm.loaded_for_id(*loaded_id) {
                    Some(loaded) => loaded,
                    None => return Cow::Borrowed("<unloaded>"),
                };
                Cow::Owned(
                    loaded
                        .funcdebugs
                        .get(*function_index as usize)
                        .map(|debug| &loaded.debug_strtab()[debug.name.clone()])
                        .unwrap_or(if *function_index == 0 { "<main>" } else { "?" })
                        .to_owned(),
                )
            }
            UnwindEntry::Native { receiver, .. } => Cow::Borrowed(receiver.as_ref()),
        }
    }

    pub(crate) fn method(&self) -> Option<Cow<'_, str>> {
        match self {
            UnwindEntry::Do { .. } => None,
            UnwindEntry::Native { method, .. } => method.as_deref().map(Cow::Borrowed),
        }
    }

    pub(crate) fn module(&self, vm: &Vm<'v>) -> Cow<'_, str> {
        match self {
            UnwindEntry::Do { loaded_id, .. } => {
                let loaded = match vm.loaded_for_id(*loaded_id) {
                    Some(loaded) => loaded,
                    None => return Cow::Borrowed("<program>"),
                };
                match &loaded.module_name {
                    Some(range) => Cow::Owned(loaded.debug_strtab()[range.clone()].to_owned()),
                    None => Cow::Borrowed("<program>"),
                }
            }
            UnwindEntry::Native { module, .. } => Cow::Borrowed(module.as_ref()),
        }
    }
}

/// A GC root held by a strand's floating roots table.
///
/// The `'s` lifetime brand ensures this root is valid for the lifetime of the owning strand.
/// `Drop` clears the slot in the table so the GC no longer treats it as a root.
struct FloatingRoot<'v, 's> {
    idx: usize,
    table: NonNull<RefCell<StrandMut<'v>>>,
    phantom: PhantomData<(&'v mut &'v (), &'s mut &'s ())>,
}

impl<'v, 's> FloatingRoot<'v, 's> {
    fn new(inner: &'s StrandInner<'v>, value: Value<'v>) -> Self {
        FloatingRoot {
            idx: inner.alloc_floating_root(value),
            table: inner.mutable_ptr(),
            phantom: PhantomData,
        }
    }

    fn get(&self) -> Value<'v> {
        // Safety: table pointer is valid for 's, which outlives this borrow
        unsafe { (*self.table.as_ptr()).borrow() }.floating_roots[self.idx]
            .as_ref()
            .expect("floating root not set")
            .dup()
    }

    /// Consume the root and retrieve its value without running `Drop`.
    /// The slot in the table is cleared immediately.
    fn take(self) -> Value<'v> {
        // Safety: table pointer is valid for 's, which outlives this access
        let val = unsafe { (*self.table.as_ptr()).borrow_mut() }.floating_roots[self.idx]
            .take()
            .unwrap_or(Value::NIL);
        mem::forget(self);
        val
    }
}

impl<'v, 's> Drop for FloatingRoot<'v, 's> {
    fn drop(&mut self) {
        // Safety: table pointer is valid for 's, which outlives this drop
        unsafe { (*self.table.as_ptr()).borrow_mut() }.floating_roots[self.idx] = None;
    }
}

enum Variant<'v, 's> {
    Unsupported,
    Immutable,
    Overflow,
    ZeroDiv,
    SinkStop,
    IterStop,
    Canceled,
    NonLocalJump(u8, gc::Weak<'v, frame::Upvars<'v>>),
    Boxed(FloatingRoot<'v, 's>, Box<ErrorMeta<'v>>),
}

/// Do execution error
pub struct Error<'v, 's> {
    inner: Variant<'v, 's>,
    phantom: PhantomData<(&'v mut &'v (), &'s mut &'s ())>,
}

/// Entry in an error backtrace
pub struct BacktraceEntry<'v, 'a>(&'a Vm<'v>, &'a UnwindEntry<'v>);

impl<'v, 'a> BacktraceEntry<'v, 'a> {
    /// Source and line number of the error.
    ///
    /// This may not be available if:
    /// - The entry represents a native function
    /// - The corresponding module has since been unloaded
    /// - Debug information was not available
    ///
    /// Note that the filename may be a lossy approximation of the native path
    /// on the system on which the code was originally compiled to bytecode.
    pub fn source(&self) -> Option<(Cow<'_, str>, u32)> {
        self.1.source(self.0)
    }

    /// Call or method call receiver
    ///
    /// This will be the function name for function calls and the receiver type
    /// name for method calls.  This may be synthetic in some circumstances:
    ///
    /// - Anonymous functions
    /// - Some native functions
    /// - Functions in subsequently unloaded modules
    /// - The top level of a module or script
    /// - Functions for which debug information was unavailable
    pub fn receiver(&self) -> Cow<'_, str> {
        self.1.receiver(self.0)
    }

    /// Method.  Will be `None` if this entry is an ordinary function call.
    /// Certain method names corresponding to internal operations are synthetic.
    pub fn method(&self) -> Option<Cow<'_, str>> {
        self.1.method()
    }

    /// Module name.  This may be synthetic in some cases.
    pub fn module(&self) -> Cow<'_, str> {
        self.1.module(self.0)
    }
}

impl<'v, 'a> Frame for BacktraceEntry<'v, 'a> {
    fn source(&self) -> Option<(Cow<'_, str>, u32)> {
        BacktraceEntry::source(self)
    }

    fn receiver(&self) -> Cow<'_, str> {
        BacktraceEntry::receiver(self)
    }

    fn method(&self) -> Option<Cow<'_, str>> {
        BacktraceEntry::method(self)
    }

    fn module(&self) -> Cow<'_, str> {
        BacktraceEntry::module(self)
    }
}

/// Iterator over backtrace entries
pub struct BacktraceIter<'v, 'a>(Option<&'a Vm<'v>>, slice::Iter<'a, UnwindEntry<'v>>);

impl<'v, 'a> BacktraceIter<'v, 'a> {
    pub(crate) fn new(vm: &'a Vm<'v>, iter: slice::Iter<'a, UnwindEntry<'v>>) -> Self {
        Self(Some(vm), iter)
    }

    pub(crate) fn empty() -> Self {
        Self(None, [].iter())
    }
}

impl<'v, 'a> Iterator for BacktraceIter<'v, 'a> {
    type Item = BacktraceEntry<'v, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.1
            .next()
            .map(|e| BacktraceEntry(self.0.expect("backtrace VM missing"), e))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.1.size_hint()
    }
}

impl<'v, 'a> ExactSizeIterator for BacktraceIter<'v, 'a> {
    fn len(&self) -> usize {
        self.1.len()
    }
}

pub(crate) struct OwnedBacktraceEntry<'v, 'a> {
    vm: &'a Vm<'v>,
    entry: UnwindEntry<'v>,
}

impl<'v, 'a> Frame for OwnedBacktraceEntry<'v, 'a> {
    fn source(&self) -> Option<(Cow<'_, str>, u32)> {
        self.entry.source(self.vm)
    }

    fn receiver(&self) -> Cow<'_, str> {
        self.entry.receiver(self.vm)
    }

    fn method(&self) -> Option<Cow<'_, str>> {
        self.entry.method()
    }

    fn module(&self) -> Cow<'_, str> {
        self.entry.module(self.vm)
    }
}

pub(crate) struct OwnedBacktraceIter<'v, 'a> {
    vm: &'a Vm<'v>,
    entries: std::vec::IntoIter<UnwindEntry<'v>>,
}

impl<'v, 'a> OwnedBacktraceIter<'v, 'a> {
    pub(crate) fn new(vm: &'a Vm<'v>, entries: Vec<UnwindEntry<'v>>) -> Self {
        Self {
            vm,
            entries: entries.into_iter(),
        }
    }
}

impl<'v, 'a> Iterator for OwnedBacktraceIter<'v, 'a> {
    type Item = OwnedBacktraceEntry<'v, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries
            .next()
            .map(|entry| OwnedBacktraceEntry { vm: self.vm, entry })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl<'v, 'a> ExactSizeIterator for OwnedBacktraceIter<'v, 'a> {
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Do execution result
pub type Result<'v, 's, T> = result::Result<T, Error<'v, 's>>;

impl<'v, 's> Debug for Error<'v, 's> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Error").field(&self.kind()).finish()
    }
}

impl<'v, 's> fmt::Display for Error<'v, 's> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Variant::Unsupported => write!(f, "unsupported operation"),
            Variant::Immutable => write!(f, "object is immutable"),
            Variant::Overflow => write!(f, "numeric overflow"),
            Variant::ZeroDiv => write!(f, "integer zero divisor"),
            Variant::SinkStop => write!(f, "output iterator stopped"),
            Variant::IterStop => write!(f, "input iterator stopped"),
            Variant::Canceled => write!(f, "strand canceled"),
            Variant::NonLocalJump(..) => write!(f, "non-local jump"),
            Variant::Boxed(root, info) => {
                let val = root.get();
                match val.downcast_ref(info.vm.builtin_types().error) {
                    Some(boxed) => write!(f, "{}", boxed.get()),
                    None => write!(f, "<value>"),
                }
            }
        }
    }
}

impl<'v, 's> error::Error for Error<'v, 's> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.inner {
            Variant::Boxed(root, info) => {
                let val = root.get();
                match val.downcast_ref(info.vm.builtin_types().error) {
                    Some(boxed) => match boxed.get() {
                        Boxed::Compile(error)
                        | Boxed::Bytecode(error)
                        | Boxed::Runtime(error)
                        | Boxed::Interrupt(error) => {
                            // Safety: the Box<dyn error::Error + 'static> is
                            // kept alive by FloatingRoot which outlives &self.
                            Some(unsafe {
                                mem::transmute::<&dyn error::Error, &dyn error::Error>(&**error)
                            })
                        }
                        _ => None,
                    },
                    None => None,
                }
            }
            _ => None,
        }
    }
}

/// Format helper for `Error`
pub struct Display<'v, 's, 'a> {
    error: &'a Error<'v, 's>,
    strand: RefCell<&'a mut Strand<'v, 's>>,
}

impl<'v, 's, 'a> fmt::Display for Display<'v, 's, 'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Variant::Boxed(root, _) = &self.error.inner {
            let val = root.get();
            val.op_display(*self.strand.borrow_mut(), f)
                .map_err(|_| fmt::Error)
        } else {
            write!(f, "{}", self.error)
        }
    }
}

impl<'v, 's> Error<'v, 's> {
    fn new_info(inner: &'s StrandInner<'v>, kind: Boxed<'v>) -> Self {
        let value = Value::from_object(GcObj::new(
            inner.vm().arena(),
            inner.vm().builtin_types().error,
            kind,
        ));
        Self {
            inner: Variant::Boxed(
                FloatingRoot::new(inner, value),
                Box::new(ErrorMeta::new(inner.vm())),
            ),
            phantom: PhantomData,
        }
    }

    fn new_variant(variant: Variant<'v, 's>) -> Self {
        Self {
            inner: variant,
            phantom: PhantomData,
        }
    }

    fn expand(&mut self, inner: &'s StrandInner<'v>) {
        self.inner = match mem::replace(&mut self.inner, Variant::Unsupported) {
            Variant::Unsupported => Self::new_info(inner, Boxed::Unsupported).inner,
            Variant::Immutable => Self::new_info(inner, Boxed::Immutable).inner,
            Variant::Overflow => Self::new_info(inner, Boxed::Overflow).inner,
            Variant::ZeroDiv => Self::new_info(inner, Boxed::ZeroDiv).inner,
            Variant::SinkStop => Self::new_info(inner, Boxed::SinkStop).inner,
            Variant::IterStop => Self::new_info(inner, Boxed::IterStop).inner,
            Variant::Canceled => Self::new_info(inner, Boxed::Canceled).inner,
            Variant::NonLocalJump(..) => unreachable!("non-local jump should never be boxed"),
            Variant::Boxed(..) => return,
        }
    }

    /// Get error as a Do `Value`, performing internal conversion if necessary
    pub fn get_value<'a>(&'a mut self, strand: &Strand<'v, 's>, output: impl Output<'v>) {
        loop {
            match self.inner {
                Variant::Boxed(ref root, ..) => {
                    let val = root.get();
                    Output::set(strand, output, &val);
                    break;
                }
                _ => self.expand(strand.inner),
            }
        }
    }

    /// Create error: operation not supported
    pub fn not_supported(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::Unsupported)
    }

    /// Create error: immutable structure
    pub fn immutable(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::Immutable)
    }

    /// Create error: concurrent access violation
    pub fn concurrency(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_info(strand.inner, Boxed::Concurrency(None))
    }

    /// Create error: concurrent access violation (with message)
    pub fn concurrency_msg(
        #[allow(unused_variables)] strand: &Strand<'v, 's>,
        msg: impl Into<Cow<'v, str>>,
    ) -> Self {
        Self::new_info(strand.inner, Boxed::Concurrency(Some(msg.into())))
    }

    pub(crate) fn concurrency_raw(#[allow(unused_variables)] strand: &'s StrandInner<'v>) -> Self {
        Self::new_info(strand, Boxed::Concurrency(None))
    }

    /// Create error: field does not exist
    pub fn field(strand: &Strand<'v, 's>, key: Sym<'v, '_>) -> Self {
        Self::new_info(strand.inner, Boxed::Field(key.as_str(strand).into()))
    }

    /// Create error: field does not exist
    pub fn field_name(strand: &Strand<'v, 's>, key: impl Into<String>) -> Self {
        Self::new_info(strand.inner, Boxed::Field(key.into().into()))
    }

    /// Create error: absent or invalid index
    pub fn index(strand: &Strand<'v, 's>) -> Self {
        Self::new_info(strand.inner, Boxed::Index)
    }

    /// Create error: unexpected positional item.
    pub fn unexpected_positional(strand: &Strand<'v, 's>, index: usize) -> Self {
        Self::new_info(strand.inner, Boxed::UnexpectedPos(index))
    }

    /// Create error: unexpected key item.
    pub fn unexpected_key(strand: &Strand<'v, 's>, key: impl Input<'v>) -> Self {
        let value = Value::from_input(strand, key);
        let str = if let Some(sym) = value.as_sym(strand) {
            sym.as_str(strand).to_string()
        } else if let Some(str) = value.as_str(strand) {
            format!("{:?}", str)
        } else if let Case::Prim(prim) = value.case() {
            format!("{:?}", prim)
        } else {
            "<unknown>".to_string()
        };
        Self::new_info(strand.inner, Boxed::UnexpectedKey(str.into()))
    }

    /// Create error: missing positional item.
    pub fn missing_positional(strand: &Strand<'v, 's>, index: usize) -> Self {
        Self::new_info(strand.inner, Boxed::MissingPos(index))
    }

    /// Create error: missing key argument.
    pub fn missing_key(strand: &Strand<'v, 's>, key: impl Input<'v>) -> Self {
        let value = Value::from_input(strand, key);
        let str = if let Some(sym) = value.as_sym(strand) {
            sym.as_str(strand).to_string()
        } else if let Some(str) = value.as_str(strand) {
            format!("{:?}", str)
        } else if let Case::Prim(prim) = value.case() {
            format!("{:?}", prim)
        } else {
            "<unknown>".to_string()
        };
        Self::new_info(strand.inner, Boxed::MissingKey(str.into()))
    }

    pub(crate) fn unexpected_positional_raw(strand: &'s StrandInner<'v>, index: usize) -> Self {
        Self::new_info(strand, Boxed::UnexpectedPos(index))
    }

    pub(crate) fn unexpected_key_raw(strand: &'s StrandInner<'v>, key: Sym<'v, '_>) -> Self {
        Self::new_info(strand, Boxed::UnexpectedKey(key.as_str(strand.vm()).into()))
    }

    pub(crate) fn missing_positional_raw(strand: &'s StrandInner<'v>, index: usize) -> Self {
        Self::new_info(strand, Boxed::MissingPos(index))
    }

    pub(crate) fn missing_key_raw(strand: &'s StrandInner<'v>, key: Sym<'v, '_>) -> Self {
        Self::new_info(strand, Boxed::MissingKey(key.as_str(strand.vm()).into()))
    }

    /// Create error: numeric overflow
    pub fn overflow(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::Overflow)
    }

    /// Create error: zero divisor
    pub fn zero_div(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::ZeroDiv)
    }

    /// Create error: output stopped
    pub fn sink_stop(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::SinkStop)
    }

    pub(crate) fn output_stop_raw(#[allow(unused_variables)] strand: &'s StrandInner<'v>) -> Self {
        Self::new_variant(Variant::SinkStop)
    }

    /// Create error: input stopped
    pub fn iter_stop(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::IterStop)
    }

    /// Create error: misuse of value according to type
    pub fn type_error(strand: &Strand<'v, 's>, msg: impl Into<Cow<'v, str>>) -> Self {
        Self::new_info(strand.inner, Boxed::Type(msg.into()))
    }

    /// Create error: invalid value with otherwise acceptable type
    pub fn value(strand: &Strand<'v, 's>, msg: impl Into<Cow<'v, str>>) -> Self {
        Self::new_info(strand.inner, Boxed::Value(msg.into()))
    }

    /// Create error: invalid operation for the current state
    pub fn state_error(strand: &Strand<'v, 's>, msg: impl Into<Cow<'v, str>>) -> Self {
        Self::new_info(strand.inner, Boxed::State(msg.into()))
    }

    /// Create error: compilation failed
    pub fn compile(strand: &Strand<'v, 's>, err: impl Into<Box<dyn error::Error>>) -> Self {
        Self::new_info(strand.inner, Boxed::Compile(err.into()))
    }

    /// Create error: module could not be imported
    pub fn import(strand: &Strand<'v, 's>, name: &str) -> Self {
        Self::new_info(strand.inner, Boxed::Import(name.into()))
    }

    /// Create error: cyclic module import detected
    pub fn cyclic_import(strand: &Strand<'v, 's>, name: &str) -> Self {
        Self::new_info(strand.inner, Boxed::CyclicImport(name.into()))
    }

    pub(crate) fn bytecode(strand: &Strand<'v, 's>, err: bytecode::Error) -> Self {
        Self::new_info(strand.inner, Boxed::Bytecode(err.into()))
    }

    /// Create error: generic runtime error
    pub fn runtime(strand: &Strand<'v, 's>, err: impl Into<Box<dyn error::Error>>) -> Self {
        Self::new_info(strand.inner, Boxed::Runtime(err.into()))
    }

    /// Create error: generic value
    pub fn from_value(strand: &Strand<'v, 's>, err: impl Input<'v>) -> Self {
        let val = Value::from_input(strand, err);
        Self {
            inner: Variant::Boxed(
                FloatingRoot::new(strand.inner, val),
                Box::new(ErrorMeta::new(strand.inner.vm())),
            ),
            phantom: PhantomData,
        }
    }

    /// Constuct error from `self` which indicates it was caused by `cause`.
    pub fn caused_by(self, strand: &Strand<'v, 's>, cause: Error<'v, 's>) -> Self {
        match (self.inner, cause.inner) {
            (Variant::Boxed(left, mut left_bt), Variant::Boxed(right, right_bt))
                if left.get().repr_eq(strand, &right.get()) =>
            {
                left_bt.backtrace.extend(right_bt.backtrace);
                left_bt.sticky.extend(right_bt.sticky);
                Self {
                    inner: Variant::Boxed(left, left_bt),
                    phantom: PhantomData,
                }
            }
            (left, _) => Self {
                inner: left,
                phantom: PhantomData,
            },
        }
    }

    /// Create error: object
    pub fn object<T: Object<'v>>(strand: &Strand<'v, 's>, ty: Type<'v, T>, value: T) -> Self
    where
        T::Annex: Default,
    {
        let val = ty.create_raw(strand, value, Default::default());
        Self {
            inner: Variant::Boxed(
                FloatingRoot::new(strand.inner, val),
                Box::new(ErrorMeta::new(strand.inner.vm())),
            ),
            phantom: PhantomData,
        }
    }

    /// Create error: object with annex
    pub fn object_with_annex<T: Object<'v>>(
        strand: &Strand<'v, 's>,
        ty: Type<'v, T>,
        value: T,
        annex: T::Annex,
    ) -> Self {
        let val = ty.create_raw(strand, value, annex);
        Self {
            inner: Variant::Boxed(
                FloatingRoot::new(strand.inner, val),
                Box::new(ErrorMeta::new(strand.inner.vm())),
            ),
            phantom: PhantomData,
        }
    }

    pub(crate) fn runtime_raw(
        strand: &'s StrandInner<'v>,
        err: impl Into<Box<dyn error::Error>>,
    ) -> Self {
        Self::new_info(strand, Boxed::Runtime(err.into()))
    }

    pub(crate) fn call_depth_raw(strand: &'s StrandInner<'v>) -> Self {
        Self::runtime_raw(strand, "maximum call depth exceeded")
    }

    /// Create error: interrupt.  Interrupt errors are usually not catchable by
    /// Do programs.
    pub fn interrupt(strand: &Strand<'v, 's>, err: impl Into<Box<dyn error::Error>>) -> Self {
        Self::new_info(strand.inner, Boxed::Interrupt(err.into()))
    }

    /// Create error: canceled.
    pub fn canceled(#[allow(unused_variables)] strand: &Strand<'v, 's>) -> Self {
        Self::new_variant(Variant::Canceled)
    }

    pub(crate) fn canceled_raw(#[allow(unused_variables)] strand: &'s StrandInner<'v>) -> Self {
        Self::new_variant(Variant::Canceled)
    }

    pub(crate) fn non_local_jump(indicator: u8, weak: gc::Weak<'v, frame::Upvars<'v>>) -> Self {
        Self::new_variant(Variant::NonLocalJump(indicator, weak))
    }

    pub(crate) fn as_nl_branch(&self) -> Option<(u8, &gc::Weak<'v, frame::Upvars<'v>>)> {
        match &self.inner {
            Variant::NonLocalJump(indicator, weak) => Some((*indicator, weak)),
            _ => None,
        }
    }

    /// Get category of error
    pub fn kind(&self) -> ErrorKind {
        match &self.inner {
            Variant::Unsupported => ErrorKind::Unsupported,
            Variant::Immutable => ErrorKind::Immutable,
            Variant::Overflow => ErrorKind::Overflow,
            Variant::ZeroDiv => ErrorKind::ZeroDiv,
            Variant::SinkStop => ErrorKind::SinkStop,
            Variant::IterStop => ErrorKind::IterStop,
            Variant::Canceled => ErrorKind::Canceled,
            Variant::NonLocalJump(..) => ErrorKind::Runtime,
            Variant::Boxed(root, info) => {
                let val = root.get();
                match val.downcast_ref(info.vm.builtin_types().error) {
                    Some(boxed) => boxed.get().kind(),
                    None => ErrorKind::Runtime,
                }
            }
        }
    }

    /// Is error ordinarily catchable by Do programs?
    pub fn catchable(&self) -> bool {
        !matches!(self.inner, Variant::NonLocalJump(..))
            && !matches!(self.kind(), ErrorKind::Interrupt)
    }

    /// Iterate over backtrace associated with error, deepest entries first.
    ///
    /// Note that the backtrace for a propagating error will cease at the deepest live frame.
    /// Use [`Strand::backtrace`](crate::strand::Strand::backtrace) to continue the trace in that case.
    pub fn backtrace<'a>(&'a self) -> impl ExactSizeIterator<Item = impl Frame> + 'a {
        match &self.inner {
            Variant::Boxed(_, info) => BacktraceIter::new(info.vm, info.backtrace.iter()),
            _ => BacktraceIter::empty(),
        }
    }

    pub fn display<'a>(&'a self, strand: &'a mut Strand<'v, 's>) -> Display<'v, 's, 'a> {
        Display {
            error: self,
            strand: RefCell::new(strand),
        }
    }

    pub(crate) fn clone_backtrace(&mut self, strand: &Strand<'v, 's>) -> Vec<UnwindEntry<'v>> {
        if self.as_nl_branch().is_some() {
            return Vec::new();
        }
        loop {
            match &self.inner {
                Variant::Boxed(_, info) => return info.backtrace.clone(),
                _ => self.expand(strand.inner),
            }
        }
    }

    pub(crate) fn into_pair(mut self, strand: &Strand<'v, 's>) -> ErrorPair<'v> {
        assert!(
            self.as_nl_branch().is_none(),
            "non-local jumps cannot be paired"
        );
        while !matches!(self.inner, Variant::Boxed(..)) {
            self.expand(strand.inner);
        }
        match self.inner {
            Variant::Boxed(root, info) => (root.take(), info.backtrace),
            _ => unreachable!(),
        }
    }

    pub(crate) fn from_pair(
        strand: &Strand<'v, 's>,
        value: Value<'v>,
        backtrace: Vec<UnwindEntry<'v>>,
    ) -> Self {
        Self {
            inner: Variant::Boxed(
                FloatingRoot::new(strand.inner, value),
                Box::new(ErrorMeta {
                    vm: strand.inner.vm(),
                    backtrace,
                    sticky: Vec::new(),
                }),
            ),
            phantom: PhantomData,
        }
    }

    pub(crate) fn from_pair_ref(strand: &Strand<'v, 's>, pair: &ErrorPair<'v>) -> Self {
        Self::from_pair(strand, pair.0.dup(), pair.1.clone())
    }

    pub(crate) fn from_backtrace_value(
        strand: &Strand<'v, 's>,
        err: impl Input<'v>,
        backtrace: impl Input<'v>,
    ) -> Result<'v, 's, Self> {
        let backtrace = Value::from_input(strand, backtrace);
        Ok(Self::from_pair(
            strand,
            Value::from_input(strand, err),
            backtrace::entries_from_value(strand, &backtrace)?,
        ))
    }

    pub(crate) fn push_backtrace(&mut self, inner: &'s StrandInner<'v>, entry: UnwindEntry<'v>) {
        if self.as_nl_branch().is_some() {
            return;
        }
        loop {
            match &mut self.inner {
                Variant::Boxed(_, info) => {
                    info.backtrace.push(entry);
                    break;
                }
                _ => self.expand(inner),
            }
        }
    }

    pub(crate) fn push_sticky(&mut self, inner: &'s StrandInner<'v>, sticky: Gc<'v, Program<'v>>) {
        loop {
            match &mut self.inner {
                Variant::Boxed(_, info) => {
                    info.sticky.push(sticky);
                    break;
                }
                _ => self.expand(inner),
            }
        }
    }

    pub(crate) fn migrate<'ps>(self, strand: &Strand<'v, 'ps>) -> Error<'v, 'ps> {
        Error {
            inner: match self.inner {
                Variant::Boxed(root, info) => {
                    let val = root.take();
                    Variant::Boxed(FloatingRoot::new(strand.inner, val), info)
                }
                Variant::Unsupported => Variant::Unsupported,
                Variant::Immutable => Variant::Immutable,
                Variant::Overflow => Variant::Overflow,
                Variant::ZeroDiv => Variant::ZeroDiv,
                Variant::SinkStop => Variant::SinkStop,
                Variant::IterStop => Variant::IterStop,
                Variant::Canceled => Variant::Canceled,
                Variant::NonLocalJump(i, w) => Variant::NonLocalJump(i, w),
            },
            phantom: PhantomData,
        }
    }
}

/// [`std::error::Error`] extension trait.
pub trait ErrorExt {
    /// Convert into Do runtime error ([`Error::runtime`]).
    fn into_do<'v, 's>(self, strand: &Strand<'v, 's>) -> Error<'v, 's>;
}

impl<T: Into<Box<dyn error::Error + 'static>>> ErrorExt for T {
    fn into_do<'v, 's>(self, strand: &Strand<'v, 's>) -> Error<'v, 's> {
        Error::runtime(strand, self)
    }
}

/// [`std::result::Result`] extension trait.
pub trait ResultExt<T> {
    /// Convert error into Do runtime error ([`Error::runtime`]).
    fn into_do<'v, 's>(self, strand: &Strand<'v, 's>) -> Result<'v, 's, T>;
}

impl<T, E: ErrorExt> ResultExt<T> for result::Result<T, E> {
    fn into_do<'v, 's>(self, strand: &Strand<'v, 's>) -> Result<'v, 's, T> {
        self.map_err(|e| e.into_do(strand))
    }
}
