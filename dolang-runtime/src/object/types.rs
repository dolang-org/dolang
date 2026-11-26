//! Builtin primitive type class objects

use std::ops::ControlFlow;

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{self, Collect, arena::Visit, with_pin},
    object::{
        BoundMethod, class,
        protocol::{
            Inspect, Protocol, Recv, abstract_call, abstract_init, instance_get_fallback, members,
            recv_special_mcall, type_mcall_fallback, value_members,
        },
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Slot, TypeObject, Value as DoValue},
    vm::{AllocExt, Vm},
    with_slots,
};

pub(crate) struct Value;

unsafe impl Collect for Value {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Value {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Value";
    const TYPE_OBJECT: bool = true;

    fn op_req<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &DoValue<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        this.delegator()
            .ok_or_else(|| Error::unsupported(strand, "reflected `==`"))?
            .op_eq_direct(strand, other, out)
    }

    fn op_rne<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &DoValue<'v>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        this.delegator()
            .ok_or_else(|| Error::unsupported(strand, "reflected `!=`"))?
            .op_ne_direct(strand, other, out)
    }

    /// The default source form of a value is its string form, whatever that is.
    fn op_verbatim<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match this.delegator() {
            Some(delegator) => delegator.op_display(strand, w),
            None => Self::op_display(this, strand, w),
        }
    }

    /// The default string form of a value is its debug form, whatever that is.
    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match this.delegator() {
            Some(delegator) => delegator.op_debug(strand, w),
            None => crate::fmt!(strand, w, "{}.{}", Self::MODULE, Self::NAME),
        }
    }

    /// The default debug form of a value names its type, as `<std.Int>`.
    fn op_debug<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match this.delegator() {
            Some(delegator) => with_slots!(strand, |mut ty| {
                delegator.op_type(strand, Slot::reborrow(&mut ty));
                w.write_str(strand, "<")?;
                ty.op_display(strand, w)?;
                w.write_str(strand, ">")
            }),
            None => {
                w.write_str(strand, "<type ")?;
                Self::op_display(this, strand, w)?;
                w.write_str(strand, ">")
            }
        }
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            is_final: false,
            members: value_members(),
            type_members: members![],
        })
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            instance_get_fallback(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD if this.delegator().is_none() => abstract_init(strand, &this, args),
            // Unbound `Value.(eq)` and `Value.(ne)` skip the operands' own
            // comparisons and test for the same value.
            tag @ (sym::EQ_METHOD | sym::NE_METHOD) if this.delegator().is_none() => {
                unpack!(strand, args => left, right);
                let same = left.repr_eq(&right);
                strand.store(out, same == (tag == sym::EQ_METHOD));
                Ok(())
            }
            _ => recv_special_mcall(strand, this, method, args, out).await,
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Value"))
    }
}

pub(crate) struct Type;

unsafe impl Collect for Type {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Type {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Type";
    const TYPE_OBJECT: bool = true;

    /// The string form of a type is its qualified name, which only the type itself
    /// knows.
    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        match this.delegator() {
            Some(delegator) => delegator.op_display(strand, w),
            None => crate::fmt!(strand, w, "{}.{}", Self::MODULE, Self::NAME),
        }
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            is_final: false,
            members: value_members(),
            type_members: members![],
        })
    }

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    /// A type object is called to construct its instances, so it is a `Func`
    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &DoValue<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype
                .eq(strand, &strand.singletons().func)
                .unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }

    /// `Type.(call) SomeType ...` performs default instantiation.
    ///
    /// This is the ordinary unbound-method idiom — `Class` is an instance of
    /// `Type`, so this invokes `Type`'s `(call)` rather than the receiver's
    /// override, the same way `Base.method $self` does. It is how a class-level
    /// `(call)` delegates to the construction it replaced.
    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::CALL_METHOD => {
                unpack!(strand, args => receiver, ...trailing);
                if !receiver.is_instance_of(strand, &strand.singletons().type_obj) {
                    return Err(Error::type_error(
                        strand,
                        "Type.(call): expected a type object",
                    ));
                }
                match receiver.downcast_ref(strand.builtin_types().class_object) {
                    Some(class) => {
                        with_pin!(strand, class, |class| {
                            class::instantiate(Recv::new(class), strand, trailing, out).await
                        })
                    }
                    // A native type object has no overridable `(call)`, so its
                    // own `op_call` is already the default.
                    None => receiver.op_call(strand, trailing, out).await,
                }
            }
            sym::INIT_METHOD if this.delegator().is_none() => abstract_init(strand, &this, args),
            _ => recv_special_mcall(strand, this, method, args, out).await,
        }
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if field.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            BoundMethod::create(strand, &this, field, out);
            Ok(())
        } else {
            instance_get_fallback(strand, &this, field, out)
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "Type"))
    }
}

pub(crate) struct Bool;
pub(crate) struct ArgAccType;

pub(crate) struct NilType;

unsafe impl Collect for Bool {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for NilType {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for ArgAccType {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Bool {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Bool";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        unpack!(strand, args => value);
        let value = value
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "Bool: expected Bool"))?;
        strand.store(out, value);
        Ok(())
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: true,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                ..Value,
                Method(sym::FMT_METHOD),
                Method(sym::BAND_METHOD),
                Method(sym::RBAND_METHOD),
                Method(sym::BOR_METHOD),
                Method(sym::RBOR_METHOD),
                Method(sym::BXOR_METHOD),
                Method(sym::RBXOR_METHOD),
                Method(sym::BNOT_METHOD),
                Method(sym::BOOL_METHOD),
            ],
        })
    }

    fn op_get<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        field: Sym<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match field.tag() {
            sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::REQ_METHOD
            | sym::NE_METHOD
            | sym::RNE_METHOD
            | sym::BAND_METHOD
            | sym::RBAND_METHOD
            | sym::BOR_METHOD
            | sym::RBOR_METHOD
            | sym::BXOR_METHOD
            | sym::RBXOR_METHOD
            | sym::BNOT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => instance_get_fallback(strand, &this, field, out),
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let vm = strand.vm();
        type_mcall_fallback(strand, &vm.singletons().bool, method, args, out).await
    }
}

impl<'v> Protocol<'v> for ArgAccType {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "ArgAcc";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    fn op_subtype<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        supertype: &DoValue<'v>,
    ) -> bool {
        supertype.eq(strand, &this).unwrap_or(false)
            || supertype.eq(strand, TypeObject::Value).unwrap_or(false)
    }
}

impl<'v> Protocol<'v> for NilType {
    const MODULE: &'v str = "std";
    const NAME: &'v str = "Nil";
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj)
    }

    // Final, so no class inherits these members. The lists stay empty until
    // inspection serves something besides inheritance.
    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            is_final: true,
            members: members![..Value],
            type_members: &[],
        })
    }
}

/// Private type identity for adapters that expose only type-only protocols.
pub(crate) struct AdapterType(pub(crate) &'static str);

unsafe impl gc::ImmState for AdapterType {
    const CYCLIC: bool = false;
}

impl gc::Annex for AdapterType {
    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

pub(crate) type AdapterTypeObj = gc::Imm<AdapterType>;

impl<'v> Protocol<'v> for AdapterTypeObj {
    const NATIVE_FRAMES: bool = false;
    const TYPE_OBJECT: bool = true;

    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        strand.store(out, &strand.singletons().type_obj);
    }
    fn op_display<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "{}", this.get().0)
    }
}
