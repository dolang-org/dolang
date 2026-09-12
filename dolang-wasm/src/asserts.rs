//! A `test` module providing the assertion functions of the shell's `test`
//! module, so examples written against it run in the playground.

use dolang::runtime::{Error, Strand, call, unpack, vm::Builder};

/// Evaluates an optional message argument, treating a falsy value as absent.
macro_rules! message {
    ($strand:expr, $msg:expr) => {
        match $msg {
            Some(msg) if msg.to_bool($strand) => Some(msg.to_string($strand)?),
            _ => None,
        }
    };
}

fn fail<'v, 's>(
    strand: &mut Strand<'v, 's>,
    detail: Option<String>,
    msg: Option<String>,
) -> Error<'v, 's> {
    let text = match (detail, msg) {
        (Some(detail), Some(msg)) => format!("assertion failed: {detail}: {msg}"),
        (Some(text), None) | (None, Some(text)) => format!("assertion failed: {text}"),
        (None, None) => "assertion failed".to_owned(),
    };
    Error::runtime(strand, text)
}

pub(crate) fn configure(builder: &mut Builder<'_>) {
    let str_sym = builder.sym("str");
    let msg_sym = builder.sym("msg");
    builder
        .module("test")
        .function("assert", async move |strand, args, _| {
            let ([cond], [msg]) = unpack!(strand, args, 1, 1)?;
            if !cond.to_bool(strand) {
                let msg = message!(strand, msg);
                return Err(fail(strand, None, msg));
            }
            Ok(())
        })
        .function("assert_not", async move |strand, args, _| {
            let ([cond], [msg]) = unpack!(strand, args, 1, 1)?;
            if cond.to_bool(strand) {
                let msg = message!(strand, msg);
                return Err(fail(strand, None, msg));
            }
            Ok(())
        })
        .function("assert_eq", async move |strand, args, _| {
            let ([left, right], [msg]) = unpack!(strand, args, 2, 1)?;
            if !left.eq(strand, &right) {
                let detail = format!("{} == {}", left.to_debug(strand)?, right.to_debug(strand)?);
                let msg = message!(strand, msg);
                return Err(fail(strand, Some(detail), msg));
            }
            Ok(())
        })
        .function("assert_ne", async move |strand, args, _| {
            let ([left, right], [msg]) = unpack!(strand, args, 2, 1)?;
            if !left.ne(strand, &right) {
                let detail = format!("{} != {}", left.to_debug(strand)?, right.to_debug(strand)?);
                let msg = message!(strand, msg);
                return Err(fail(strand, Some(detail), msg));
            }
            Ok(())
        })
        .function("assert_throws", async move |strand, args, mut out| {
            let ([ty, block], [expected, msg]) =
                unpack!(strand, args, 2, 0, str_sym = None, msg_sym = None)?;
            let msg = message!(strand, msg);
            match call!(strand, &block, &mut out).await {
                Ok(()) => {
                    let detail = format!("{} thrown", ty.to_debug(strand)?);
                    Err(fail(strand, Some(detail), msg))
                }
                Err(mut error) if error.catchable() => {
                    error.get_value(strand, &mut out);
                    if !out.is_instance_of(strand, &ty) {
                        return Err(error);
                    }
                    if let Some(expected) = expected {
                        let actual = out.to_string(strand)?;
                        let expected = expected.to_string(strand)?;
                        if actual != expected {
                            let detail = format!("error str {actual:?} == {expected:?}");
                            return Err(fail(strand, Some(detail), msg));
                        }
                    }
                    // The caught error is the result, as in the shell's module.
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })
        .function("assert_type", async move |strand, args, _| {
            let ([expected, value], [msg]) = unpack!(strand, args, 2, 1)?;
            if !value.is_instance_of(strand, &expected) {
                let detail = format!(
                    "{} is not an instance of {}",
                    value.to_debug(strand)?,
                    expected.to_debug(strand)?
                );
                let msg = message!(strand, msg);
                return Err(fail(strand, Some(detail), msg));
            }
            Ok(())
        })
        .commit();
}
