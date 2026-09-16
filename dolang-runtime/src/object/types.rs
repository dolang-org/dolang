//! Builtin primitive type class objects

use std::ops::ControlFlow;

use crate::value::fmt::Format;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        arg::ArgPack,
        class,
        protocol::{GcObj, Inspect, Protocol, Recv, members, type_mcall_fallback},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Output, Slot, TypeObject, Value as DoValue},
    vm::Vm,
};

pub(crate) struct Value;

unsafe impl Collect for Value {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Value {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type Value>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "Value is not instantiable"))
    }
}

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

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type>")
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
                let ([receiver], [], trailing) = unpack!(strand, args, 1, 0, ...)?;
                if !receiver.is_instance_of(strand, &strand.singletons().type_obj) {
                    return Err(Error::type_error(
                        strand,
                        "Type.(call): expected a type object",
                    ));
                }
                match receiver.downcast_ref(strand.builtin_types().class_object) {
                    Some(class) => {
                        class::instantiate(Recv::new(class), strand, trailing, out).await
                    }
                    // A native type object has no overridable `(call)`, so its
                    // own `op_call` is already the default.
                    None => receiver.op_call(strand, trailing, out).await,
                }
            }
            sym::GET_METHOD => {
                let ([field], []) = unpack!(strand, args, 1, 0)?;
                let field = field
                    .as_sym(strand)
                    .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
                Self::op_get(this, strand, field, out)
            }
            sym::SET_METHOD => {
                let ([field, value], []) = unpack!(strand, args, 2, 0)?;
                let field = field
                    .as_sym(strand)
                    .ok_or_else(|| Error::type_error(strand, "field: expected `Sym`"))?;
                Self::op_set(this, strand, field, value)
            }
            _ => {
                strand
                    .with_slots(async move |strand, [mut func]| {
                        Self::op_get(this, strand, method, Slot::reborrow(&mut func))?;
                        func.op_call(strand, args, out).await
                    })
                    .await
            }
        }
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(Error::type_error(strand, "Type is not instantiable"))
    }
}

pub(crate) struct Bool;
pub(crate) struct ArgsType;

pub(crate) struct NilType;

unsafe impl Collect for Bool {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for NilType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

unsafe impl Collect for ArgsType {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

impl<'v> Protocol<'v> for Bool {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Bool>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([value], []) = unpack!(strand, args, 1, 0)?;
        let value = value
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "Bool: expected Bool"))?;
        Output::set(strand, out, value);
        Ok(())
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: false,
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::CALL_METHOD),
            ],
            members: members![
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
                Method(sym::FMT_METHOD),
                Method(sym::EQ_METHOD),
                Method(sym::BAND_METHOD),
                Method(sym::BOR_METHOD),
                Method(sym::BXOR_METHOD),
                Method(sym::BNOT_METHOD),
                Method(sym::BOOL_METHOD),
                Method(sym::HASH_METHOD),
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
            sym::INIT_METHOD
            | sym::STR_METHOD
            | sym::DBG_METHOD
            | sym::FMT_METHOD
            | sym::EQ_METHOD
            | sym::BAND_METHOD
            | sym::BOR_METHOD
            | sym::BXOR_METHOD
            | sym::BNOT_METHOD
            | sym::BOOL_METHOD
            | sym::HASH_METHOD => {
                BoundMethod::create(strand, &this, field, out);
                Ok(())
            }
            _ => Err(Error::field(strand, field)),
        }
    }

    async fn op_mcall<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        match method.tag() {
            sym::INIT_METHOD => {
                let ([self_val, value], []) = unpack!(strand, args, 2, 0)?;
                let value = value
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "Bool: expected Bool"))?;
                let native = DoValue::from_bool(value);
                self_val.op_fill(strand, &strand.singletons().bool, native)?;
                Ok(())
            }
            _ => {
                let vm = strand.vm();
                type_mcall_fallback(strand, &vm.singletons().bool, method, args, out).await
            }
        }
    }
}

impl<'v> Protocol<'v> for ArgsType {
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
        supertype: &DoValue<'v>,
    ) -> bool {
        let sings = strand.singletons();
        supertype.eq(strand, &this)
            || supertype.eq(strand, TypeObject::Value)
            || sings.iterable.eq(strand, supertype)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Args>")
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        out.store(DoValue::from_object(GcObj::new(
            strand.arena(),
            strand.builtin_types().arg_pack,
            ArgPack::from_args(strand, args),
        )));
        Ok(())
    }
}

impl<'v> Protocol<'v> for NilType {
    fn op_type<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) {
        Output::set(strand, out, &strand.singletons().type_obj)
    }

    fn op_debug<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        crate::fmt!(strand, w, "<type std.Nil>")
    }
}

#[cfg(test)]
mod tests {
    use crate::{call, error::ErrorKind, method, test_support::with_vm};

    use super::*;

    #[test]
    fn value_op_call_errors_not_instantiable() {
        with_vm(async |strand, [mut out]| {
            let err = call!(strand, &strand.singletons().value, &mut out)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn type_op_call_errors_not_instantiable() {
        with_vm(async |strand, [mut out]| {
            let err = call!(strand, &strand.singletons().type_obj, &mut out)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn bool_op_call_converts_bool_and_rejects_non_bool() {
        with_vm(async |strand, [mut out]| {
            call!(strand, &strand.singletons().bool, &mut out, true)
                .await
                .unwrap();
            let result: &DoValue = &out;
            assert!(result.to_bool(strand));

            let err = call!(strand, &strand.singletons().bool, &mut out, 1_i64)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Type);
        });
    }

    #[test]
    fn bool_op_get_known_method_succeeds_unknown_field_errors() {
        with_vm(async |strand, [mut ok_out, unused_out]| {
            let bool_recv = strand
                .builtin_types()
                .bool_type
                .cast(&strand.singletons().bool)
                .unwrap();
            bool_recv.enter_sync(strand, |strand, recv| {
                Bool::op_get(
                    recv,
                    strand,
                    Sym::well_known(sym::STR_METHOD),
                    Slot::reborrow(&mut ok_out),
                )
                .unwrap();
            });
            let bound: &DoValue = &ok_out;
            assert!(!bound.is_nil());

            let bool_recv = strand
                .builtin_types()
                .bool_type
                .cast(&strand.singletons().bool)
                .unwrap();
            bool_recv.enter_sync(strand, |strand, recv| {
                let err =
                    Bool::op_get(recv, strand, Sym::well_known(sym::LEN), unused_out).unwrap_err();
                assert_eq!(err.kind(), ErrorKind::Field);
            });
        });
    }

    #[test]
    fn bool_op_mcall_default_dispatch_reaches_op_band() {
        with_vm(async |strand, [mut out]| {
            method!(
                strand,
                &strand.singletons().bool,
                Sym::well_known(sym::BAND_METHOD),
                &mut out,
                true,
                false
            )
            .await
            .unwrap();
            let result: &DoValue = &out;
            assert!(!result.to_bool(strand));
        });
    }

    #[test]
    fn args_type_op_subtype_and_op_call() {
        with_vm(async |strand, [mut out]| {
            let args_recv = strand
                .builtin_types()
                .args_type
                .cast(&strand.singletons().args)
                .unwrap();
            args_recv.enter_sync(strand, |strand, recv| {
                assert!(ArgsType::op_subtype(
                    recv,
                    strand,
                    &strand.singletons().args
                ));
            });
            let args_recv = strand
                .builtin_types()
                .args_type
                .cast(&strand.singletons().args)
                .unwrap();
            args_recv.enter_sync(strand, |strand, recv| {
                assert!(ArgsType::op_subtype(
                    recv,
                    strand,
                    &strand.singletons().iterable
                ));
            });
            let args_recv = strand
                .builtin_types()
                .args_type
                .cast(&strand.singletons().args)
                .unwrap();
            args_recv.enter_sync(strand, |strand, recv| {
                assert!(!ArgsType::op_subtype(
                    recv,
                    strand,
                    &strand.singletons().int
                ));
            });

            call!(strand, &strand.singletons().args, &mut out)
                .await
                .unwrap();
            let result: &DoValue = &out;
            assert!(
                result
                    .downcast_ref(strand.builtin_types().arg_pack)
                    .is_some()
            );
        });
    }
}
