use std::{borrow::Cow, fmt, mem, ops::ControlFlow};

use dolang_util::alias;

use crate::{
    Program,
    arg::Args,
    error::{Error, ErrorKind, Result, ResultExt},
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

use super::{dict::Dict, kv};

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

    pub(crate) fn entries(&self) -> Vec<(Sym<'v, 'static>, Value<'v>)> {
        let upvars = self.upvars.borrow().expect("upvar borrow conflict");
        self.map
            .iter()
            .map(|(sym, idx)| (*sym, upvars.vars[*idx].dup()))
            .collect()
    }
}

impl<'v> Protocol<'v> for Module<'v> {
    fn op_subtype<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, &strand.singletons().module)
            || supertype.eq(strand, TypeObject::Value)
    }

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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let borrow = this.get();
        let name = borrow
            .loaded
            .module_name
            .as_ref()
            .map(|r| &borrow.loaded.debug_strtab()[r.clone()])
            .unwrap_or("?");
        write!(w, "{name}").into_do(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<").into_do(strand)?;
        Self::op_display(this, strand, w)?;
        write!(w, ">").into_do(strand)
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
            Err(_) => iter::iterable_get(strand, &this, field, out),
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<module iterator>").into_do(strand)
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.get().name).into_do(strand)
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<").into_do(strand)?;
        Self::op_display(this, strand, w)?;
        write!(w, ">").into_do(strand)
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
                let hv = kv::hash(strand, &sym)?;
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
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let borrow = this.borrow(strand)?;
        match &borrow.inner {
            NamespaceInner::Normal(module) => {
                let name = module
                    .loaded
                    .module_name
                    .as_ref()
                    .map(|r| &module.loaded.debug_strtab()[r.clone()])
                    .unwrap_or("?");
                write!(w, "{name}").into_do(strand)
            }
            NamespaceInner::Custom(value) => value.op_display(strand, w),
            NamespaceInner::Empty => write!(w, "namespace").into_do(strand),
        }
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<").into_do(strand)?;
        Self::op_display(this, strand, w)?;
        write!(w, ">").into_do(strand)
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
        let hv = kv::hash(strand, &key).unwrap();
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

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || supertype.eq(strand, &strand.singletons().iterable)
            || supertype.eq(strand, TypeObject::Value)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        use crate::error::ResultExt;
        write!(w, "<type std.module>").into_do(strand)
    }
}
