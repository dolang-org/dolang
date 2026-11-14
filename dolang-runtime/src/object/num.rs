use std::ops::ControlFlow;

use crate::{
    arg::Args,
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{
            Inspect, Member, Protocol, Recv, abstract_call, abstract_init, instance_get_fallback,
            instance_mcall_fallback, members, type_mcall_fallback,
        },
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Input, Output, Slot, Value, fmt::Format, prim::Prim},
    vm::Vm,
};

pub(crate) struct Num;

unsafe impl Collect for Num {
    const CYCLIC: bool = false;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

fn num_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    members![
        Method(sym::ADD_METHOD),
        Method(sym::RADD_METHOD),
        Method(sym::SUB_METHOD),
        Method(sym::RSUB_METHOD),
        Method(sym::MUL_METHOD),
        Method(sym::RMUL_METHOD),
        Method(sym::DIV_METHOD),
        Method(sym::RDIV_METHOD),
        Method(sym::EDIV_METHOD),
        Method(sym::REDIV_METHOD),
        Method(sym::MOD_METHOD),
        Method(sym::RMOD_METHOD),
        Method(sym::NEG_METHOD),
        Method(sym::EQ_METHOD),
        Method(sym::LT_METHOD),
        Method(sym::ABS),
        Method(sym::SIGNUM),
        Method(sym::ROUND),
        Method(sym::FLOOR),
        Method(sym::CEIL),
        Method(sym::TRUNC),
        Method(sym::MIN),
        Method(sym::MAX),
        Method(sym::CLAMP),
    ]
}

pub(crate) fn int_get<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::ABS
        | sym::SIGNUM
        | sym::ROUND
        | sym::FLOOR
        | sym::CEIL
        | sym::TRUNC
        | sym::MIN
        | sym::MAX
        | sym::CLAMP => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => instance_get_fallback(strand, rcvr, field, out),
    }
}

pub(crate) fn float_get<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    field: Sym<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match field.tag() {
        sym::IS_NAN | sym::IS_FINITE | sym::IS_INFINITE | sym::IS_NORMAL | sym::IS_SUBNORMAL => {
            BoundMethod::create(strand, rcvr, field, out);
            Ok(())
        }
        _ => int_get(strand, rcvr, field, out),
    }
}

fn extrema<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
    is_min: bool,
) -> Result<'v, 's, ()> {
    unpack!(strand, args => mut first, *rest);
    out.store(first.take());
    for mut value in rest {
        let replace = if is_min {
            value.op_lt(strand, &out)?.to_bool(strand)?
        } else {
            out.op_lt(strand, &value)?.to_bool(strand)?
        };
        if replace {
            out.store(value.take());
        }
        strand.check_trap_gc()?;
    }
    Ok(())
}

fn default_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    mut args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    args.prepend_self(strand, rcvr);
    match method.tag() {
        sym::MIN => extrema(strand, args, out, true),
        sym::MAX => extrema(strand, args, out, false),
        sym::CLAMP => {
            unpack!(strand, args => mut value, mut lower, mut upper);
            if upper.op_lt(strand, &lower)?.to_bool(strand)? {
                return Err(Error::value(
                    strand,
                    "clamp: lower bound exceeds upper bound",
                ));
            }
            if value.op_lt(strand, &lower)?.to_bool(strand)? {
                out.store(lower.take());
            } else if upper.op_lt(strand, &value)?.to_bool(strand)? {
                out.store(upper.take());
            } else {
                out.store(value.take());
            }
            Ok(())
        }
        _ => unreachable!(),
    }
}

async fn fallback_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match method.tag() {
        sym::MIN | sym::MAX | sym::CLAMP => default_mcall(strand, rcvr, method, args, out),
        sym::ABS | sym::SIGNUM | sym::ROUND | sym::FLOOR | sym::CEIL | sym::TRUNC => {
            let msg = format!("abstract method `{}`", method.as_str(strand));
            Err(Error::unsupported(strand, msg))
        }
        _ => {
            let mut args = args;
            args.prepend_self(strand, rcvr);
            type_mcall_fallback(strand, &strand.singletons().num, method, args, out).await
        }
    }
}

pub(crate) fn int_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    value: i128,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    match method.tag() {
        sym::ABS => {
            unpack!(strand, args =>);
            let value = value.checked_abs().ok_or_else(|| Error::overflow(strand))?;
            Output::set(strand, out, value);
            Ok(())
        }
        sym::SIGNUM => {
            unpack!(strand, args =>);
            Output::set(strand, out, value.signum());
            Ok(())
        }
        sym::ROUND | sym::FLOOR | sym::CEIL | sym::TRUNC => {
            unpack!(strand, args =>);
            Output::set(strand, out, rcvr);
            Ok(())
        }
        sym::MIN | sym::MAX | sym::CLAMP => default_mcall(strand, rcvr, method, args, out),
        _ => instance_mcall_fallback(strand, rcvr, method, args, out),
    }
}

pub(crate) fn float_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    value: f64,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let result = match method.tag() {
        sym::ABS => Some(Prim::F64(value.abs())),
        sym::SIGNUM => Some(Prim::F64(value.signum())),
        sym::ROUND => Some(Prim::F64(value.round())),
        sym::FLOOR => Some(Prim::F64(value.floor())),
        sym::CEIL => Some(Prim::F64(value.ceil())),
        sym::TRUNC => Some(Prim::F64(value.trunc())),
        sym::IS_NAN => Some(Prim::Bool(value.is_nan())),
        sym::IS_FINITE => Some(Prim::Bool(value.is_finite())),
        sym::IS_INFINITE => Some(Prim::Bool(value.is_infinite())),
        sym::IS_NORMAL => Some(Prim::Bool(value.is_normal())),
        sym::IS_SUBNORMAL => Some(Prim::Bool(value.is_subnormal())),
        _ => None,
    };
    if let Some(result) = result {
        unpack!(strand, args =>);
        Output::set(strand, out, result);
        Ok(())
    } else if matches!(method.tag(), sym::MIN | sym::MAX | sym::CLAMP) {
        default_mcall(strand, rcvr, method, args, out)
    } else {
        instance_mcall_fallback(strand, rcvr, method, args, out)
    }
}

impl<'v> Protocol<'v> for Num {
    fn op_radd<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        this.delegator()
            .ok_or_else(|| Error::unsupported(strand, "reflected `+`"))?
            .op_add_direct(strand, other)
    }

    fn op_rmul<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, Value<'v>> {
        this.delegator()
            .ok_or_else(|| Error::unsupported(strand, "reflected `*`"))?
            .op_mul_direct(strand, other)
    }

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
        fmt!(strand, w, "<type std.Num>")
    }

    fn op_inspect<'a>(_this: Recv<'v, 'a, Self>, _vm: &Vm<'v>) -> Option<Inspect<'v, 'a>> {
        Some(Inspect {
            is_abstract: true,
            is_final: false,
            members: num_members(),
            type_members: members![
                Method(sym::VERBATIM_METHOD),
                Method(sym::STR_METHOD),
                Method(sym::DBG_METHOD),
            ],
        })
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
            int_get(strand, &this, field, out)
        }
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if method.tag() == sym::INIT_METHOD && this.delegator().is_none() {
            return abstract_init(strand, &this, args);
        }
        fallback_mcall(strand, &this, method, args, out).await
    }

    async fn op_call<'a, 's>(
        _this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        _args: Args<'v, 'a>,
        _out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Err(abstract_call(strand, "std.Num"))
    }
}
