use dolang::runtime::{Arg, Args, Error, Result, Strand, Value, value::View};

/// The bytes of a `Str` or `Bin`, as any console's `write` accepts.
pub(crate) fn data_bytes<'v, 's>(
    strand: &mut Strand<'v, 's>,
    data: &Value<'v>,
) -> Result<'v, 's, Vec<u8>> {
    match data.view(strand) {
        View::Str(value) => Ok(value.pin().as_bytes().to_vec()),
        View::Bin(value) => Ok(value.pin().to_vec()),
        _ => Err(Error::type_error(strand, "expected `Str` or `Bin`")),
    }
}

/// The arguments of a `Console.write` call, concatenated.
pub(crate) fn write_data<'v, 's>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, '_>,
) -> Result<'v, 's, Vec<u8>> {
    let mut bytes = Vec::new();
    for arg in args {
        match arg {
            Arg::Pos(value) => bytes.extend(data_bytes(strand, &value)?),
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        }
    }
    Ok(bytes)
}

pub(crate) fn strip_line_ending(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
}
