use std::{
    any::TypeId,
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{HashMap, hash_map::Entry},
    future::Future,
    marker::PhantomData,
    mem,
    ops::{Deref, DerefMut, Range},
    pin::Pin,
    ptr::NonNull,
    task::{Poll, Waker},
};

use dolang_util::{alias, mono::MonoVec};
use futures::{
    channel::mpsc,
    stream::{FuturesUnordered, StreamExt},
};

use crate::{
    Func, FuncDebug, Program, ProgramAnnex,
    arg::Args,
    bytecode::file,
    error::{Error, Result},
    frame::{CallFrame, Native},
    gc::{self, Gc, arena::Arena},
    object::{
        BuiltinTypes, Singletons, TypeTable,
        function::NativeFunction,
        module::{Native as NativeModule, NativeField},
        native::{Object, ObjectVtbl, Type, TypeBuilder},
        protocol::{GcObj, Header},
        sym::SymObj,
    },
    sig::{self, UnpackKey, UnpackKeyKind},
    stdlib,
    strand::{Local, LocalKey, LocalRootKey, LocalVtbl, Strand, StrandGroup, StrandInner},
    sym::{self, Sym},
    unpack,
    value::{Input, Output, Slot, Value},
};

/// A spawned background strand future.
pub(crate) type SpawnedFuture<'v> = Pin<Box<dyn Future<Output = ()> + 'v>>;

pub(crate) struct ImportGuard<'v> {
    pub(crate) owner: *const StrandInner<'v>,
    pub(crate) waiters: Vec<Waker>,
}

impl<'v> Drop for ImportGuard<'v> {
    fn drop(&mut self) {
        for waker in self.waiters.drain(..) {
            waker.wake()
        }
    }
}

pub(crate) enum ImportCacheEntry<'v> {
    Pending(ImportGuard<'v>),
    Ready(Value<'v>),
}

pub(crate) struct ErasedState {
    ptr: NonNull<()>,
    free: unsafe fn(NonNull<()>),
}

impl Drop for ErasedState {
    fn drop(&mut self) {
        unsafe { (self.free)(self.ptr) }
    }
}

pub trait Stateful<'v>: 'v {
    type Tag: 'static;
}

pub(crate) mod private {
    pub struct Sealed;
}

/// VM-scoped global state.
pub struct State<'v, T: 'v>(NonNull<T>, PhantomData<(&'v mut T, &'v mut &'v ())>);

impl<'v, T: 'v> Copy for State<'v, T> {}

impl<'v, T: 'v> Clone for State<'v, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'v, T: 'v> Deref for State<'v, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.as_ptr() }
    }
}

impl<'v, T: 'v> AsRef<T> for State<'v, T> {
    fn as_ref(&self) -> &T {
        unsafe { &*self.0.as_ptr() }
    }
}

/// Capability to allocate or otherwise perturb GC state in controlled ways.
///
/// Dyn-compatible, so a function can take `&mut dyn Alloc<'v>`. The generic operations live
/// in [`AllocExt`], which every `Alloc` implements.
pub trait Alloc<'v> {
    #[doc(hidden)]
    fn alloc_vm(&mut self, _: private::Sealed) -> &'v Vm<'v>;
}

/// Lazy setup operations, available on every [`Alloc`], including `dyn Alloc`.
pub trait AllocExt<'v>: Alloc<'v> {
    /// Run the lazy setup declared under tag `K` with [`Builder::lazy`], unless it has
    /// already run.
    ///
    /// Does nothing if no lazy setup is declared under `K`, so a crate can move between eager
    /// and lazy registration without affecting callers.
    fn force<K: 'static>(&mut self) {
        let vm = self.alloc_vm(private::Sealed);
        let unit = vm.lazy.borrow().by_tag.get(&TypeId::of::<K>()).copied();
        if let Some(unit) = unit {
            force_lazy_unit(self, unit);
        }
    }

    /// Run the lazy setup declared under `T::Tag`, then fetch the state registered with that
    /// tag.
    ///
    /// # Panics
    ///
    /// Panics if no state is registered under `T::Tag` once the setup has run.
    fn force_state<T: Stateful<'v>>(&mut self) -> State<'v, T> {
        self.force::<T::Tag>();
        self.alloc_vm(private::Sealed).state()
    }
}

impl<'v, A: Alloc<'v> + ?Sized> AllocExt<'v> for A {}

type LazyInit<'v> = Box<dyn FnOnce(&mut Register<'v>) + 'v>;

enum LazyPhase<'v> {
    Pending(LazyInit<'v>),
    Running,
    Done,
}

struct LazyUnit<'v> {
    tag_name: &'static str,
    modules: Box<[&'v str]>,
    phase: LazyPhase<'v>,
}

/// Setups declared with [`Builder::lazy`].
#[derive(Default)]
pub(crate) struct LazyTable<'v> {
    units: Vec<LazyUnit<'v>>,
    by_tag: HashMap<TypeId, usize>,
    pub(crate) by_module: HashMap<&'v str, usize>,
    /// Units whose setup is on the call stack, innermost last.
    running: Vec<usize>,
}

/// Run the setup of a lazy unit unless it has already run.
///
/// Requiring `Alloc` ties forcing to a context that may allocate.
pub(crate) fn force_lazy_unit<'v, A: Alloc<'v> + ?Sized>(alloc: &mut A, unit: usize) {
    let vm = alloc.alloc_vm(private::Sealed);
    let init = {
        let mut lazy = vm.lazy.borrow_mut();
        let lazy = &mut *lazy;
        let entry = &mut lazy.units[unit];
        match mem::replace(&mut entry.phase, LazyPhase::Running) {
            LazyPhase::Pending(init) => {
                lazy.running.push(unit);
                init
            }
            LazyPhase::Running => panic!("cyclic initialization of lazy unit {}", entry.tag_name),
            LazyPhase::Done => {
                entry.phase = LazyPhase::Done;
                return;
            }
        }
    };

    // Safety: `alloc` holds allocation capability for `vm`, and the register is only lent out
    // behind `&mut` for the duration of `init`
    let mut reg = unsafe { Register::new(vm) };
    init(&mut reg);

    let mut lazy = vm.lazy.borrow_mut();
    let lazy = &mut *lazy;
    lazy.running.pop();
    let entry = &mut lazy.units[unit];
    entry.phase = LazyPhase::Done;
    let native_modules = vm.native_modules.borrow();
    if let Some(name) = entry
        .modules
        .iter()
        .find(|name| !native_modules.contains_key(**name))
    {
        panic!(
            "lazy unit {} did not register declared module {name}",
            entry.tag_name
        );
    }
}

type Trap<'v> = dyn for<'s> Fn(&mut Strand<'v, 's>) -> Result<'v, 's, ()> + 'v;
type ChannelFactory<'v> = dyn for<'s> Fn(&mut Strand<'v, 's>, Slot<'v, '_>, Slot<'v, '_>) + 'v;

/// VM handle.
///
/// Many core operations require a VM handle, such as instantiating Do value types.
///
/// Other handles automatically dereference to this type and can be used in its place:
/// - [`Builder`]
/// - [`Register`]
/// - [`Strand`]
pub struct Vm<'v> {
    pub(crate) import_cache: RefCell<HashMap<String, ImportCacheEntry<'v>>>,
    pub(crate) native_modules: RefCell<HashMap<&'v str, Value<'v>>>,
    pub(crate) importers: MonoVec<Value<'v>>,
    pub(crate) pipe_handler: RefCell<Option<Box<ChannelFactory<'v>>>>,
    pub(crate) trap: RefCell<Option<Box<Trap<'v>>>>,
    // SAFETY: GC objects may point into state, so `arena` must be cleared first (but not dropped)
    pub(crate) state: RefCell<HashMap<TypeId, ErasedState>>,
    // SAFETY: must be unregistered after clearing GC objects and state, as this invalidates
    // all Sym<'v, 'v>
    pub(crate) symroots: MonoVec<GcObj<'v, SymObj>>,
    pub(crate) symtab: sym::Table<'v>,
    // SAFETY: must be dropped before arena, as it holds GC objects
    pub(crate) singletons: Singletons<'v>,
    // SAFETY: must be dropped before any vtbls
    pub(crate) arena: Arena<'v>,
    // SAFETY: this field is self-referential; so it must drop before `types`
    pub(crate) builtin_types: BuiltinTypes<'v>,
    pub(crate) types: TypeTable<'v>,
    /// Class object singletons for user-registered [`Object`] types.
    pub(crate) type_singletons: MonoVec<Value<'v>>,
    /// Instance vtbls of user-registered [`Object`] types that declared an error
    /// kind via a nominal supertype.
    ///
    /// A native error type carries its own representation rather than a boxed
    /// error, so [`crate::error::Error::kind`] classifies its instances by
    /// looking their vtbl up here. Only types that declared a kind appear, which
    /// in practice is a handful per VM.
    pub(crate) error_kind_vtbls: MonoVec<NonNull<ObjectVtbl<'v>>>,
    pub(crate) locals: MonoVec<LocalVtbl<'v>>,
    pub(crate) local_root_count: Cell<usize>,
    pub(crate) spawn_tx: RefCell<Option<mpsc::UnboundedSender<SpawnedFuture<'v>>>>,
    // Strings that have to be allocated for the lifetime of the VM
    pub(crate) strings: RefCell<Vec<alias::Box<str>>>,
    // SAFETY: pending setups may capture GC values, so this must be cleared before `arena`
    pub(crate) lazy: RefCell<LazyTable<'v>>,
}

impl<'v> Drop for Vm<'v> {
    fn drop(&mut self) {
        // Drop things in a safe order
        *self.lazy.get_mut() = Default::default();
        self.import_cache.get_mut().clear();
        self.native_modules.get_mut().clear();
        self.importers.drain().for_each(drop);
        *self.pipe_handler.get_mut() = None;
        self.type_singletons.drain().for_each(drop);
        // Close spawn channel (should already be None after enter() returns)
        self.spawn_tx.get_mut().take();
        // GC objects could point into state, so clear it before state
        self.arena.clear();
        // Anything that still points into state at this point is bound to be leaked
        self.state.get_mut().clear();
        self.symroots.drain().for_each(drop);
        self.symtab.clear();
    }
}

impl<'v> Vm<'v> {
    #[doc(hidden)]
    // For internal use only. Extensions may need to invalidate importer-provided
    // modules when their backing provider is unregistered.
    pub fn evict_import_cache(&self, name: &str) {
        self.import_cache.borrow_mut().remove(name);
    }

    /// Spawn a background task on the VM event loop.
    ///
    /// The task is polled alongside the main future passed to [`Builder::enter`]
    /// and any background strands spawned by the runtime. Tasks spawned this way
    /// run until completion, even if the caller that scheduled them has already
    /// returned.
    ///
    /// # Panics
    ///
    /// Panics if called after the VM has left [`Builder::enter`].
    pub fn spawn_task(&self, task: impl Future<Output = ()> + 'v) {
        self.spawn_tx
            .borrow()
            .as_ref()
            .expect("vm task spawned outside enter()")
            .unbounded_send(Box::pin(task))
            .expect("spawn channel closed");
    }

    pub(crate) fn string(&self, str: &str) -> &'v str {
        let str = alias::Box::new_str(str);
        let ptr = &raw const *str;
        self.strings.borrow_mut().push(str);
        unsafe { &*ptr }
    }

    /// Returns the approximate size of allocated GC objects in bytes.
    #[inline]
    pub fn gc_allocated_size(&self) -> usize {
        self.arena().allocated()
    }

    /// Fetch previously-registered state handle
    ///
    /// # Panics
    ///
    /// Panics if no state is registered under `T::Tag`. State registered by a lazy setup
    /// that hasn't run yet is not registered; use [`AllocExt::force_state`] to run it first.
    #[inline]
    pub fn state<T: Stateful<'v>>(&self) -> State<'v, T> {
        match self.try_state() {
            Some(state) => state,
            None => self.state_missing(TypeId::of::<T::Tag>()),
        }
    }

    /// Fetch previously-registered state handle, if any.
    ///
    /// Returns `None` for state registered by a lazy setup that hasn't run yet.
    #[inline]
    pub fn try_state<T: Stateful<'v>>(&self) -> Option<State<'v, T>> {
        self.state
            .borrow()
            .get(&TypeId::of::<T::Tag>())
            .map(|entry| State(entry.ptr.cast(), PhantomData))
    }

    #[cold]
    #[inline(never)]
    fn state_missing(&self, tag: TypeId) -> ! {
        let lazy = self.lazy.borrow();
        if let Some(&unit) = lazy.by_tag.get(&tag)
            && !matches!(lazy.units[unit].phase, LazyPhase::Done)
        {
            panic!(
                "state of lazy unit {} has not been initialized; use `AllocExt::force_state`",
                lazy.units[unit].tag_name
            )
        }
        panic!("state not registered")
    }

    /// Insert a native module, enforcing ownership of names declared by lazy units.
    fn insert_native_module(&self, name: &'v str, module: Value<'v>) {
        {
            let lazy = self.lazy.borrow();
            let owner = lazy.by_module.get(name).copied();
            let running = lazy.running.last().copied();
            match (owner, running) {
                (owner, running) if owner == running => {}
                (Some(owner), _) => panic!(
                    "module {name} is declared by lazy unit {}",
                    lazy.units[owner].tag_name
                ),
                (None, Some(running)) => panic!(
                    "lazy unit {} registered undeclared module {name}",
                    lazy.units[running].tag_name
                ),
                (None, None) => unreachable!(),
            }
        }
        let replaced = self.native_modules.borrow_mut().insert(name, module);
        drop(replaced);
    }

    pub(crate) fn arena(&self) -> &Arena<'v> {
        &self.arena
    }

    pub(crate) fn name_for_sym<'a>(&self, sym: Sym<'v, 'a>) -> &'a str {
        self.symtab.name(sym)
    }

    pub(crate) fn sym_register_obj(&self, name: &str) -> GcObj<'v, SymObj> {
        self.symtab
            .register(self.arena(), self.builtin_types.sym, name)
    }

    pub(crate) fn sym_register_unique_obj(&self, name: &str) -> GcObj<'v, SymObj> {
        self.symtab
            .register_unique(self.arena(), self.builtin_types.sym, name)
    }

    pub(crate) fn sym_obj(&self, sym: Sym<'v, '_>) -> GcObj<'v, SymObj> {
        self.symtab.obj(sym)
    }

    /// Collect cycles if enough allocations have accumulated, pruning dead
    /// symbols afterward.
    pub(crate) fn collect(&self) {
        if self.arena.collect() {
            self.symtab.gc();
        }
    }

    /// Unconditionally collect cycles and prune dead symbols.
    pub(crate) fn collect_full(&self) {
        self.arena.collect_full();
        self.symtab.gc();
    }

    pub(crate) fn builtin_types(&self) -> &BuiltinTypes<'v> {
        &self.builtin_types
    }

    pub(crate) fn singletons(&self) -> &Singletons<'v> {
        &self.singletons
    }

    fn slice_range(buffer: &[u8], slice: &[u8]) -> Option<Range<usize>> {
        let buffer_start = buffer.as_ptr().addr();
        let slice_start = slice.as_ptr().addr();

        let byte_start = slice_start.wrapping_sub(buffer_start);

        let start = byte_start;
        let end = start.wrapping_add(slice.len());

        if start <= buffer.len() && end <= buffer.len() {
            Some(start..end)
        } else {
            None
        }
    }

    /// Load and deserialize bytecode into a Program object.
    ///
    /// # Bytecode Loading Process
    ///
    /// This function transforms serialized bytecode into runtime data structures:
    ///
    /// 1. **Deserialize**: Parse the bytecode file format into structured tables
    /// 2. **Function Table**: Build function descriptors with bytecode ranges
    /// 3. **Symbol Table**: Register symbols in the VM's symbol table
    /// 4. **Constant Table**: Convert file constants into runtime Values
    /// 5. **Pack/Unpack Tables**: Build argument packing/unpacking specifications
    /// 6. **Debug Info**: Process source maps for stack traces
    ///
    /// # Table Transformation
    ///
    /// Each bytecode table is transformed for runtime efficiency:
    /// - `symtab`: File symbol indices → registered Symbol objects
    /// - `consttab`: Serialized constants → runtime Value objects
    /// - `packtab`: Argument patterns → sig::Pack specifications
    /// - `unpacktab`: Function signatures → sig::Unpack with default values
    /// - `sourcemap`: Delta-encoded offsets → (offset, line, file) tuples
    fn load_bytecode(
        &self,
        bytecode: Bytecode,
        importer: Value<'v>,
    ) -> dolang_bytecode::Result<Gc<'v, Program<'v>>> {
        let verified = file::deserialize(&bytecode.0)?;

        let funcs: alias::Box<_> = verified
            .functab
            .content
            .iter()
            .map(|e| {
                (
                    Func {
                        sig: e.func.sig,
                        locals: e.func.locals,
                        upvars: e.func.upvars.clone(),
                        bytecode: Self::slice_range(&bytecode.0, e.func.bytecode).unwrap(),
                    },
                    e.cert.max_operand_depth + e.func.locals,
                )
            })
            .collect();

        let symroots: Vec<_> = verified
            .symtab
            .content
            .iter()
            .map(|s| {
                let name = std::str::from_utf8(&verified.bintab.content[s.name.clone()])
                    .expect("verified UTF-8");
                if s.private {
                    self.sym_register_unique_obj(name)
                } else {
                    self.sym_register_obj(name)
                }
            })
            .collect();

        let symtab: Vec<_> = symroots
            .iter()
            .map(|s| unsafe { Sym::from_obj(s) })
            .collect();

        let consttab: alias::Box<_> = verified
            .consttab
            .content
            .iter()
            .map(|c| match c {
                file::Const::Nil => Value::NIL,
                file::Const::Int(v) => Value::from_int(self, *v),
                file::Const::VerbatimInt(v, file::StrId { start, end }) => {
                    let s = std::str::from_utf8(&verified.bintab.content[*start..*end])
                        .expect("verified UTF-8");
                    Value::from_int_verbatim(self, *v, s)
                }
                file::Const::F64(v) => Value::from_f64(self, *v),
                file::Const::VerbatimF64(v, file::StrId { start, end }) => {
                    let s = std::str::from_utf8(&verified.bintab.content[*start..*end])
                        .expect("verified UTF-8");
                    Value::from_f64_verbatim(self, *v, s)
                }
                file::Const::Bool(v) => Value::from_bool(*v),
                file::Const::Str(file::StrId { start, end }) => {
                    Value::from_object(gc::Base::upcast(unsafe {
                        gc::Base::from_header_utf8_slice(
                            &self.arena,
                            Header::new(self.arena(), self.builtin_types.str.vtbl),
                            &verified.bintab.content[*start..*end],
                        )
                    }))
                }
                file::Const::Bin(file::BinId { start, end }) => {
                    Value::from_u8_slice(self, &verified.bintab.content[*start..*end])
                }
                file::Const::Sym(idx) => Value::from_object(symroots[*idx].clone()),
            })
            .collect();

        let packtab: alias::Box<_> = verified
            .packtab
            .content
            .iter()
            .map(|s| {
                if s.iter().any(|p| matches!(p, dolang_bytecode::Arg::Pack)) {
                    sig::Pack::Var(
                        s.iter()
                            .map(|p| match p {
                                dolang_bytecode::Arg::Value => sig::Arg::Pos,
                                dolang_bytecode::Arg::Pack => sig::Arg::Expand,
                                dolang_bytecode::Arg::Key(id) => {
                                    sig::Arg::Key(unsafe { Sym::from_obj(&symroots[*id]) })
                                }
                            })
                            .collect(),
                    )
                } else {
                    sig::Pack::Fixed(
                        s.iter()
                            .map(|p| match p {
                                dolang_bytecode::Arg::Value => None,
                                dolang_bytecode::Arg::Pack => unreachable!(),
                                dolang_bytecode::Arg::Key(id) => {
                                    Some(unsafe { Sym::from_obj(&symroots[*id]) })
                                }
                            })
                            .collect(),
                    )
                }
            })
            .collect();

        let unpacktab: alias::Box<_> = verified
            .unpacktab
            .content
            .iter()
            .map(|u| {
                sig::Unpack::new(
                    u.required,
                    u.optional.iter().map(|d| consttab[*d].dup()).collect(),
                    u.keys
                        .iter()
                        .map(
                            |dolang_bytecode::file::UnpackKey { kind, default }| unsafe {
                                UnpackKey {
                                    kind: match kind {
                                        dolang_bytecode::file::UnpackKeyKind::Sym(idx) => {
                                            UnpackKeyKind::Sym(Sym::from_obj(&symroots[*idx]))
                                        }
                                        dolang_bytecode::file::UnpackKeyKind::Const(idx) => {
                                            UnpackKeyKind::Const(consttab[*idx].dup())
                                        }
                                    },
                                    default: default.map(|d| consttab[d].dup()),
                                }
                            },
                        )
                        .collect(),
                    u.variadic,
                )
            })
            .collect::<Vec<_>>()
            .into();

        let debugbintab = Self::slice_range(&bytecode.0, verified.debugbintab.content).unwrap();

        let module_name = verified.module_name.clone().map(|id| id.start..id.end);

        let funcdebugs = verified
            .funcdebugtab
            .content
            .iter()
            .map(|debug| {
                let mut sourcemap = Vec::new();
                let mut iter = debug.sourcemap.iter();
                let first = iter.next().expect("empty source map?!");
                let mut offset = 0;
                let mut line = first.line_delta;
                sourcemap.push((offset, line as u32, first.file.clone()));
                for entry in iter {
                    offset += entry.offset_delta + 1;
                    line += entry.line_delta;
                    sourcemap.push((offset, line as u32, entry.file.clone()))
                }
                FuncDebug {
                    name: debug.name.start..debug.name.end,
                    sourcemap: sourcemap.into(),
                }
            })
            .collect();

        let loaded = Gc::new_with_annex(
            self.arena(),
            Program { importer },
            ProgramAnnex {
                bytecode: bytecode.0,
                funcs,
                symroots,
                symtab,
                consttab,
                packtab,
                unpacktab,
                debugbintab,
                funcdebugs,
                module_name,
            },
        );

        Ok(loaded)
    }
}

/// Builder for configuring native modules.
///
/// # Native Module Creation
///
/// Native modules allow embedding Rust code into the Do runtime. This builder
/// provides a convenient API for defining functions and values that will be
/// available to Do code.
///
/// # Example
///
/// ```ignore
/// builder.module("my_module")
///     .function("greet", async |strand, args, out| {
///         Output::set(strand, out, "Hello!");
///         Ok(())
///     })
///     .commit();
/// ```
#[must_use]
pub struct ModuleBuilder<'v, 'a> {
    name: &'v str,
    vm: &'a mut Register<'v>,
    contents: Vec<(Sym<'v, 'v>, NativeField<'v>)>,
}

impl<'v, 'a> ModuleBuilder<'v, 'a> {
    #[inline(never)]
    fn push(&mut self, sym: Sym<'v, 'v>, field: NativeField<'v>) {
        self.contents.push((sym, field));
    }

    /// Create a native function in the module with the given name.
    ///
    /// ## Function Signature
    ///
    /// The function must be async and accept three parameters:
    /// - `strand: &mut Strand<'v, 's>` - The current execution strand for async operations
    /// - `args: Args<'v, 'b>` - Arguments passed from the Do code
    /// - `out: Slot<'v, 'b>` - Output slot where the return value should be stored
    ///
    /// The function must return `Result<'v, 's, ()>` where:
    /// - `Ok(())` indicates success
    /// - `Err(...)` propagates an error to the Do code
    ///
    /// ## Example
    ///
    /// ```ignore
    /// .function("add", async |strand, args, out| {
    ///     let ([a, b], []) = unpack!(strand, args, 2, 0)?;
    ///     let a = a.as_i64(strand).ok_or_else(|| Error::type_error(strand, "expected Int"))?;
    ///     let b = b.as_i64(strand).ok_or_else(|| Error::type_error(strand, "expected Int"))?;
    ///     Output::set(strand, out, a + b);
    ///     Ok(())
    /// })
    /// ```
    ///
    /// ## Type Parameters
    ///
    /// - `F`: The async function type.
    pub fn function<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> AsyncFn(
                &mut Strand<'v, 's>,
                Args<'v, 'b>,
                Slot<'v, 'b>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        let sym = self.vm.sym(name);
        let vtbl = self.vm.inner.builtin_types.native_function;
        let name = self.vm.inner.string(name);
        let func = NativeFunction::new(f, self.name, name);
        self.push(
            sym,
            NativeField::Value(Value::from_object(GcObj::new(
                self.vm.inner.arena(),
                vtbl,
                func,
            ))),
        );
        self
    }

    pub(crate) fn function_without_frame<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> AsyncFn(
                &mut Strand<'v, 's>,
                Args<'v, 'b>,
                Slot<'v, 'b>,
            ) -> Result<'v, 's, ()>
            + 'v,
    {
        let sym = self.vm.sym(name);
        let vtbl = self.vm.inner.builtin_types.native_function;
        let name = self.vm.inner.string(name);
        let func = NativeFunction::without_frame(f, self.name, name);
        self.push(
            sym,
            NativeField::Value(Value::from_object(GcObj::new(
                self.vm.inner.arena(),
                vtbl,
                func,
            ))),
        );
        self
    }

    /// Create a native function with scratch [`Slot`]s for temporary values.
    ///
    /// This is a convenience wrapper around [`ModuleBuilder::function`] that
    /// automatically calls [`Strand::with_slots`] before invoking your function.
    ///
    /// ## Example
    ///
    /// ```ignore
    /// strand.function_with_slots("process", async |strand, args, out, [mut temp1, mut temp2]| {
    ///     // temp1 and temp2 are available for use
    /// })
    /// ```
    pub fn function_with_slots<const N: usize>(
        self,
        name: &'v str,
        f: impl for<'b, 's> AsyncFn(
            &mut Strand<'v, 's>,
            Args<'v, 'b>,
            Slot<'v, 'b>,
            [Slot<'v, 'b>; N],
        ) -> Result<'v, 's, ()>
        + 'v,
    ) -> Self {
        self.function(name, async move |strand, args, out| {
            strand
                .with_slots(async |strand, slots| f(strand, args, out, slots).await)
                .await
        })
    }

    /// Add a constant value to the module.
    ///
    /// # Example
    ///
    /// ```ignore
    /// builder.value("PI", std::f64::consts::PI)
    /// builder.value("version", "1.0.0")
    /// ```
    pub fn value(mut self, name: &str, value: impl Input<'v>) -> Self {
        let sym = self.vm.sym(name);
        self.push(
            sym,
            NativeField::Value(Value::from_input(self.vm.inner, value)),
        );
        self
    }

    /// Register a computed value getter in the module.
    ///
    /// The closure is invoked each time the field is read.
    pub fn get<F>(mut self, name: &str, f: F) -> Self
    where
        F: for<'b, 's> Fn(&mut Strand<'v, 's>, Slot<'v, 'b>) -> Result<'v, 's, ()> + 'v,
    {
        let sym = self.vm.sym(name);
        self.push(sym, NativeField::Getter(Box::new(f)));
        self
    }

    /// Register a computed value getter with scratch [`Slot`]s for temporary values.
    pub fn get_with_slots<const N: usize>(
        self,
        name: &str,
        f: impl for<'b, 's> Fn(
            &mut Strand<'v, 's>,
            Slot<'v, 'b>,
            [Slot<'v, 'b>; N],
        ) -> Result<'v, 's, ()>
        + 'v,
    ) -> Self {
        self.get(name, move |strand, out| {
            strand.with_slots_sync(|strand, slots| f(strand, out, slots))
        })
    }

    /// Create a native object in the module
    pub fn object<T: Object<'v>>(mut self, name: &str, ty: Type<'v, T>, value: T) -> Self
    where
        T::Annex: Default,
    {
        let sym = self.vm.sym(name);
        self.push(
            sym,
            NativeField::Value(ty.create_raw(self.vm, value, Default::default())),
        );
        self
    }

    /// Create a native object in the module with an annex
    pub fn object_with_annex<T: Object<'v>>(
        mut self,
        name: &str,
        ty: Type<'v, T>,
        value: T,
        annex: T::Annex,
    ) -> Self {
        let sym = self.vm.sym(name);
        self.push(
            sym,
            NativeField::Value(ty.create_raw(self.vm, value, annex)),
        );
        self
    }

    /// Register the module with the VM.
    ///
    /// # Returns
    ///
    /// Returns a reference to the [`Register`] to allow method chaining for
    /// additional configuration.
    ///
    /// # Example
    ///
    /// ```ignore
    /// vm.module("http")
    ///     .function("get", http_get)
    ///     .commit();
    /// // Module is now available to Do code
    /// ```
    pub fn commit(self) -> &'a mut Register<'v> {
        let mut items = self.contents;
        items.sort_by_key(|(sym, _)| *sym);
        for pair in items.windows(2) {
            if pair[0].0 == pair[1].0 {
                panic!(
                    "duplicate native module member {}.{}",
                    self.name,
                    pair[0].0.as_str(self.vm.inner),
                );
            }
        }

        let module = NativeModule::new(self.name, items);
        let module = Value::from_object(GcObj::new(
            self.vm.inner.arena(),
            self.vm.inner.builtin_types.native_module,
            module,
        ));
        self.vm.inner.insert_native_module(self.name, module);
        self.vm
    }
}

/// Handle for registering symbols, state, native types, and native modules.
///
/// [`Builder`] and [`TypeBuilder`] dereference to this type, so setup code that only
/// registers things should take `&mut Register<'v>`.
///
/// A `Register` is only ever lent out behind `&mut`. It implements [`Alloc`], so holding one
/// by value would permit allocation outside a context that owns that capability.
pub struct Register<'v> {
    pub(crate) inner: &'v Vm<'v>,
}

impl<'v> Register<'v> {
    /// # Safety
    ///
    /// The result must only be lent out as `&mut Register` from a context that already holds
    /// allocation capability for `vm`.
    pub(crate) unsafe fn new(vm: &'v Vm<'v>) -> Self {
        Self { inner: vm }
    }
}

impl<'v> Alloc<'v> for Register<'v> {
    fn alloc_vm(&mut self, _: private::Sealed) -> &'v Vm<'v> {
        self.inner
    }
}

impl<'v> Deref for Register<'v> {
    type Target = Vm<'v>;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'v> AsRef<Vm<'v>> for Register<'v> {
    fn as_ref(&self) -> &Vm<'v> {
        self.inner
    }
}

/// Virtual machine builder.
///
/// Dereferences to [`Register`] for registration. Configuration that affects every strand
/// (strand-local keys, importers, traps) is only available here.
pub struct Builder<'v> {
    reg: Register<'v>,
}

impl<'v> Alloc<'v> for Builder<'v> {
    fn alloc_vm(&mut self, _: private::Sealed) -> &'v Vm<'v> {
        self.reg.inner
    }
}

impl<'v> Deref for Builder<'v> {
    type Target = Register<'v>;

    fn deref(&self) -> &Self::Target {
        &self.reg
    }
}

impl<'v> DerefMut for Builder<'v> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reg
    }
}

impl<'v> AsRef<Vm<'v>> for Builder<'v> {
    fn as_ref(&self) -> &Vm<'v> {
        self.reg.inner
    }
}

impl Builder<'static> {
    /// Build new VM.  The provided function is called with a handle which can be
    /// used to configure and enter the VM.
    pub async fn build<R>(f: impl for<'v> AsyncFnOnce(&mut Builder<'v>) -> R) -> R {
        let mut types = TypeTable::new();

        let builtin_types = BuiltinTypes::new(&mut types);

        let arena = Arena::new();
        let symtab = sym::Table::new(
            // Safety: this transmute represents the promise that anything allocated
            // when constructing the table will remain valid for `'v`; the constructor
            // doesn't actually capture this arena reference, so it's OK that it's moved
            // into the VM below
            unsafe { mem::transmute::<&Arena<'_>, &Arena<'_>>(&arena) },
            builtin_types.sym,
        );

        // Create builtin class singleton objects
        let builtin_classes = Singletons::new(&arena, &builtin_types);

        let vm = Vm {
            arena,
            builtin_types,
            singletons: builtin_classes,
            types,
            symtab,
            symroots: Default::default(),
            state: Default::default(),
            native_modules: Default::default(),
            importers: Default::default(),
            pipe_handler: Default::default(),
            trap: Default::default(),
            import_cache: Default::default(),
            locals: Default::default(),
            local_root_count: Cell::new(0),
            spawn_tx: Default::default(),
            strings: Default::default(),
            type_singletons: Default::default(),
            error_kind_vtbls: Default::default(),
            lazy: Default::default(),
        };

        // Safety: VM is kept alive for the same duration as its contents, as it's self-referential.
        // `vm` is not moved after this point and outlives `this`, which is the only way the
        // reference escapes; every `'v`-branded value is confined to `f`, which returns before
        // `vm` is dropped
        // The builder holds allocation capability, so lending out its `Register` is sound
        let mut this = Builder {
            reg: unsafe {
                Register::new(mem::transmute::<&Vm<'static>, &'static Vm<'static>>(&vm))
            },
        };

        stdlib::configure(&mut this);
        f(&mut this).await
    }
}

impl<'v> Register<'v> {
    /// Resolve a name to a symbol. The returned symbol will live for the life of the VM.
    #[inline(never)]
    pub fn sym(&mut self, name: &str) -> Sym<'v, 'v> {
        let root =
            self.inner
                .symtab
                .register(self.inner.arena(), self.inner.builtin_types().sym, name);
        // SAFETY: symroots keeps symbol rooted indefinitely (until VM is dropped).
        let sym = unsafe { Sym::from_obj(&root) };
        self.inner.symroots.push(root);
        sym
    }

    /// Register custom state that will live for the life of the VM, and can therefore be
    /// referenced by native objects, functions, and modules, etc.
    pub fn register_state<T: Stateful<'v>>(&mut self, value: T) -> State<'v, T> {
        match self.inner.state.borrow_mut().entry(TypeId::of::<T::Tag>()) {
            Entry::Occupied(_) => panic!("duplicate state registration"),
            Entry::Vacant(entry) => {
                let state = alias::Box::into_non_null(alias::Box::new(value));
                entry.insert(ErasedState {
                    ptr: state.cast(),
                    free: |ptr| {
                        let _ = unsafe { alias::Box::from_non_null(ptr.cast::<T>()) };
                    },
                });
                State(state, PhantomData)
            }
        }
    }

    /// Register a native object type.
    ///
    /// Once registered, native objects can be instantiated with [`Type::create`]. Native objects
    /// can also be registered as module items with [`ModuleBuilder::object`], or as entire
    /// modules with [`Register::module_object`].
    ///
    /// The type's class object singleton is initialized with default values for `T::Type` and
    /// `T::TypeAnnex`.
    ///
    /// Use [`Register::build_type`] when you need to customize registration before committing it.
    pub fn register_type<T: Object<'v>>(&mut self) -> Type<'v, T>
    where
        T::Type: Default,
        T::TypeAnnex: Default,
    {
        self.build_type(Default::default(), Default::default())
            .build()
    }

    /// Begin registering a native object type with explicit class object state and annex.
    ///
    /// The returned [`TypeBuilder`] has already been passed through [`Object::build`]. Finish
    /// registration with [`TypeBuilder::build`].
    ///
    /// See [`Register::register_type`] for the common case where `T::Type` and `T::TypeAnnex`
    /// are both `Default`.
    pub fn build_type<T: Object<'v>>(
        &mut self,
        value: T::Type,
        annex: T::TypeAnnex,
    ) -> TypeBuilder<'v, '_, T> {
        T::build(TypeBuilder::<T>::new(self, value, annex))
    }

    /// Registers a native module which may be imported by Do code with the provided name.
    /// The returned builder object must be used to configure the module and finished
    /// with [`ModuleBuilder::commit`].
    ///
    /// The contents of native modules are immutable.
    pub fn module<'a>(&'a mut self, name: &'a str) -> ModuleBuilder<'v, 'a> {
        ModuleBuilder {
            name: self.inner.string(name),
            vm: self,
            contents: Default::default(),
        }
    }

    /// Registers a native object as an importable module with the provided name.  This
    /// allows full control over the behavior of the module.  The object must implement
    /// [`Object::get`] in order for item imports to succeed.
    pub fn module_object<T: Object<'v>>(
        &mut self,
        name: &str,
        ty: &Type<'v, T>,
        value: T,
    ) -> &mut Self
    where
        T::Annex: Default,
    {
        let name = self.inner.string(name);
        let module = ty.create_raw(self.inner, value, Default::default());
        self.inner.insert_native_module(name, module);
        self
    }

    /// Registers a native object with an annex as an importable module with the provided name.
    pub fn module_object_with_annex<T: Object<'v>>(
        &mut self,
        name: &str,
        ty: &Type<'v, T>,
        value: T,
        annex: T::Annex,
    ) -> &mut Self {
        let name = self.inner.string(name);
        let module = ty.create_raw(self.inner, value, annex);
        self.inner.insert_native_module(name, module);
        self
    }
}

impl<'v> Builder<'v> {
    /// Register strand-local state key.
    pub fn local<T: Local<'v>>(&mut self) -> LocalKey<'v, T> {
        let index = self.reg.inner.locals.len();
        let vtbl = LocalVtbl::new::<T>();
        self.reg.inner.locals.push(vtbl);
        // Safety: index matches position of vtbl in vector
        unsafe { LocalKey::new(index) }
    }

    /// Register a strand-local GC root key.
    pub fn local_root(&mut self) -> LocalRootKey<'v> {
        let index = self.reg.inner.local_root_count.get();
        self.reg.inner.local_root_count.set(index + 1);
        LocalRootKey::new(index)
    }

    /// Declare a lazy setup under tag `K`.
    ///
    /// `init` runs at most once: when Do code imports one of `modules`, or when Rust code
    /// calls [`AllocExt::force`] with `K`. Declaring the setup under the `Tag` of the state it
    /// registers lets [`AllocExt::force_state`] run it and fetch that state in one step.
    ///
    /// `init` must register every module in `modules`, and no other modules. Strand-local keys,
    /// importers, and traps are only available on `Builder`; reserve them before declaring the
    /// setup and move them into `init`.
    ///
    /// # Panics
    ///
    /// Panics if a setup is already declared under `K`, or if a module in `modules` is already
    /// registered or declared by another setup.
    pub fn lazy<K: 'static>(
        &mut self,
        modules: &[&str],
        init: impl FnOnce(&mut Register<'v>) + 'v,
    ) -> &mut Self {
        let vm = self.reg.inner;
        let tag_name = std::any::type_name::<K>();
        let mut lazy = vm.lazy.borrow_mut();
        let lazy = &mut *lazy;
        let unit = lazy.units.len();
        match lazy.by_tag.entry(TypeId::of::<K>()) {
            Entry::Occupied(_) => panic!("duplicate lazy unit {tag_name}"),
            Entry::Vacant(entry) => {
                entry.insert(unit);
            }
        }
        let native_modules = vm.native_modules.borrow();
        let modules = modules
            .iter()
            .map(|name| {
                if native_modules.contains_key(*name) || lazy.by_module.contains_key(*name) {
                    panic!("module {name} declared by lazy unit {tag_name} is already registered");
                }
                let name = vm.string(name);
                lazy.by_module.insert(name, unit);
                name
            })
            .collect();
        lazy.units.push(LazyUnit {
            tag_name,
            modules,
            phase: LazyPhase::Pending(Box::new(init)),
        });
        drop(native_modules);
        self
    }

    // Internal function for dolang-ext-shell only
    #[doc(hidden)]
    pub fn pipe_handler(
        &mut self,
        factory: impl for<'s> Fn(&mut Strand<'v, 's>, Slot<'v, '_>, Slot<'v, '_>) + 'v,
    ) -> &mut Self {
        *self.reg.inner.pipe_handler.borrow_mut() = Some(Box::new(factory));
        self
    }

    /// Registers a module importer function.  Do `import` statements check 3 sources of modules
    /// in order:
    ///
    /// 1. Native modules (registered with [`Register::module`] or [`Register::module_object`]).
    /// 2. Cached, previously imported Do modules.
    /// 3. Module importers in order of registration.  If any succeed, the module is cached so long as it
    ///    remains referenced.
    ///
    /// A typical importer should try the following sequence of steps :
    /// 1. If applicable, locate cached bytecode for the named module
    ///     - Try to run the bytecode
    ///     - If running the bytecode fails with a bytecode error (e.g. version mismatch),
    ///       treat the cache as expired
    /// 2. If cached bytecode is not available, locate and compile Do source for the named module
    ///     - A typical organization scheme is to replace `.` in the module name with path separators,
    ///       append `.dol` to the result, then search one or more source paths for the resulting
    ///       relative path.
    ///     - Compile the source code in module mode with the Do `compile` module.
    ///     - If this fails, log any emitted diagnostics and return [`Error::compile()`].
    ///     - Try to run the bytecode
    /// 3. On success, pass back the verbatim result of [`Bytecode::run`] in `out` on success,
    ///    or the returned error on failure.
    ///
    /// Of course, you're free to construct and return any sort of result or error whatsoever.
    /// Note that `import` statements that import individual items from modules do so through
    /// field access (equivalent to `.field` syntax).
    ///
    /// # Arguments
    /// - `import`: takes 3 arguments
    ///   * `strand`: the current strand
    ///   * `name`: the name of the module, conventionally in lower-case dotted form, e.g. `foo.bar.baz`
    ///   * `out`: an output slot to fill with the result
    pub fn importer(
        &mut self,
        import: impl for<'b, 's> AsyncFn(
            &'b mut Strand<'v, 's>,
            &'b str,
            Slot<'v, 'b>,
        ) -> Result<'v, 's, ()>
        + 'v,
    ) -> &mut Self {
        let vtbl = self.reg.inner.builtin_types.native_function;
        self.reg.inner.importers.push(Value::from_object(GcObj::new(
            self.reg.inner.arena(),
            vtbl,
            NativeFunction::new(
                async move |strand, args, out| {
                    let ([name], _) = unpack!(strand, args, 1, 0)?;
                    let name = name.to_string(strand)?;
                    import(strand, &name, out).await
                },
                "<host>",
                "import",
            ),
        )));
        self
    }

    /// Set trap function. The trap function will be called periodically during
    /// execution of the VM and may return an error. In particular, [`Error::abort`]
    /// creates an error that ordinary Do programs can't catch, forcing unwinding back
    /// into host frames.  This may be used to enforce timeouts to prevent scripts from
    /// looping infinitely, for example.
    ///
    /// When the trap function is called is not precisely specified, but there's guaranteed
    /// to be a constant upper bound on Do program instructions executed between invocations.
    /// Standard prelude functions will ensure this guarantee is respected by not performing
    /// unbounded work on behalf of Do programs without periodic trap checks.
    pub fn trap(
        &mut self,
        trap: impl for<'s> Fn(&mut Strand<'v, 's>) -> Result<'v, 's, ()> + 'v,
    ) -> &mut Self {
        *self.reg.inner.trap.borrow_mut() = Some(Box::new(trap));
        self
    }

    /// Finalize configuration and enter VM.  The given async function is invoked with a
    /// [`Strand`] which can be used for further operations. For example, a [`Bytecode`] object
    /// can be run to obtain its return value. The result of the function is returned.
    pub async fn enter<R>(&mut self, f: impl AsyncFnOnce(&mut Strand<'v, '_>) -> R) -> R {
        let (tx, mut rx) = mpsc::unbounded();
        *self.reg.inner.spawn_tx.borrow_mut() = Some(tx);

        let group = StrandGroup::new();
        let strand = StrandInner::new(self.reg.inner, None);
        let _guard = unsafe { strand.init_group_leader(&group) };
        let native = Native {
            module: "<host>".into(),
            receiver: "<enter>".into(),
            method: None,
            parent: None,
        };
        let mut strand = unsafe { Strand::from_native_frame(&strand, &native) };

        let mut background = FuturesUnordered::new();

        // Run main future, polling background tasks alongside it
        let res = {
            let main_fut = f(&mut strand);
            futures::pin_mut!(main_fut);

            futures::future::poll_fn(|cx| {
                // Drain spawn channel
                while let Poll::Ready(Some(task)) = rx.poll_next_unpin(cx) {
                    background.push(task);
                }
                // Poll background tasks
                while let Poll::Ready(Some(())) = background.poll_next_unpin(cx) {}
                // Poll main future
                main_fut.as_mut().poll(cx)
            })
            .await
        };

        // Close spawn channel
        *self.reg.inner.spawn_tx.borrow_mut() = None;

        // Cancel all join handles so orphaned background strands can unwind
        self.reg.inner.arena.cancel_join_handles();

        // Drain remaining background tasks
        while let Ok(task) = rx.try_recv() {
            background.push(task);
        }
        while background.next().await.is_some() {}

        self.reg.inner.collect_full();
        res
    }

    /// Finalize and enter VM with additional scratch [`Slot`]s.
    ///
    /// Combines [`Builder::enter`] with [`Strand::with_slots`].
    pub async fn enter_with_slots<const N: usize, R>(
        &mut self,
        f: impl for<'a> AsyncFnOnce(&mut Strand<'v, '_>, [Slot<'v, 'a>; N]) -> R,
    ) -> R {
        self.enter(async move |strand| {
            strand
                .with_slots(async move |strand, slots| f(strand, slots).await)
                .await
        })
        .await
    }
}

/// Bytecode
///
/// Ready to be consumed by a VM with [`Bytecode::run`].
pub struct Bytecode(pub(crate) Cow<'static, [u8]>);

impl Bytecode {
    /// Create bytecode from raw bytes
    pub fn new(bytes: impl Into<Cow<'static, [u8]>>) -> Self {
        Self(bytes.into())
    }
}

impl Bytecode {
    /// Run bytecode
    ///
    /// For Do code compiled in script mode, the result (if not a raised error) is that of the
    /// final top-level statement, or any top-level early `return`.  For module mode, it's a
    /// module containing all exported bindings, or the the value of an early `return`.
    ///
    /// The bytecode is gifted to the VM.
    ///
    /// # Arguments
    /// - `strand`: current strand
    /// - `out`: set to the returned value on success
    pub async fn run<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        self.run_inner(strand, Value::NIL, out).await
    }

    /// Run bytecode with a program-bound import handler.
    pub async fn run_with_importer<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        importer: impl Input<'v>,
        out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let importer = Value::from_input(strand.vm(), importer);
        self.run_inner(strand, importer, out).await
    }

    async fn run_inner<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        importer: Value<'v>,
        mut out: impl Output<'v>,
    ) -> Result<'v, 's, ()> {
        let loaded = strand
            .load_bytecode(self, importer)
            .map_err(|e| Error::bytecode(strand, e))?;
        let mut frame = unsafe { CallFrame::new(loaded, 0, None, None) };
        strand
            .run(strand.inner, &mut frame, Slot::from_output(&mut out))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::test_support::with_builder;

    struct TagA;
    struct TagB;

    struct TestState {
        value: i64,
    }

    impl<'v> Stateful<'v> for TestState {
        type Tag = TagA;
    }

    #[test]
    fn lazy_unit_runs_once_on_import() {
        with_builder(async |vm| {
            let runs = Rc::new(Cell::new(0));
            let counter = runs.clone();
            vm.lazy::<TagA>(&["lazy_test"], move |reg| {
                counter.set(counter.get() + 1);
                reg.register_state(TestState { value: 7 });
                reg.module("lazy_test").value("x", 7_i64).commit();
            });
            assert_eq!(runs.get(), 0);
            vm.enter_with_slots(async move |strand, [mut out]| {
                assert!(strand.try_state::<TestState>().is_none());
                strand.import("lazy_test", &mut out).await.unwrap();
                strand.import("lazy_test", &mut out).await.unwrap();
                assert_eq!(runs.get(), 1);
                assert_eq!(strand.state::<TestState>().value, 7);
            })
            .await
        });
    }

    #[test]
    fn force_state_runs_setup() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&[], |reg| {
                reg.register_state(TestState { value: 3 });
            });
            vm.enter(async |strand| {
                assert!(strand.try_state::<TestState>().is_none());
                assert_eq!(strand.force_state::<TestState>().value, 3);
                assert_eq!(strand.try_state::<TestState>().unwrap().value, 3);
                // Forcing again is a no-op
                strand.force::<TagA>();
            })
            .await
        });
    }

    #[test]
    fn force_without_lazy_unit_is_noop() {
        with_builder(async |vm| {
            vm.register_state(TestState { value: 5 });
            assert_eq!(vm.force_state::<TestState>().value, 5);
        });
    }

    #[test]
    #[should_panic(expected = "has not been initialized")]
    fn state_before_force_panics() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&[], |reg| {
                reg.register_state(TestState { value: 1 });
            });
            vm.state::<TestState>();
        });
    }

    #[test]
    #[should_panic(expected = "cyclic initialization")]
    fn cyclic_force_panics() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&[], |reg| reg.force::<TagB>());
            vm.lazy::<TagB>(&[], |reg| reg.force::<TagA>());
            vm.force::<TagA>();
        });
    }

    #[test]
    #[should_panic(expected = "did not register declared module")]
    fn missing_declared_module_panics() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&["lazy_missing"], |_| {});
            vm.force::<TagA>();
        });
    }

    #[test]
    #[should_panic(expected = "registered undeclared module")]
    fn undeclared_module_panics() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&[], |reg| {
                reg.module("lazy_undeclared").commit();
            });
            vm.force::<TagA>();
        });
    }

    #[test]
    #[should_panic(expected = "is already registered")]
    fn declaring_registered_module_panics() {
        with_builder(async |vm| {
            vm.module("lazy_taken").commit();
            vm.lazy::<TagA>(&["lazy_taken"], |_| {});
        });
    }

    #[test]
    #[should_panic(expected = "is already registered")]
    fn declaring_module_of_other_unit_panics() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&["lazy_taken"], |_| {});
            vm.lazy::<TagB>(&["lazy_taken"], |_| {});
        });
    }

    #[test]
    #[should_panic(expected = "is declared by lazy unit")]
    fn committing_module_of_lazy_unit_panics() {
        with_builder(async |vm| {
            vm.lazy::<TagA>(&["lazy_owned"], |_| {});
            vm.module("lazy_owned").commit();
        });
    }
}
