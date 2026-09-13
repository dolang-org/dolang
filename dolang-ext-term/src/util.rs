use dolang::runtime::{Arg, Args, Error, Result, Strand, Value, value::View};

use crate::global::Global;

/// How a [`SinkConsole`](crate::console::SinkConsole) quantizes the bytes
/// written to it into values.
///
/// Framing only: both modes are lossless, so concatenating the values
/// reproduces the bytes written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    /// One `Str` per line, terminator included. A final partial line simply
    /// has none.
    Line,
    /// Arbitrary `Bin` chunks, at whatever boundaries the writes fall on.
    Chunk,
}

/// Decodes a `:LINE:`/`:CHUNK:` framing argument, defaulting to line framing.
pub(crate) fn parse_mode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mode: Option<&Value<'v>>,
) -> Result<'v, 's, Framing> {
    let global = strand.state::<Global<'v>>();
    match mode {
        None => Ok(Framing::Line),
        Some(value) => match value.as_sym(strand) {
            Some(sym) if sym == global.syms.line => Ok(Framing::Line),
            Some(sym) if sym == global.syms.chunk => Ok(Framing::Chunk),
            _ => Err(Error::value(strand, "mode must be :LINE: or :CHUNK:")),
        },
    }
}

/// Encodes a value as the bytes to write for it, adding nothing.
///
/// A `Str` or `Bin` contributes its own bytes and nothing else; anything else
/// is stringified, so `put 42` writes `42` the way `echo` would.
pub(crate) fn encode_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Vec<u8>> {
    Ok(match value.view(strand) {
        View::Str(value) => value.pin().as_bytes().to_vec(),
        View::Bin(value) => value.pin().to_vec(),
        _ => value.to_string(strand)?.into_bytes(),
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_exactly_one_complete_line_ending() {
        assert_eq!(strip_line_ending("text\r\n"), "text");
        assert_eq!(strip_line_ending("text\n"), "text");
        assert_eq!(strip_line_ending("text\r"), "text\r");
        assert_eq!(strip_line_ending("text\n\n"), "text\n");
        assert_eq!(strip_line_ending("text\r\r\n"), "text\r");
    }
}
