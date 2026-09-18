use std::{
    borrow::Cow,
    hash::{DefaultHasher, Hasher},
    mem,
    ops::ControlFlow,
};

use crate::value::fmt::Format;

use dolang_util::alias;

use crate::{
    Program,
    arg::Args,
    error::{Error, ErrorKind, Result},
    frame::Upvars,
    gc::{Collect, Gc, arena::Visit},
    object::{
        iter,
        protocol::{GcObj, Protocol, Recv},
        tuple,
    },
    strand::Strand,
    sym::Sym,
    value::{Output, Slot, TypeObject, Value},
    vm::Vm,
};

use super::dict::Dict;

pub(crate) type ModuleGetter<'v> =
    dyn for<'a, 's> Fn(&mut Strand<'v, 's>, Slot<'v, 'a>) -> Result<'v, 's, ()> + 'v;

pub(crate) enum NativeField<'v> {
    Value(Value<'v>),
    Getter(Box<ModuleGetter<'v>>),
}

pub(crate) struct Module<'v> {
    pub(crate) loaded: Gc<'v, Program<'v>>,
    upvars: Gc<'v, Upvars<'v>>,
    // Conceptually the 'static lifetime here is really the lifetime of the loaded bytecode,
    // which `loaded` roots for us
    // FIXME: it would be nice for this to be a flexible array member instead
    map: alias::Box<[(Sym<'v, 'static>, usize)]>,
}

unsafe impl<'v> Collect for Module<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.loaded.accept(visit)?;
        self.upvars.accept(visit)
    }

    fn clear(&mut self) {
        // Can't actually clear this structure, but:
        // - compiled isn't cyclic
        // - upvars can clear itself
    }
}

impl<'v> Module<'v> {
    // Safety:
    // - `upvars` and `syms` must have the same arity
    // - `syms` must be rooted by `loaded`
    pub(crate) unsafe fn from_upvars_syms<'a, 's>(
        loaded: Gc<'v, Program<'v>>,
        upvars: Gc<'v, Upvars<'v>>,
        syms: impl IntoIterator<Item = Option<Sym<'v, 'a>>>,
    ) -> Self {
        let mut this = Self {
            map: syms
                .into_iter()
                .enumerate()
                .filter_map(|(i, s)| s.map(|s| (unsafe { s.into_static_scope_unchecked() }, i)))
                .collect(),
            upvars,
            loaded,
        };

        this.map.sort_by_key(|(s, _)| *s);

        this
    }
}

impl<'v> Protocol<'v> for Module<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().module)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        let name = borrow
            .loaded
            .annex()
            .module_name
            .as_ref()
            .map(|r| &borrow.loaded.annex().debug_strtab()[r.clone()])
            .unwrap_or("?");
        crate::fmt!(strand, w, "{name}")
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<")?;
        Self::op_display(this, strand, w)?;
        crate::fmt!(strand, w, ">")
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let re = this.get();
        match re.map.binary_search_by_key(&field, |(s, _)| *s) {
            Ok(index) => {
                unsafe {
                    out.store(
                        re.upvars
                            .borrow()
                            .expect("upvar borrow conflict")
                            .vars
                            .get_unchecked(re.map.get_unchecked(index).1)
                            .dup(),
                    );
                }
                Ok(())
            }
            // No `Iterable` fallback: a module's namespace is reserved for its
            // exports, so it does not claim the supertype (see `Type` below).
            // This also keeps the default `op_mcall` safe — handing back a
            // `BoundMethod` here would make it recurse, since the default is
            // `op_get` followed by `op_call`.
            Err(_) => Err(Error::field(strand, field)),
        }
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let re = this.get();
        match re.map.binary_search_by_key(&field, |(s, _)| *s) {
            Ok(index) => {
                unsafe {
                    *re.upvars
                        .borrow_mut()
                        .expect("upvar borrow conflict")
                        .vars
                        .get_unchecked_mut(index) = value.take();
                }
                Ok(())
            }
            Err(_) => Err(Error::field(strand, field)),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let sr = this.to_strong();
        strand.builtin_types().module_iter.create(
            strand,
            Iter {
                module: sr,
                index: 0,
            },
            out,
        );
        Ok(())
    }
}

pub(crate) struct Iter<'v> {
    module: GcObj<'v, Module<'v>>,
    index: usize,
}

unsafe impl<'v> Collect for Iter<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.module.accept(visit)
    }

    fn clear(&mut self) {
        // Can't actually clear module
    }
}

impl<'v> Protocol<'v> for Iter<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().input_iter)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<module iterator>")
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut this_borrow = this.borrow_mut(strand)?;
        let mod_borrow = this_borrow
            .module
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?;
        if this_borrow.index == mod_borrow.map.len() {
            return Ok(false);
        }
        let entry = &mod_borrow.map[this_borrow.index];
        let key = Value::from_object(strand.sym_obj(entry.0));
        let value = mod_borrow
            .upvars
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?
            .vars[entry.1]
            .dup();
        out.store(Value::from_object(tuple::tuple(strand, [key, value])));
        mem::drop(mod_borrow);
        this_borrow.index += 1;
        Ok(true)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        iter::iter_mcall(strand, &this, method, args, out).await
    }
}

pub(crate) struct Native<'v> {
    name: &'v str,
    map: alias::Box<[(Sym<'v, 'v>, NativeField<'v>)]>,
}

unsafe impl<'v> Collect for Native<'v> {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        unreachable!()
    }
}

impl<'v> Native<'v> {
    pub(crate) fn new(
        name: &'v str,
        items: impl IntoIterator<Item = (Sym<'v, 'v>, NativeField<'v>)>,
    ) -> Self {
        let mut this = Self {
            name,
            map: items.into_iter().collect(),
        };

        this.map.sort_by_key(|(s, _)| *s);
        this
    }
}

impl<'v> Protocol<'v> for Native<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().module)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "{}", this.get().name)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<")?;
        Self::op_display(this, strand, w)?;
        crate::fmt!(strand, w, ">")
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let name = this.get().name;
        let map = &this.borrow(strand)?.map;
        match map.binary_search_by_key(&field, |(s, _)| *s) {
            Ok(index) => match &unsafe { map.get_unchecked(index) }.1 {
                NativeField::Value(value) => {
                    Output::set(strand, out, value);
                    Ok(())
                }
                NativeField::Getter(getter) => Strand::for_native_frame(
                    strand,
                    Cow::Borrowed(name),
                    Cow::Borrowed(name),
                    Some(Cow::Borrowed("(get)")),
                    |strand| getter(strand, out),
                ),
            },
            Err(_) => Err(Error::field(strand, field)),
        }
    }

    fn op_set<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _field: Sym<'v, 'a>,
        _value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::immutable(strand))
    }
}

enum NamespaceInner<'v> {
    Normal(Module<'v>),
    Custom(Value<'v>),
    Empty,
}

pub(crate) struct Namespace<'v> {
    inner: NamespaceInner<'v>,
    dict: GcObj<'v, Dict<'v>>,
}

unsafe impl<'v> Collect for Namespace<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        match &self.inner {
            NamespaceInner::Normal(module) => module.accept(visit)?,
            NamespaceInner::Custom(value) => value.accept(visit)?,
            NamespaceInner::Empty => (),
        }
        self.dict.accept(visit)
    }

    fn clear(&mut self) {
        self.inner = NamespaceInner::Empty;
    }
}

impl<'v> Namespace<'v> {
    pub(crate) fn new(vm: &Vm<'v>) -> Self {
        Self {
            inner: NamespaceInner::Empty,
            dict: GcObj::new(vm.arena(), vm.builtin_types().dict, Dict::new()),
        }
    }

    pub(crate) fn insert<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        components: &[&str],
        mut slot: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match components.split_first() {
            Some((&first, rest)) => {
                let sym = Value::from_object(strand.sym_register_obj(first));
                if let Some(value) = self
                    .dict
                    .borrow()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .get(strand, &sym, Some(0))?
                {
                    let ns = value
                        .downcast_ref(strand.builtin_types().namespace)
                        .ok_or_else(|| {
                            Error::type_error(strand, "can't import into non-namespace")
                        })?;
                    return ns
                        .borrow_mut()
                        .ok_or_else(|| Error::concurrency(strand))?
                        .insert(strand, rest, slot);
                }
                let mut ns = Self::new(strand);
                ns.insert(strand, rest, slot)?;
                let mut hasher = DefaultHasher::new();
                sym.op_hash(strand, &mut hasher)?;
                let hv = hasher.finish();
                self.dict
                    .borrow_mut()
                    .ok_or_else(|| Error::concurrency(strand))?
                    .insert(
                        strand,
                        sym,
                        Value::from_object(GcObj::new(
                            strand.arena(),
                            strand.builtin_types().namespace,
                            ns,
                        )),
                        hv,
                        true,
                    );
                Ok(())
            }
            None => {
                if let Some(module) = slot.downcast_ref(strand.builtin_types().module) {
                    let module = module.get();
                    self.inner = NamespaceInner::Normal(Module {
                        loaded: module.loaded.clone(),
                        upvars: module.upvars.clone(),
                        map: module.map.clone(),
                    });
                } else {
                    self.inner = NamespaceInner::Custom(slot.take());
                }
                Ok(())
            }
        }
    }
}

impl<'v> Protocol<'v> for Namespace<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().module)
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        match &borrow.inner {
            NamespaceInner::Normal(module) => {
                let name = module
                    .loaded
                    .annex()
                    .module_name
                    .as_ref()
                    .map(|r| &module.loaded.annex().debug_strtab()[r.clone()])
                    .unwrap_or("?");
                crate::fmt!(strand, w, "{name}")
            }
            NamespaceInner::Custom(value) => value.op_display(strand, w),
            NamespaceInner::Empty => crate::fmt!(strand, w, "namespace"),
        }
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<")?;
        Self::op_display(this, strand, w)?;
        crate::fmt!(strand, w, ">")
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let re = this.borrow(strand)?;
        match &re.inner {
            NamespaceInner::Normal(module) => {
                match module.map.binary_search_by_key(&field, |(s, _)| *s) {
                    Ok(index) => {
                        unsafe {
                            out.store(
                                module
                                    .upvars
                                    .borrow()
                                    .expect("upvar borrow conflict")
                                    .vars
                                    .get_unchecked(module.map.get_unchecked(index).1)
                                    .dup(),
                            );
                        }
                        return Ok(());
                    }
                    Err(_) => return Err(Error::field(strand, field)),
                }
            }
            NamespaceInner::Custom(value) => {
                match value.op_get(strand, field, Slot::reborrow(&mut out)) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == ErrorKind::Field => (),
                    Err(e) => return Err(e),
                }
            }
            NamespaceInner::Empty => (),
        };
        let key = Value::from_object(strand.sym_obj(field));
        match re
            .dict
            .borrow()
            .ok_or_else(|| Error::concurrency(strand))?
            .get(strand, &key, Some(0))?
        {
            Some(value) => {
                out.store(value.dup());
                Ok(())
            }
            None => Err(Error::field(strand, field)),
        }
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let re = this.borrow(strand)?;
        match &re.inner {
            NamespaceInner::Normal(module) => {
                match module.map.binary_search_by_key(&field, |(s, _)| *s) {
                    Ok(index) => {
                        module
                            .upvars
                            .borrow_mut()
                            .expect("upvar borrow conflict")
                            .vars[index] = value.take();
                        return Ok(());
                    }
                    Err(_) => return Err(Error::field(strand, field)),
                }
            }
            NamespaceInner::Custom(module) => return module.op_set(strand, field, value),
            NamespaceInner::Empty => (),
        }
        let key = Value::from_object(strand.sym_obj(field));
        let mut hasher = DefaultHasher::new();
        key.op_hash(strand, &mut hasher).unwrap();
        let hv = hasher.finish();
        re.dict
            .borrow_mut()
            .ok_or_else(|| Error::concurrency(strand))?
            .insert(strand, key, value.take(), hv, true);
        Ok(())
    }
}

// ── Module Class ────────────────────────────────────────────────

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    // Deliberately not `Iterable`. Modules follow the iteration protocol —
    // `op_iter` yields `(name, value)` pairs, which the REPL's dynamic prelude
    // relies on to carry bindings across executions — but a module's member
    // namespace is entirely reserved for its exports, so it cannot expose
    // `Iterable`'s method surface. Same reasoning as `Record`.
    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this) || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Module>")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dolang_compile::{Config, Mode};

    use crate::{
        error::ErrorKind,
        method, sym,
        test_support::{with_builder, with_vm},
        vm::Bytecode,
    };

    use super::*;

    /// Compiles `source` in module mode and runs it, yielding a real, VM-constructed
    /// `Module` value into `out` — this is the only way to exercise `Module`'s `Protocol`
    /// impl, since its fields require an actual `Gc<Program>`/`Upvars` pair that can't be
    /// fabricated by hand.
    async fn compile_module<'v, 's>(
        strand: &mut Strand<'v, 's>,
        name: &str,
        source: &str,
        out: impl Output<'v>,
    ) {
        let mut config = Config::new();
        config.mode(Mode::Module { name });
        let mut bytes = Vec::new();
        config
            .unit(Path::new("<test>"), source.as_bytes())
            .emit(&mut bytes)
            .unwrap();
        Bytecode::new(bytes).run(strand, out).await.unwrap();
    }

    fn make_native<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
        let items = vec![
            (
                Sym::well_known(sym::LEN),
                NativeField::Value(Value::from_i64(strand, 42)),
            ),
            (
                Sym::well_known(sym::COUNT),
                NativeField::Getter(Box::new(
                    |strand: &mut Strand<'_, '_>, out: Slot<'_, '_>| {
                        Output::set(strand, out, 7_i64);
                        Ok(())
                    },
                )),
            ),
        ];
        strand
            .builtin_types()
            .native_module
            .create(strand, Native::new("test_native", items), out);
    }

    fn make_namespace<'v>(strand: &mut Strand<'v, '_>, out: impl Output<'v>) {
        let ns = Namespace::new(strand);
        strand.builtin_types().namespace.create(strand, ns, out);
    }

    #[test]
    fn native_op_get_dispatches_value_and_getter_fields_errors_on_unknown() {
        with_vm(async |strand, [mut slot, mut out]| {
            make_native(strand, &mut slot);

            strand
                .builtin_types()
                .native_module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Native::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                });
            assert_eq!(out.to_i64(strand).unwrap(), 42);

            strand
                .builtin_types()
                .native_module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Native::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::COUNT),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                });
            assert_eq!(out.to_i64(strand).unwrap(), 7);

            strand
                .builtin_types()
                .native_module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let err = Native::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::KEYS),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);
                });
        });
    }

    #[test]
    fn native_op_set_always_errors_immutable() {
        with_vm(async |strand, [mut slot, mut val]| {
            make_native(strand, &mut slot);
            Output::set(strand, &mut val, 1_i64);
            strand
                .builtin_types()
                .native_module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let err = Native::op_set(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut val),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Immutable);
                });
        });
    }

    #[test]
    fn native_op_display_and_op_debug_show_the_module_name() {
        with_vm(async |strand, [mut slot]| {
            make_native(strand, &mut slot);
            strand
                .builtin_types()
                .native_module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut display = String::new();
                    Native::op_display(recv.clone(), strand, &mut display).unwrap();
                    assert_eq!(display, "test_native");

                    let mut debug = String::new();
                    Native::op_debug(recv, strand, &mut debug).unwrap();
                    assert_eq!(debug, "<test_native>");
                });
        });
    }

    #[test]
    fn namespace_op_get_set_fall_back_to_dict_when_empty() {
        with_vm(async |strand, [mut slot, mut val, mut out]| {
            make_namespace(strand, &mut slot);
            Output::set(strand, &mut val, 99_i64);

            strand
                .builtin_types()
                .namespace
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Namespace::op_set(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut val),
                    )
                    .unwrap();
                });

            strand
                .builtin_types()
                .namespace
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Namespace::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap();
                });
            assert_eq!(out.to_i64(strand).unwrap(), 99);
        });
    }

    #[test]
    fn namespace_op_get_unknown_field_on_empty_namespace_errors() {
        with_vm(async |strand, [mut slot, mut out]| {
            make_namespace(strand, &mut slot);
            strand
                .builtin_types()
                .namespace
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let err = Namespace::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);
                });
        });
    }

    #[test]
    fn namespace_insert_creates_a_nested_custom_namespace() {
        // Register "a" via `Builder::sym` before entering: it stays permanently rooted for
        // the life of the VM, unlike a symbol interned later on a `Strand`, which the
        // symbol table only holds weakly.
        with_builder(async |vm| {
            let sym_a = vm.sym("a");
            vm.enter_with_slots(async move |strand, [mut leaf, mut root, mut out]| {
                Output::set(strand, &mut leaf, 7_i64);

                let mut ns = Namespace::new(strand);
                ns.insert(strand, &["a"], leaf).unwrap();
                strand
                    .builtin_types()
                    .namespace
                    .create(strand, ns, &mut root);

                strand
                    .builtin_types()
                    .namespace
                    .cast(&root)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        Namespace::op_get(recv, strand, sym_a, Slot::reborrow(&mut out)).unwrap();
                    });

                // The child stored at "a" is itself a `Namespace`, whose `inner` wraps the
                // inserted leaf value directly (`insert` with an already-empty remaining
                // path sets `NamespaceInner::Custom`, rather than storing the leaf under a
                // dict key).
                let child = strand.builtin_types().namespace.cast(&out).unwrap();
                child.enter_sync(strand, |strand, recv| {
                    let borrow = recv.borrow(strand).unwrap();
                    match &borrow.inner {
                        NamespaceInner::Custom(v) => assert_eq!(v.to_i64(strand).unwrap(), 7),
                        _ => panic!("expected a Custom namespace leaf"),
                    }
                });
            })
            .await
        });
    }

    #[test]
    fn module_op_type_op_display_and_op_debug() {
        with_vm(async |strand, [mut slot, mut out]| {
            compile_module(strand, "test_mod", "pub let x = 1", &mut slot).await;

            strand
                .builtin_types()
                .module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Module::op_type(recv, strand, Slot::reborrow(&mut out));
                });
            assert!(out.eq(strand, &strand.singletons().module));

            strand
                .builtin_types()
                .module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut display = String::new();
                    Module::op_display(recv.clone(), strand, &mut display).unwrap();
                    assert_eq!(display, "test_mod");

                    let mut debug = String::new();
                    Module::op_debug(recv, strand, &mut debug).unwrap();
                    assert_eq!(debug, "<test_mod>");
                });
        });
    }

    #[test]
    fn module_op_get_and_op_set_known_and_unknown_field() {
        // Register both symbols via `Builder::sym` before entering: it roots them
        // permanently for the life of the VM, so a `Sym` derived from either stays valid
        // even once it flows into `Error::field`'s `Sym::as_str` call — unlike a symbol
        // interned later on a `Strand`, which the symbol table only holds weakly.
        with_builder(async |vm| {
            let sym_x = vm.sym("x");
            let sym_unknown = vm.sym("nope");
            vm.enter_with_slots(async move |strand, [mut slot, mut out]| {
                compile_module(strand, "test_mod", "pub let x = 1", &mut slot).await;

                strand
                    .builtin_types()
                    .module
                    .cast(&slot)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        Module::op_get(recv, strand, sym_x, Slot::reborrow(&mut out)).unwrap();
                    });
                assert_eq!(out.to_i64(strand).unwrap(), 1);

                Output::set(strand, &mut out, 2_i64);
                strand
                    .builtin_types()
                    .module
                    .cast(&slot)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        Module::op_set(recv, strand, sym_x, Slot::reborrow(&mut out)).unwrap();
                    });

                strand
                    .builtin_types()
                    .module
                    .cast(&slot)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        Module::op_get(recv, strand, sym_x, Slot::reborrow(&mut out)).unwrap();
                    });
                assert_eq!(out.to_i64(strand).unwrap(), 2);

                strand
                    .builtin_types()
                    .module
                    .cast(&slot)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        let err =
                            Module::op_get(recv, strand, sym_unknown, Slot::reborrow(&mut out))
                                .unwrap_err();
                        assert_eq!(err.kind(), ErrorKind::Field);
                    });
                strand
                    .builtin_types()
                    .module
                    .cast(&slot)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        let err =
                            Module::op_set(recv, strand, sym_unknown, Slot::reborrow(&mut out))
                                .unwrap_err();
                        assert_eq!(err.kind(), ErrorKind::Field);
                    });
            })
            .await
        });
    }

    #[test]
    fn module_op_iter_yields_key_value_pairs() {
        with_vm(async |strand, [mut slot, mut out, mut next_out]| {
            compile_module(strand, "test_mod", "pub let x = 1", &mut slot).await;

            strand
                .builtin_types()
                .module
                .cast(&slot)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    Module::op_iter(recv, strand, Slot::reborrow(&mut out))
                        .await
                        .unwrap();
                })
                .await;

            let more = strand
                .builtin_types()
                .module_iter
                .cast(&out)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    Iter::op_next(recv, strand, Slot::reborrow(&mut next_out))
                        .await
                        .unwrap()
                })
                .await;
            assert!(more);
        });
    }

    #[test]
    fn module_iter_op_get_and_op_mcall_delegate_to_iter_glue() {
        with_vm(async |strand, [mut slot, mut iter_slot, mut out]| {
            compile_module(strand, "test_mod", "pub let x = 1", &mut slot).await;

            strand
                .builtin_types()
                .module
                .cast(&slot)
                .unwrap()
                .enter(strand, async |strand, recv| {
                    Module::op_iter(recv, strand, Slot::reborrow(&mut iter_slot))
                        .await
                        .unwrap();
                })
                .await;

            strand
                .builtin_types()
                .module_iter
                .cast(&iter_slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let err = Iter::op_get(
                        recv,
                        strand,
                        Sym::well_known(sym::LEN),
                        Slot::reborrow(&mut out),
                    )
                    .unwrap_err();
                    assert_eq!(err.kind(), ErrorKind::Field);
                });

            let err = method!(strand, &iter_slot, Sym::well_known(sym::LEN), &mut out)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Field);
        });
    }

    #[test]
    fn native_op_type_is_module_singleton() {
        with_vm(async |strand, [mut slot, mut out]| {
            make_native(strand, &mut slot);
            strand
                .builtin_types()
                .native_module
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Native::op_type(recv, strand, Slot::reborrow(&mut out));
                });
            assert!(out.eq(strand, &strand.singletons().module));
        });
    }

    #[test]
    fn namespace_op_type_is_module_singleton() {
        with_vm(async |strand, [mut slot, mut out]| {
            make_namespace(strand, &mut slot);
            strand
                .builtin_types()
                .namespace
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    Namespace::op_type(recv, strand, Slot::reborrow(&mut out));
                });
            assert!(out.eq(strand, &strand.singletons().module));
        });
    }

    #[test]
    fn namespace_op_display_and_op_debug_for_empty_and_custom() {
        with_vm(async |strand, [mut slot]| {
            make_namespace(strand, &mut slot);
            strand
                .builtin_types()
                .namespace
                .cast(&slot)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut display = String::new();
                    Namespace::op_display(recv.clone(), strand, &mut display).unwrap();
                    assert_eq!(display, "namespace");

                    let mut debug = String::new();
                    Namespace::op_debug(recv, strand, &mut debug).unwrap();
                    assert_eq!(debug, "<namespace>");
                });
        });
    }

    #[test]
    fn namespace_op_display_for_custom_delegates_to_wrapped_value() {
        with_vm(async |strand, [mut leaf, mut root]| {
            Output::set(strand, &mut leaf, 7_i64);
            let mut ns = Namespace::new(strand);
            ns.insert(strand, &[], leaf).unwrap();
            strand
                .builtin_types()
                .namespace
                .create(strand, ns, &mut root);

            strand
                .builtin_types()
                .namespace
                .cast(&root)
                .unwrap()
                .enter_sync(strand, |strand, recv| {
                    let mut display = String::new();
                    Namespace::op_display(recv, strand, &mut display).unwrap();
                    assert_eq!(display, "7");
                });
        });
    }

    #[test]
    fn namespace_op_get_custom_propagates_non_field_error() {
        // Register "nope" via `Builder::sym` before entering, so it stays permanently
        // rooted instead of relying on the symbol table's weak reference.
        with_builder(async |vm| {
            let sym_unknown = vm.sym("nope");
            vm.enter_with_slots(async move |strand, [mut root, mut leaf, mut out]| {
                // Build a namespace whose `Custom` slot wraps a boxed `Sym` object:
                // `SymObj`'s `Protocol` impl doesn't override `op_get`, so unknown fields
                // hit the trait's default, which returns a `Type` error rather than
                // `Field` — this exercises the branch in `Namespace::op_get` that
                // propagates a non-`Field` error instead of falling back to the dict.
                let sym_obj = strand.sym_register_obj("leaf");
                Output::set(strand, &mut leaf, &Value::from_object(sym_obj));
                let mut ns = Namespace::new(strand);
                ns.insert(strand, &[], leaf).unwrap();
                strand
                    .builtin_types()
                    .namespace
                    .create(strand, ns, &mut root);

                strand
                    .builtin_types()
                    .namespace
                    .cast(&root)
                    .unwrap()
                    .enter_sync(strand, |strand, recv| {
                        let err =
                            Namespace::op_get(recv, strand, sym_unknown, Slot::reborrow(&mut out))
                                .unwrap_err();
                        assert_eq!(err.kind(), ErrorKind::Type);
                    });
            })
            .await
        });
    }

    #[test]
    fn namespace_insert_into_non_namespace_errors() {
        with_vm(async |strand, [mut leaf, mut extra]| {
            // `insert`'s recursive path always wraps intermediate components in a
            // `Namespace` object, so the only way to get a plain, non-namespace value
            // under a dict key is to write directly into the backing dict, the same way
            // `Namespace::op_set`'s fallback does. Put a bare int under "a" that way, then
            // try to `insert` through "a/b" — since "a" now holds a non-namespace value,
            // that must fail rather than silently overwrite it.
            let mut ns = Namespace::new(strand);
            Output::set(strand, &mut leaf, 1_i64);
            let sym_a = Value::from_object(strand.sym_register_obj("a"));
            let mut hasher = DefaultHasher::new();
            sym_a.op_hash(strand, &mut hasher).unwrap();
            let hv = hasher.finish();
            ns.dict
                .borrow_mut()
                .unwrap()
                .insert(strand, sym_a, leaf.take(), hv, true);

            Output::set(strand, &mut extra, 2_i64);
            let err = ns.insert(strand, &["a", "b"], extra).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn module_type_op_type() {
        with_vm(async |strand, [mut out]| {
            let module_recv = strand
                .builtin_types()
                .module_type
                .cast(&strand.singletons().module)
                .unwrap();
            module_recv.enter_sync(strand, |strand, recv| {
                Type::op_type(recv, strand, Slot::reborrow(&mut out));
            });
            assert!(out.eq(strand, &strand.singletons().type_obj));
        });
    }

    #[test]
    fn module_type_op_subtype_and_op_debug() {
        with_vm(async |strand, []| {
            let module_recv = strand
                .builtin_types()
                .module_type
                .cast(&strand.singletons().module)
                .unwrap();
            module_recv.enter_sync(strand, |strand, recv| {
                assert!(Type::op_subtype(recv, strand, &strand.singletons().module));
            });

            let module_recv = strand
                .builtin_types()
                .module_type
                .cast(&strand.singletons().module)
                .unwrap();
            module_recv.enter_sync(strand, |strand, recv| {
                assert!(!Type::op_subtype(
                    recv,
                    strand,
                    &strand.singletons().iterable
                ));
            });

            let module_recv = strand
                .builtin_types()
                .module_type
                .cast(&strand.singletons().module)
                .unwrap();
            module_recv.enter_sync(strand, |strand, recv| {
                let mut s = String::new();
                Type::op_debug(recv, strand, &mut s).unwrap();
                assert_eq!(s, "<type std.Module>");
            });
        });
    }
}
