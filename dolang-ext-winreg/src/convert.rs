//! Conversion between `dolang_vfs_winreg::Value` and Do values.
//!
//! Recognized `REG_*` kinds map to their natural Do type (`Str`/`Array` of
//! `Str`/`Int`/`Bin`/`Nil`); anything else (`Value::Other`) surfaces as raw
//! `Bin`, the same shape [`crate::value_entry::ValueEntry`]'s `.value` field
//! uses for an unrecognized kind.

use dolang::runtime::{
    Error, Output, Result, Slot, Strand,
    value::{AsTuple, Nil, View},
};
use dolang_vfs_winreg::Value;

/// Writes the natural Do representation of `value` to `out`.
pub(crate) fn to_do<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    match value {
        Value::Sz(s) | Value::ExpandSz(s) => {
            Output::set(strand, out, s.as_str());
        }
        Value::MultiSz(parts) => {
            Output::set(strand, out, AsTuple::new(parts.iter().map(String::as_str)))
        }
        Value::Dword(v) | Value::DwordBigEndian(v) => {
            Output::set(strand, out, i128::from(*v));
        }
        Value::Qword(v) => {
            Output::set(strand, out, i128::from(*v));
        }
        Value::Binary(data) => {
            Output::set(strand, out, data.as_slice());
        }
        Value::None => {
            Output::set(strand, out, Nil);
        }
        Value::Other { data, .. } => {
            Output::set(strand, out, data.as_slice());
        }
    }
    Ok(())
}

/// Which `REG_*` kind to build when converting a Do value into a
/// [`Value`] — either forced by an explicit `kind:` override, or inferred
/// from the kind of a value already stored under the target name.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Sz,
    ExpandSz,
    MultiSz,
    Dword,
    DwordBigEndian,
    Qword,
    Binary,
    None,
}

impl Kind {
    /// The [`Kind`] of an existing [`Value`], or `None` for
    /// [`Value::Other`] (an unrecognized kind, handled separately since it
    /// has no selectable [`Kind`]).
    fn of(value: &Value) -> Option<Kind> {
        match value {
            Value::Sz(_) => Some(Kind::Sz),
            Value::ExpandSz(_) => Some(Kind::ExpandSz),
            Value::MultiSz(_) => Some(Kind::MultiSz),
            Value::Dword(_) => Some(Kind::Dword),
            Value::DwordBigEndian(_) => Some(Kind::DwordBigEndian),
            Value::Qword(_) => Some(Kind::Qword),
            Value::Binary(_) => Some(Kind::Binary),
            Value::None => Some(Kind::None),
            Value::Other { .. } => None,
        }
    }
}

/// Converts a Do value into a [`Value`] to write.
///
/// - `kind`, if given, forces the exact `REG_*` kind (an explicit `kind:`
///   override on `set`).
/// - Otherwise, if `existing` is `Some`, the input is coerced into that
///   value's kind (`Value::Other` round-trips: writing `Bin` back keeps the
///   original raw kind).
/// - Otherwise a default kind is inferred from the Do value's own type:
///   `Str` -> `Sz`, `Array` of `Str` -> `MultiSz`, `Int` -> `Dword` if it
///   fits in 32 bits else `Qword`, `Bin` -> `Binary`, `Nil` -> `None`.
pub(crate) async fn from_do<'v, 's>(
    strand: &mut Strand<'v, 's>,
    existing: Option<&Value>,
    kind: Option<Kind>,
    input: Slot<'v, '_>,
) -> Result<'v, 's, Value> {
    if let Some(kind) = kind {
        return build(strand, kind, input).await;
    }
    match existing {
        Some(Value::Other { kind, .. }) => Ok(Value::Other {
            kind: *kind,
            data: expect_bin(strand, input)?,
        }),
        Some(existing) => build(strand, Kind::of(existing).unwrap(), input).await,
        None => default_from_do(strand, input).await,
    }
}

async fn build<'v, 's>(
    strand: &mut Strand<'v, 's>,
    kind: Kind,
    input: Slot<'v, '_>,
) -> Result<'v, 's, Value> {
    match kind {
        Kind::Sz => Ok(Value::Sz(expect_str(strand, input)?)),
        Kind::ExpandSz => Ok(Value::ExpandSz(expect_str(strand, input)?)),
        Kind::MultiSz => Ok(Value::MultiSz(expect_str_iterable(strand, input).await?)),
        Kind::Dword => Ok(Value::Dword(expect_u32(strand, input)?)),
        Kind::DwordBigEndian => Ok(Value::DwordBigEndian(expect_u32(strand, input)?)),
        Kind::Qword => Ok(Value::Qword(expect_u64(strand, input)?)),
        Kind::Binary => Ok(Value::Binary(expect_bin(strand, input)?)),
        Kind::None => Ok(Value::None),
    }
}

async fn default_from_do<'v, 's>(
    strand: &mut Strand<'v, 's>,
    input: Slot<'v, '_>,
) -> Result<'v, 's, Value> {
    match input.view(strand.vm()) {
        View::Str(_) => Ok(Value::Sz(expect_str(strand, input)?)),
        View::Array(_) | View::Tuple(_) | View::Object(_) => {
            Ok(Value::MultiSz(expect_str_iterable(strand, input).await?))
        }
        View::Int(i) => match u32::try_from(i) {
            Ok(v) => Ok(Value::Dword(v)),
            Err(_) => Ok(Value::Qword(u64::try_from(i).map_err(|_| {
                Error::value(strand, "integer out of range for a registry value")
            })?)),
        },
        View::Bin(_) => Ok(Value::Binary(expect_bin(strand, input)?)),
        View::Nil => Ok(Value::None),
        _ => Err(Error::type_error(
            strand,
            "expected Str, iterable of Str, Int, Bin, or Nil",
        )),
    }
}

fn expect_str<'v, 's>(strand: &mut Strand<'v, 's>, input: Slot<'v, '_>) -> Result<'v, 's, String> {
    match input.view(strand.vm()) {
        View::Str(s) => Ok(strand.access(|access| s.as_str(access).to_string())),
        _ => Err(Error::type_error(strand, "expected Str")),
    }
}

async fn expect_str_iterable<'v, 's>(
    strand: &mut Strand<'v, 's>,
    input: Slot<'v, '_>,
) -> Result<'v, 's, Vec<String>> {
    strand
        .with_slots(async move |strand, [mut iter, mut item]| {
            input.iter(strand, &mut iter).await?;
            let mut parts = Vec::new();
            while iter.next(strand, &mut item).await? {
                parts.push(expect_str(strand, Slot::reborrow(&mut item))?);
            }
            Ok(parts)
        })
        .await
}

fn expect_u32<'v, 's>(strand: &mut Strand<'v, 's>, input: Slot<'v, '_>) -> Result<'v, 's, u32> {
    match input.view(strand.vm()) {
        View::Int(i) => {
            u32::try_from(i).map_err(|_| Error::value(strand, "expected a value that fits DWORD"))
        }
        _ => Err(Error::type_error(strand, "expected Int")),
    }
}

fn expect_u64<'v, 's>(strand: &mut Strand<'v, 's>, input: Slot<'v, '_>) -> Result<'v, 's, u64> {
    match input.view(strand.vm()) {
        View::Int(i) => {
            u64::try_from(i).map_err(|_| Error::value(strand, "expected a value that fits QWORD"))
        }
        _ => Err(Error::type_error(strand, "expected Int")),
    }
}

fn expect_bin<'v, 's>(strand: &mut Strand<'v, 's>, input: Slot<'v, '_>) -> Result<'v, 's, Vec<u8>> {
    match input.view(strand.vm()) {
        View::Bin(bin) => Ok(strand.access(|access| bin.as_slice(access).to_vec())),
        _ => Err(Error::type_error(strand, "expected Bin")),
    }
}
