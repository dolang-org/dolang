use std::ops::ControlFlow;

use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    gc::{Collect, arena::Visit},
    object::{
        BoundMethod,
        protocol::{Inspect, Member, Protocol, Recv, dispatch_native_method, members},
    },
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Input, Output, Slot, Value, fmt::Format},
    vm::Vm,
};

pub(crate) struct Num;

unsafe impl Collect for Num {
    const CYCLIC: bool = false;
    const IMMUTABLE: bool = true;
    type Annex = ();

    fn accept(&self, _visit: &mut dyn Visit) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn clear(&mut self) {}
}

fn num_members<'v, 'a>() -> &'a [Member<'v, 'a>] {
    members![
        Method(sym::ADD_METHOD),
        Method(sym::SUB_METHOD),
        Method(sym::RSUB_METHOD),
        Method(sym::MUL_METHOD),
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
        _ => Err(Error::field(strand, field)),
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

async fn extrema<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    mut args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
    is_min: bool,
) -> Result<'v, 's, ()> {
    let Some(Arg::Pos(mut first)) = args.next() else {
        return Err(Error::missing_positional(strand, 0));
    };
    out.store(first.take());
    for arg in args {
        let mut value = match arg {
            Arg::Pos(value) => value,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };
        let replace = if is_min {
            value.op_lt(strand, &out)?.to_bool(strand)
        } else {
            out.op_lt(strand, &value)?.to_bool(strand)
        };
        if replace {
            out.store(value.take());
        }
        strand.check_trap_gc()?;
    }
    Ok(())
}

async fn default_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    method: Sym<'v, 'a>,
    mut args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    args.prepend_self(Value::from_input(strand.vm(), rcvr));
    match method.tag() {
        sym::MIN => extrema(strand, args, out, true).await,
        sym::MAX => extrema(strand, args, out, false).await,
        sym::CLAMP => {
            let ([mut value, mut lower, mut upper], []) = unpack!(strand, args, 3, 0)?;
            if upper.op_lt(strand, &lower)?.to_bool(strand) {
                return Err(Error::value(
                    strand,
                    "clamp: lower bound exceeds upper bound",
                ));
            }
            if value.op_lt(strand, &lower)?.to_bool(strand) {
                out.store(lower.take());
            } else if upper.op_lt(strand, &value)?.to_bool(strand) {
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
        sym::MIN | sym::MAX | sym::CLAMP => default_mcall(strand, rcvr, method, args, out).await,
        sym::ABS | sym::SIGNUM | sym::ROUND | sym::FLOOR | sym::CEIL | sym::TRUNC => {
            Err(Error::not_supported(strand))
        }
        _ => {
            let mut args = args;
            args.prepend_self(Value::from_input(strand.vm(), rcvr));
            dispatch_native_method(strand, &strand.singletons().num, method, args, out).await
        }
    }
}

pub(crate) async fn int_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    value: i128,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    mut out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let receiver = Value::from_input(strand.vm(), rcvr);
    match method.tag() {
        sym::ABS => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let value = value.checked_abs().ok_or_else(|| Error::overflow(strand))?;
            Output::set(strand, out, value);
            Ok(())
        }
        sym::SIGNUM => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            Output::set(strand, out, value.signum());
            Ok(())
        }
        sym::ROUND | sym::FLOOR | sym::CEIL | sym::TRUNC => {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            out.store(receiver);
            Ok(())
        }
        sym::MIN | sym::MAX | sym::CLAMP => {
            default_mcall(strand, &receiver, method, args, out).await
        }
        _ => Err(Error::field(strand, method)),
    }
}

pub(crate) async fn float_mcall<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    rcvr: impl Input<'v>,
    value: f64,
    method: Sym<'v, 'a>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let receiver = Value::from_input(strand.vm(), rcvr);
    let result = match method.tag() {
        sym::ABS => Some(Value::from_f64(strand, value.abs())),
        sym::SIGNUM => Some(Value::from_f64(strand, value.signum())),
        sym::ROUND => Some(Value::from_f64(strand, value.round())),
        sym::FLOOR => Some(Value::from_f64(strand, value.floor())),
        sym::CEIL => Some(Value::from_f64(strand, value.ceil())),
        sym::TRUNC => Some(Value::from_f64(strand, value.trunc())),
        sym::IS_NAN => Some(Value::from_bool(value.is_nan())),
        sym::IS_FINITE => Some(Value::from_bool(value.is_finite())),
        sym::IS_INFINITE => Some(Value::from_bool(value.is_infinite())),
        sym::IS_NORMAL => Some(Value::from_bool(value.is_normal())),
        sym::IS_SUBNORMAL => Some(Value::from_bool(value.is_subnormal())),
        _ => None,
    };
    if let Some(result) = result {
        let ([], []) = unpack!(strand, args, 0, 0)?;
        let mut out = out;
        out.store(result);
        Ok(())
    } else if matches!(method.tag(), sym::MIN | sym::MAX | sym::CLAMP) {
        default_mcall(strand, &receiver, method, args, out).await
    } else {
        Err(Error::field(strand, method))
    }
}

impl<'v> Protocol<'v> for Num {
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
        int_get(strand, &this, field, out)
    }

    async fn op_mcall<'a, 's>(
        this: Recv<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        method: Sym<'v, 'a>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        fallback_mcall(strand, &this, method, args, out).await
    }
}
