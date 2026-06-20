use std::{cell::OnceCell, fmt, ops::ControlFlow};

use dolang_util::alias;

use crate::{
    Program,
    arg::Args,
    call,
    error::{Error, ErrorKind, Result, ResultExt},
    gc::{self, Annex, Collect, arena::Visit},
    method,
    object::{
        BoundMethod,
        protocol::{Inspect, Recv, Spread, SpreadContext, default_spread},
    },
    sig::Unpack,
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, Slots, Value},
    vm::Vm,
};

use super::protocol::{GcObj, GcObjBorrow, Protocol};

pub(crate) struct Descriptor;

unsafe impl Collect for Descriptor {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Descriptor {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<type Descriptor>").into_do(strand)
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            members: vec![Sym::well_known(sym::GET), Sym::well_known(sym::SET)],
        })
    }
}

/// A single entry in a class's unified symbol table.
pub(crate) enum ClassEntry<'v> {
    /// Index into `field_defaults` / instance `fields`.
    Field(usize),
    /// A Do function value (method).
    Method(Value<'v>),
    /// A descriptor object whose `get`/`set` methods mediate instance access.
    Descriptor(Value<'v>),
    /// Native delegation: index into instance `natives`.
    Delegate(usize),
    /// Abstract delegation: the type-object singleton to dispatch to.
    /// Dispatched via `op_dcall` on the type-object with the instance as delegator.
    Abstract(Value<'v>),
}

pub(crate) struct ClassObject<'v> {
    // Roots below symbols
    pub(crate) program: gc::Gc<'v, Program<'v>>,
    // Class name (for debug formatting)
    pub(crate) name: alias::Box<str>,
    // Direct superclasses (for subtype checking); may be ClassObject or built-in type objects
    pub(crate) supers: alias::Box<[Value<'v>]>,
    // Unified MRO-ordered lookup table sorted by Sym.
    pub(crate) entries: alias::Box<[(Sym<'v, 'static>, ClassEntry<'v>)]>,
    // Default values for field slots (indexed by ClassEntry::Field(n))
    pub(crate) field_defaults: alias::Box<[Value<'v>]>,
    // Non-abstract native supers, in transitive collection order (left-to-right).
    // Index in this slice == slot index in ClassInstance::native_slots.
    pub(crate) native_supers: alias::Box<[Value<'v>]>,
}

impl<'v> ClassObject<'v> {
    /// Look up an entry by symbol.
    pub(crate) fn entry(&self, sym: Sym<'v, '_>) -> Option<&ClassEntry<'v>> {
        self.entries
            .binary_search_by_key(&sym, |(s, _)| *s)
            .ok()
            .map(|idx| &self.entries[idx].1)
    }

    /// Look up a method by symbol.
    pub(crate) fn method(&self, sym: Sym<'v, '_>) -> Option<&Value<'v>> {
        self.entry(sym).and_then(|entry| match entry {
            ClassEntry::Method(v) => Some(v),
            _ => None,
        })
    }

    /// Look up the (init) method.
    pub(crate) fn init(&self) -> Option<&Value<'v>> {
        self.method(Sym::well_known(sym::INIT_METHOD))
    }

    /// Look up an entry by well-known symbol tag.
    pub(crate) fn entry_by_tag(&self, tag: sym::Tag) -> Option<&ClassEntry<'v>> {
        self.entry(Sym::well_known(tag))
    }
}

unsafe impl<'v> Collect for ClassObject<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.program.accept(visit)?;
        for sup in self.supers.iter() {
            sup.accept(visit)?;
        }
        for (_, entry) in self.entries.iter() {
            match entry {
                ClassEntry::Method(v) | ClassEntry::Descriptor(v) | ClassEntry::Abstract(v) => {
                    v.accept(visit)?
                }
                _ => {}
            }
        }
        for v in self.field_defaults.iter() {
            v.accept(visit)?;
        }
        for v in self.native_supers.iter() {
            v.accept(visit)?;
        }
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {
        for v in self.supers.iter_mut() {
            *v = Value::NIL;
        }
        for (_, entry) in self.entries.iter_mut() {
            match entry {
                ClassEntry::Method(v) | ClassEntry::Descriptor(v) | ClassEntry::Abstract(v) => {
                    *v = Value::NIL
                }
                _ => {}
            }
        }
        for v in self.field_defaults.iter_mut() {
            *v = Value::NIL;
        }
        for v in self.native_supers.iter_mut() {
            *v = Value::NIL;
        }
    }
}

impl<'v> Protocol<'v> for ClassObject<'v> {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(strand.singletons().type_obj.dup())
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let this = this.get();
        if let Some(module_name) = this
            .program
            .module_name
            .as_ref()
            .map(|r| &this.program.debug_strtab()[r.clone()])
        {
            write!(w, "<type {module_name}.{}>", this.name).into_do(strand)
        } else {
            write!(w, "<type {}>", this.name).into_do(strand)
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.get();

        // Only methods are accessible on the class type object itself
        if let Some(v) = me.method(field) {
            out.store(v.dup());
            return Ok(());
        }
        Err(Error::field(strand, field))
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &Value<'v>,
    ) -> bool {
        supertype.eq(strand, &this)
            || this
                .get()
                .supers
                .iter()
                .any(|sup| sup.op_subtype(strand, supertype))
            || supertype.eq(strand, &strand.vm().singletons().value)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.get();

        // Only methods are callable on the class type object itself
        if let Some(v) = me.method(method) {
            return v.dup().op_call(strand, args, out).await;
        }
        Err(Error::field(strand, method))
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let me = this.get();
        let defaults = me.field_defaults.iter().map(|v| v.dup()).collect();
        let native_slot_count = me.native_supers.len();
        let class_obj = this.to_strong();

        strand
            .with_slots(async move |strand, [mut inst, tmp]| {
                inst.store(Value::from_object(GcObj::new_annex(
                    strand.arena(),
                    strand.builtin_types().class_instance,
                    ClassInstance { fields: defaults },
                    ClassInstanceAnnex {
                        class: class_obj,
                        natives: (0..native_slot_count)
                            .map(|_| OnceCell::new())
                            .collect::<Vec<_>>()
                            .into(),
                    },
                )));

                if let Some(func) = me.init() {
                    args.prepend_self(inst.dup());
                    func.op_call(strand, args, tmp).await?;
                } else {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                }

                // Verify all native slots are initialized
                if inst
                    .downcast_ref(strand.builtin_types().class_instance)
                    .unwrap()
                    .annex()
                    .natives
                    .iter()
                    .any(|slot| slot.get().is_none())
                {
                    return Err(Error::runtime(strand, "native supertypes not initialized"));
                }

                Output::set(strand, out, inst);
                Ok(())
            })
            .await
    }
}

pub(crate) struct ClassInstanceAnnex<'v> {
    pub(crate) class: GcObj<'v, ClassObject<'v>>,
    pub(crate) natives: alias::Box<[OnceCell<Value<'v>>]>,
}

impl<'v> Annex for ClassInstanceAnnex<'v> {
    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.class.accept(visit)?;
        for slot in self.natives.iter() {
            if let Some(v) = slot.get() {
                v.accept(visit)?;
            }
        }
        ControlFlow::Continue(())
    }

    fn clear(&self) {
        // GC cannot safely clear annexes with outstanding immutable references
    }
}

pub(crate) struct ClassInstance<'v> {
    fields: alias::Box<[Value<'v>]>,
}

unsafe impl<'v> Collect for ClassInstance<'v> {
    const CYCLIC: bool = true;
    const IMMUTABLE: bool = false;
    type Annex = ClassInstanceAnnex<'v>;

    fn accept(&self, visit: &mut dyn Visit) -> ControlFlow<()> {
        self.fields.accept(visit)
    }

    fn clear(&mut self) {
        self.fields.fill_with(|| Value::NIL);
    }
}

/// Given a [`ClassInstance`] GC borrow (as returned by [`Value::downcast_ref`]) and a
/// native super's type object, returns a reference to the initialized native slot value,
/// if any.  The returned reference's lifetime is linked to that of the borrow.
pub(crate) fn get_native_slot<'v, 'a>(
    vm: &Vm<'v>,
    borrow: GcObjBorrow<'v, 'a, ClassInstance<'v>>,
    type_obj: &Value<'v>,
) -> Option<&'a Value<'v>> {
    let annex = borrow.annex();
    let slot = annex
        .class
        .native_supers
        .iter()
        .position(|s| s.repr_eq(vm, type_obj))?;
    annex.natives[slot].get()
}

/// Returns an iterator over all initialized native values in a [`ClassInstance`].
/// The lifetime of each yielded `&Value<'v>` is linked to the borrow lifetime `'a`.
pub(crate) fn iter_natives<'v, 'a>(
    borrow: GcObjBorrow<'v, 'a, ClassInstance<'v>>,
) -> impl Iterator<Item = &'a Value<'v>> {
    borrow.annex().natives.iter().filter_map(|s| s.get())
}

fn class_sync_binary_op<'v, 'a, 's, F>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    method_sym: sym::Tag,
    other: &Value<'v>,
    native_fn: F,
) -> Result<'v, 's, Value<'v>>
where
    F: for<'ss, 'aa> FnOnce(
        &'aa Value<'v>,
        &'aa mut Strand<'v, 'ss>,
        &'aa Value<'v>,
    ) -> Result<'v, 'ss, Value<'v>>,
{
    let annex = this.annex();
    match annex.class.entry_by_tag(method_sym) {
        Some(ClassEntry::Method(v)) => strand.with_slots_sync(move |strand, [mut result]| {
            strand.sync(async |strand| call!(strand, v, &mut result, &this, other).await)?;
            Ok(result.take())
        }),
        Some(ClassEntry::Delegate(slot)) => {
            let native = annex.natives[*slot]
                .get()
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
            native_fn(native, strand, other)
        }
        _ => Err(Error::not_supported(strand)),
    }
}

fn class_sync_unary_op<'v, 'a, 's, F>(
    this: Recv<'v, 'a, ClassInstance<'v>>,
    strand: &mut Strand<'v, 's>,
    method_sym: sym::Tag,
    native_fn: F,
) -> Result<'v, 's, Value<'v>>
where
    F: FnOnce(&Value<'v>, &mut Strand<'v, 's>) -> Result<'v, 's, Value<'v>>,
{
    let annex = this.annex();
    match annex.class.entry_by_tag(method_sym) {
        Some(ClassEntry::Method(v)) => strand.with_slots_sync(move |strand, [mut result]| {
            strand.sync(async |strand| call!(strand, v, &mut result, &this).await)?;
            Ok(result.take())
        }),
        Some(ClassEntry::Delegate(slot)) => {
            let native = annex.natives[*slot]
                .get()
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
            native_fn(native, strand)
        }
        _ => Err(Error::not_supported(strand)),
    }
}

impl<'v> Protocol<'v> for ClassInstance<'v> {
    fn op_fill<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        type_obj: &Value<'v>,
        native: Value<'v>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let idx = annex
            .class
            .native_supers
            .iter()
            .position(|sup| sup.repr_eq(strand, type_obj))
            .ok_or_else(|| {
                Error::type_error(strand, "not a concrete native super of this class")
            })?;

        // Check if slot is already set with the same value (idempotent for diamond inheritance)
        if let Some(existing) = annex.natives[idx].get() {
            if existing.repr_eq(strand, &native) {
                return Ok(());
            } else {
                return Err(Error::runtime(
                    strand,
                    "native slot already initialized with a different value",
                ));
            }
        }

        annex.natives[idx]
            .set(native)
            .map_err(|_| Error::state_error(strand, "native slot already initialized"))
    }

    fn op_type<'a, 's>(
        this: Recv<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) {
        out.store(Value::from_object(this.annex().class.clone()))
    }

    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let res = {
            let w = &mut *w;
            let annex = this.annex();
            match annex.class.entry_by_tag(sym::STR_METHOD) {
                Some(ClassEntry::Method(v)) => strand.with_slots_sync(|strand, [mut result]| {
                    strand.sync(async |strand| call!(strand, v, &mut result, &this).await)?;
                    let result = result
                        .as_str_raw(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected str result"))?;
                    write!(w, "{result}").into_do(strand)?;
                    Ok(false)
                })?,
                Some(ClassEntry::Delegate(slot)) => {
                    let native = annex.natives[*slot]
                        .get()
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    strand.sync(async |strand| native.op_display(strand, w))?;
                    false
                }
                _ => true,
            }
        };
        if res {
            Self::op_debug(this, strand, w)
        } else {
            Ok(())
        }
    }

    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::DBG_METHOD) {
            Some(ClassEntry::Method(v)) => strand.with_slots_sync(move |strand, [mut result]| {
                strand.sync(async |strand| call!(strand, v, &mut result, &this).await)?;
                let result = result
                    .as_str_raw(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected str result"))?;
                write!(w, "{result}").into_do(strand)
            }),
            Some(ClassEntry::Delegate(slot)) => {
                let native = annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                strand.sync(async |strand| native.op_debug(strand, w))
            }
            _ => {
                let program = &annex.class.program;
                if let Some(module) = program
                    .module_name
                    .as_ref()
                    .map(|r| &program.debug_strtab()[r.clone()])
                {
                    write!(w, "<{module}.{}>", annex.class.name).into_do(strand)
                } else {
                    write!(w, "<{}>", annex.class.name).into_do(strand)
                }
            }
        }
    }

    fn op_display_arg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        let res = {
            let w = &mut *w;
            let this = this.clone();
            let annex = this.annex();
            match annex.class.entry_by_tag(sym::ARG_METHOD) {
                Some(ClassEntry::Method(v)) => {
                    strand.with_slots_sync(move |strand, [mut result]| {
                        strand.sync(async |strand| call!(strand, v, &mut result, &this).await)?;
                        let result = result
                            .as_str_raw(strand)
                            .ok_or_else(|| Error::type_error(strand, "expected str result"))?;
                        write!(w, "{result}").into_do(strand)?;
                        Ok(false)
                    })?
                }
                Some(ClassEntry::Delegate(slot)) => {
                    let native = annex.natives[*slot]
                        .get()
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    strand.sync(async |strand| native.op_display_arg(strand, w))?;
                    false
                }
                _ => true,
            }
        };
        if res {
            Self::op_display(this, strand, w)
        } else {
            Ok(())
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry(field) {
            Some(ClassEntry::Field(slot_idx)) => {
                let borrow = this.borrow(strand)?;
                out.store(borrow.fields[*slot_idx].dup());
                Ok(())
            }
            Some(ClassEntry::Descriptor(descriptor)) => strand.sync(async |strand| {
                method!(strand, descriptor, Sym::well_known(sym::GET), out, &this).await
            }),
            Some(ClassEntry::Method(_) | ClassEntry::Abstract(_)) => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            Some(ClassEntry::Delegate(slot)) => {
                let native = annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                strand.sync(async |strand| native.op_get(strand, field, out))
            }
            _ => match annex.class.entry_by_tag(sym::GET_METHOD) {
                Some(ClassEntry::Method(v)) => {
                    strand.sync(async |strand| call!(strand, v, out, &this, field).await)
                }
                Some(ClassEntry::Delegate(slot)) => {
                    let native = annex.natives[*slot]
                        .get()
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    strand.sync(async |strand| native.op_get(strand, field, out))
                }
                _ => Err(Error::field(strand, field)),
            },
        }
    }

    fn op_set<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        mut value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry(field) {
            Some(ClassEntry::Field(slot_idx)) => {
                let mut borrow = this.borrow_mut(strand)?;
                borrow.fields[*slot_idx] = value.take();
                Ok(())
            }
            Some(ClassEntry::Descriptor(descriptor)) => {
                strand.with_slots_sync(move |strand, [mut tmp]| {
                    strand.sync(async |strand| {
                        method!(
                            strand,
                            descriptor,
                            Sym::well_known(sym::SET),
                            &mut tmp,
                            &this,
                            &value
                        )
                        .await
                    })
                })
            }
            Some(ClassEntry::Delegate(slot)) => annex.natives[*slot]
                .get()
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                .op_set(strand, field, value),
            _ => match annex.class.entry_by_tag(sym::SET_METHOD) {
                Some(ClassEntry::Method(v)) => strand.with_slots_sync(move |strand, [mut tmp]| {
                    strand
                        .sync(async |strand| call!(strand, v, &mut tmp, &this, field, &value).await)
                }),
                Some(ClassEntry::Delegate(slot)) => {
                    let native = annex.natives[*slot]
                        .get()
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                    strand.sync(async |strand| native.op_set(strand, field, value))
                }
                _ => Err(Error::field(strand, field)),
            },
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let class = &this.annex().class;
        match class.entry(method) {
            Some(ClassEntry::Method(v)) => {
                args.prepend_self(Value::from_object(this.to_strong()));
                return v.op_call(strand, args, out).await;
            }
            Some(ClassEntry::Field(slot_idx)) => {
                let func = {
                    let borrow = this.borrow(strand)?;
                    borrow.fields[*slot_idx].dup()
                };
                return func.op_call(strand, args, out).await;
            }
            Some(ClassEntry::Descriptor(descriptor)) => {
                return strand
                    .with_slots(async move |strand, [mut callable]| {
                        method!(
                            strand,
                            descriptor,
                            Sym::well_known(sym::GET),
                            &mut callable,
                            &this
                        )
                        .await?;
                        callable.op_call(strand, args, out).await
                    })
                    .await;
            }
            Some(ClassEntry::Delegate(slot)) => {
                let native = this.annex().natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?;
                let self_val = Value::from_object(this.to_strong());
                return native.op_dcall(strand, &self_val, method, args, out).await;
            }
            Some(ClassEntry::Abstract(type_obj)) => {
                let self_val = Value::from_object(this.to_strong());
                return type_obj
                    .op_dcall(strand, &self_val, method, args, out)
                    .await;
            }
            None => {}
        }

        if let Some(v) = class.method(Sym::well_known(sym::GET_METHOD)) {
            return strand
                .with_slots(async move |strand, [mut callable]| {
                    call!(strand, v, &mut callable, &this, method).await?;
                    callable.op_call(strand, args, out).await
                })
                .await;
        }

        Err(Error::field(strand, method))
    }

    async fn op_call<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::CALL_METHOD) {
            Some(ClassEntry::Method(v)) => {
                args.prepend_self(Value::from_object(this.to_strong()));
                v.op_call(strand, args, out).await
            }
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_call(strand, args, out)
                    .await
            }
            _ => Err(Error::not_supported(strand)),
        }
    }

    async fn op_unpack<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        sig: &'a Unpack<'v, 'a>,
        out: Slots<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::UNPACK_METHOD) {
            Some(ClassEntry::Method(v)) => {
                strand
                    .with_slots(async move |strand, [mut proxy]| {
                        call!(strand, v, &mut proxy, &this).await?;
                        proxy.op_unpack(strand, sig, out).await
                    })
                    .await
            }
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_unpack(strand, sig, out)
                    .await
            }
            _ => Err(Error::not_supported(strand)),
        }
    }

    async fn op_iter<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::ITER_METHOD) {
            Some(ClassEntry::Method(v)) => call!(strand, v, out, &this).await,
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_iter(strand, out)
                    .await
            }
            _ => Err(Error::not_supported(strand)),
        }
    }

    async fn op_sink<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::SINK_METHOD) {
            Some(ClassEntry::Method(v)) => call!(strand, v, out, &this).await,
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_sink(strand, out)
                    .await
            }
            _ => Err(Error::not_supported(strand)),
        }
    }

    async fn op_spread<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        context: SpreadContext,
        sink: &'a mut dyn Spread<'v, 's>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::SPREAD_METHOD) {
            Some(ClassEntry::Method(v)) => {
                let proxy = strand
                    .with_slots(async move |strand, [mut proxy]| {
                        call!(strand, v, &mut proxy, &this).await?;
                        Ok(proxy.take())
                    })
                    .await?;
                proxy.op_spread(strand, context, sink).await
            }
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_spread(strand, context, sink)
                    .await
            }
            _ => default_spread(strand, this.clone(), context, sink).await,
        }
    }

    async fn op_next<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::NEXT_METHOD) {
            Some(ClassEntry::Method(v)) => match call!(strand, v, out, &this).await {
                Ok(()) => Ok(true),
                Err(err) if err.kind() == ErrorKind::IterStop => Ok(false),
                Err(err) => Err(err),
            },
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_next(strand, out)
                    .await
            }
            _ => Err(Error::not_supported(strand)),
        }
    }

    async fn op_put<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        item: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::PUT_METHOD) {
            Some(ClassEntry::Method(v)) => {
                strand
                    .with_slots(async move |strand, [tmp]| {
                        call!(strand, v, tmp, &this, &item).await
                    })
                    .await
            }
            Some(ClassEntry::Delegate(slot)) => {
                annex.natives[*slot]
                    .get()
                    .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                    .op_put(strand, item)
                    .await
            }
            _ => Err(Error::not_supported(strand)),
        }
    }

    fn op_add<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::ADD_METHOD, other, Value::op_add)
    }

    fn op_sub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::SUB_METHOD, other, Value::op_sub)
    }

    fn op_rsub<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::RSUB_METHOD, other, Value::op_rsub)
    }

    fn op_mul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::MUL_METHOD, other, Value::op_mul)
    }

    fn op_div<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::DIV_METHOD, other, Value::op_div)
    }

    fn op_rdiv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::RDIV_METHOD, other, Value::op_rdiv)
    }

    fn op_ediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::EDIV_METHOD, other, Value::op_ediv)
    }

    fn op_rediv<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::REDIV_METHOD, other, Value::op_rediv)
    }

    fn op_mod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::MOD_METHOD, other, Value::op_mod)
    }

    fn op_rmod<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::RMOD_METHOD, other, Value::op_rmod)
    }

    fn op_band<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::BAND_METHOD, other, Value::op_band)
    }

    fn op_bor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::BOR_METHOD, other, Value::op_bor)
    }

    fn op_bxor<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::BXOR_METHOD, other, Value::op_bxor)
    }

    fn op_shl<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::SHL_METHOD, other, Value::op_shl)
    }

    fn op_shr<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::SHR_METHOD, other, Value::op_shr)
    }

    fn op_neg<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_unary_op(this, strand, sym::NEG_METHOD, Value::op_neg)
    }

    fn op_bnot<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_unary_op(this, strand, sym::BNOT_METHOD, Value::op_bnot)
    }

    fn op_eq<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::EQ_METHOD, other, |n, s, o| {
            Ok(n.op_eq(s, o))
        })
    }

    fn op_lt<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        class_sync_binary_op(this, strand, sym::LT_METHOD, other, Value::op_lt)
    }

    fn op_bool<'a, 's>(this: Recv<'v, 'a, Self>, strand: &mut Strand<'v, 's>) -> bool {
        let this = this.clone();
        let annex = this.annex();
        match annex.class.entry_by_tag(sym::BOOL_METHOD) {
            Some(ClassEntry::Method(v)) => strand
                .with_slots_sync(move |strand, [mut result]| {
                    strand.sync(async |strand| call!(strand, v, &mut result, &this).await)?;
                    Ok::<_, crate::error::Error<'v, 's>>(result.take().op_bool(strand))
                })
                .unwrap_or(true),
            Some(ClassEntry::Delegate(slot)) => annex.natives[*slot]
                .get()
                .map(|n| n.op_bool(strand))
                .unwrap_or(true),
            _ => true,
        }
    }

    fn op_index<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        index: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry(Sym::well_known(sym::INDEX_METHOD)) {
            Some(ClassEntry::Method(v)) => {
                strand.sync(async |strand| call!(strand, v, out, &this, index).await)
            }
            Some(ClassEntry::Delegate(slot)) => annex.natives[*slot]
                .get()
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                .op_index(strand, index, out),
            _ => Err(Error::type_error(strand, "indexing not supported")),
        }
    }

    fn op_assign<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        index: Slot<'v, 'a>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        match annex.class.entry(Sym::well_known(sym::ASSIGN_METHOD)) {
            Some(ClassEntry::Method(v)) => strand.with_slots_sync(move |strand, [mut tmp]| {
                strand.sync(async |strand| call!(strand, v, &mut tmp, &this, &index, &value).await)
            }),
            Some(ClassEntry::Delegate(slot)) => annex.natives[*slot]
                .get()
                .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                .op_assign(strand, index, value),
            _ => Err(Error::type_error(strand, "index assignment not supported")),
        }
    }

    fn op_hash<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        hasher: &mut std::collections::hash_map::DefaultHasher,
    ) -> Result<'v, 's, ()> {
        use std::hash::Hash;
        let did_hash = {
            let hasher = &mut *hasher;
            let this = this.clone();
            let annex = this.annex();
            match annex.class.entry(Sym::well_known(sym::HASH_METHOD)) {
                Some(ClassEntry::Method(v)) => {
                    strand.with_slots_sync(move |strand, [mut result]| {
                        strand.sync(async |strand| call!(strand, v, &mut result, &this).await)?;
                        let v = result.as_i64(strand).ok_or_else(|| {
                            Error::type_error(strand, "expected int result from (hash)")
                        })?;
                        v.hash(hasher);
                        Ok(true)
                    })?
                }
                Some(ClassEntry::Delegate(slot)) => {
                    annex.natives[*slot]
                        .get()
                        .ok_or_else(|| Error::runtime(strand, "native slot uninitialized"))?
                        .op_hash(strand, hasher)?;
                    true
                }
                _ => false,
            }
        };
        if !did_hash {
            this.receiver.into_raw().hash(hasher);
        }
        Ok(())
    }
}
