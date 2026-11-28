use crate::{
    arg::Args,
    error::{Error, Result},
    object::{BoundMethod, protocol::instance_get_fallback},
    strand::Strand,
    sym::{self, Sym},
    unpack,
    value::{Input, Slot, prim::Prim},
    vm::AllocExt,
};

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
    strand.store(&mut out, &mut first);
    for mut value in rest {
        let replace = if is_min {
            value.lt(strand, &out)?
        } else {
            out.lt(strand, &value)?
        };
        if replace {
            strand.store(&mut out, &mut value);
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
            if upper.lt(strand, &lower)? {
                return Err(Error::value(
                    strand,
                    "clamp: lower bound exceeds upper bound",
                ));
            }
            if value.lt(strand, &lower)? {
                strand.store(&mut out, &mut lower);
            } else if upper.lt(strand, &value)? {
                strand.store(&mut out, &mut upper);
            } else {
                strand.store(&mut out, &mut value);
            }
            Ok(())
        }
        _ => unreachable!(),
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
            strand.store(out, value);
            Ok(())
        }
        sym::SIGNUM => {
            unpack!(strand, args =>);
            strand.store(out, value.signum());
            Ok(())
        }
        sym::ROUND | sym::FLOOR | sym::CEIL | sym::TRUNC => {
            unpack!(strand, args =>);
            strand.store(out, rcvr);
            Ok(())
        }
        sym::MIN | sym::MAX | sym::CLAMP => default_mcall(strand, rcvr, method, args, out),
        _ => Err(Error::field(strand, method)),
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
        strand.store(out, result);
        Ok(())
    } else if matches!(method.tag(), sym::MIN | sym::MAX | sym::CLAMP) {
        default_mcall(strand, rcvr, method, args, out)
    } else {
        Err(Error::field(strand, method))
    }
}
