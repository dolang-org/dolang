use dolang::runtime::{Error, Input, Output, Result, Slot, Strand, Sym, Value, value::View};

pub(crate) fn bool<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    name: &'static str,
) -> Result<'v, 's, bool> {
    value
        .as_bool(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{name}: expected Bool")))
}

pub(crate) fn string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &'static str,
) -> Result<'v, 's, String> {
    value
        .as_str(strand)
        .map(|value| value.to_string())
        .ok_or_else(|| Error::type_error(strand, format!("{name}: expected Str")))
}

pub(crate) fn bytes<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &'static str,
) -> Result<'v, 's, Vec<u8>> {
    match value.view(strand) {
        View::Str(value) => Ok(value.to_string().into()),
        View::Bin(value) => Ok(value.to_vec()),
        _ => Err(Error::type_error(
            strand,
            format!("{name}: expected `Str` or `Bin`"),
        )),
    }
}

/// Writes an optional value, keeping absence distinct from a missing member.
pub(crate) fn nullable_field<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<impl Input<'v>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    match value {
        Some(value) => Output::set(strand, out, value),
        None => Output::set(strand, out, dolang::runtime::value::Nil),
    }
    Ok(())
}

pub(crate) fn option_field<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<impl Input<'v>>,
    field: Sym<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    match value {
        Some(value) => {
            Output::set(strand, out, value);
            Ok(())
        }
        None => Err(Error::field(strand, field)),
    }
}
