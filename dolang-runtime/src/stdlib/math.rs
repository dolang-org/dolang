use crate::{
    arg::{Arg, Args},
    error::{Error, Result},
    strand::Strand,
    unpack,
    value::{Output, Slot, Value, prim::Prim},
    vm::Builder,
};

fn as_float<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, f64> {
    match value.to_prim(strand)? {
        Prim::Int(value) => Ok(value as f64),
        Prim::F64(value) => Ok(value),
        _ => Err(Error::type_error(
            strand,
            format!("{name}: expected Int or Float"),
        )),
    }
}

fn as_int<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &str,
) -> Result<'v, 's, i128> {
    match value.to_prim(strand)? {
        Prim::Int(value) => Ok(value),
        _ => Err(Error::type_error(strand, format!("{name}: expected Int"))),
    }
}

async fn unary<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    name: &'static str,
    op: fn(f64) -> f64,
) -> Result<'v, 's, ()> {
    let ([value], []) = unpack!(strand, args, 1, 0)?;
    let value = as_float(strand, &value, name)?;
    Output::set(strand, out, op(value));
    Ok(())
}

async fn binary<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
    name: &'static str,
    op: fn(f64, f64) -> f64,
) -> Result<'v, 's, ()> {
    let ([left, right], []) = unpack!(strand, args, 2, 0)?;
    let left = as_float(strand, &left, name)?;
    let right = as_float(strand, &right, name)?;
    Output::set(strand, out, op(left, right));
    Ok(())
}

fn gcd_pair(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

async fn gcd<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let ([first], [], rest) = unpack!(strand, args, 1, 0, ...)?;
    let mut result = as_int(strand, &first, "gcd")?.unsigned_abs();
    for arg in rest {
        let value = match arg {
            Arg::Pos(value) => value,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };
        result = gcd_pair(result, as_int(strand, &value, "gcd")?.unsigned_abs());
    }
    let result = i128::try_from(result).map_err(|_| Error::overflow(strand))?;
    Output::set(strand, out, result);
    Ok(())
}

async fn lcm<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let ([first], [], rest) = unpack!(strand, args, 1, 0, ...)?;
    let mut result = as_int(strand, &first, "lcm")?.unsigned_abs();
    if result > i128::MAX as u128 {
        return Err(Error::overflow(strand));
    }
    for arg in rest {
        let value = match arg {
            Arg::Pos(value) => value,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };
        let value = as_int(strand, &value, "lcm")?.unsigned_abs();
        result = if result == 0 || value == 0 {
            0
        } else {
            (result / gcd_pair(result, value))
                .checked_mul(value)
                .ok_or_else(|| Error::overflow(strand))?
        };
        if result > i128::MAX as u128 {
            return Err(Error::overflow(strand));
        }
    }
    Output::set(strand, out, result as i128);
    Ok(())
}

fn checked_pow<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut base: i128,
    exponent: i128,
) -> Result<'v, 's, i128> {
    let mut exponent = exponent as u128;
    let mut result = 1i128;
    while exponent != 0 {
        if exponent & 1 != 0 {
            result = result
                .checked_mul(base)
                .ok_or_else(|| Error::overflow(strand))?;
        }
        exponent >>= 1;
        if exponent != 0 {
            base = base
                .checked_mul(base)
                .ok_or_else(|| Error::overflow(strand))?;
        }
    }
    Ok(result)
}

async fn pow<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let ([base, exponent], []) = unpack!(strand, args, 2, 0)?;
    let base_prim = base.to_prim(strand)?;
    let exponent_prim = exponent.to_prim(strand)?;
    match (base_prim, exponent_prim) {
        (Prim::Int(base), Prim::Int(exponent)) if exponent >= 0 => {
            let value = checked_pow(strand, base, exponent)?;
            Output::set(strand, out, value);
        }
        (Prim::Int(base), Prim::Int(exponent)) => {
            Output::set(strand, out, (base as f64).powf(exponent as f64));
        }
        (Prim::Int(base), Prim::F64(exponent)) => {
            Output::set(strand, out, (base as f64).powf(exponent));
        }
        (Prim::F64(base), Prim::Int(exponent)) => {
            Output::set(strand, out, base.powf(exponent as f64));
        }
        (Prim::F64(base), Prim::F64(exponent)) => {
            Output::set(strand, out, base.powf(exponent));
        }
        _ => return Err(Error::type_error(strand, "pow: expected Int or Float")),
    }
    Ok(())
}

async fn hypot<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let ([first, second], [], rest) = unpack!(strand, args, 2, 0, ...)?;
    let mut result = as_float(strand, &first, "hypot")?.hypot(as_float(strand, &second, "hypot")?);
    for arg in rest {
        let value = match arg {
            Arg::Pos(value) => value,
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        };
        result = result.hypot(as_float(strand, &value, "hypot")?);
    }
    Output::set(strand, out, result);
    Ok(())
}

macro_rules! unary {
    ($builder:expr, $name:literal, $method:ident) => {
        $builder.function($name, async |strand, args, out| {
            unary(strand, args, out, $name, f64::$method).await
        })
    };
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    let base = builder.sym("base");
    let module = builder.module("math");
    let module = unary!(module, "sqrt", sqrt);
    let module = unary!(module, "cbrt", cbrt);
    let module = unary!(module, "exp", exp);
    let module = unary!(module, "expm1", exp_m1);
    let module = unary!(module, "exp2", exp2);
    let module = unary!(module, "ln", ln);
    let module = unary!(module, "ln1p", ln_1p);
    let module = unary!(module, "log2", log2);
    let module = unary!(module, "log10", log10);
    let module = unary!(module, "sin", sin);
    let module = unary!(module, "cos", cos);
    let module = unary!(module, "tan", tan);
    let module = unary!(module, "asin", asin);
    let module = unary!(module, "acos", acos);
    let module = unary!(module, "atan", atan);
    let module = unary!(module, "sinh", sinh);
    let module = unary!(module, "cosh", cosh);
    let module = unary!(module, "tanh", tanh);
    let module = unary!(module, "asinh", asinh);
    let module = unary!(module, "acosh", acosh);
    let module = unary!(module, "atanh", atanh);
    let module = unary!(module, "degrees", to_degrees);
    let module = unary!(module, "radians", to_radians);
    module
        .function("atan2", async |strand, args, out| {
            binary(strand, args, out, "atan2", f64::atan2).await
        })
        .function("copysign", async |strand, args, out| {
            binary(strand, args, out, "copysign", f64::copysign).await
        })
        .function("log", async move |strand, args, out| {
            let ([value, base_value], []) = unpack!(strand, args, 1, 0, base)?;
            let value = as_float(strand, &value, "log")?;
            let base_value = as_float(strand, &base_value, "log")?;
            Output::set(strand, out, value.log(base_value));
            Ok(())
        })
        .function("pow", pow)
        .function("hypot", hypot)
        .function("gcd", gcd)
        .function("lcm", lcm)
        .value("PI", std::f64::consts::PI)
        .value("TAU", std::f64::consts::TAU)
        .value("E", std::f64::consts::E)
        .commit();
}
