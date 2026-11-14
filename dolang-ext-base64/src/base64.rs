use ::base64::{
    Engine as _,
    alphabet::{self, Alphabet},
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

use dolang::runtime::{
    Instance, Object, Output, Result, State, Strand, Sym, Value,
    error::Error,
    object::{TypeBuilder, fmt},
    unpack,
    value::{Nil, TypeObject, View, fmt::Format},
    vm::Register,
};

use crate::global::{Global, Tag};

/// `base64.DecodeError`: the input is not valid base64.
pub(crate) struct DecodeError;

/// What is wrong with the input.
#[derive(Clone, Copy)]
enum DecodeKind {
    /// A byte outside the alphabet, or padding before the end.
    Byte,
    /// The last group has a single character.
    Length,
    /// The last character has bits that decoding would discard.
    LastSymbol,
    /// Padding does not match `pad:`.
    Padding,
}

pub(crate) struct DecodeAnnex {
    message: String,
    kind: DecodeKind,
    /// Byte offset in the input, where the crate reports one.
    offset: Option<usize>,
}

/// Base64 alphabet (RFC 4648)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Alpha {
    /// Section 4: `+` and `/` for the last two characters
    Standard,
    /// Section 5: `-` and `_` for the last two characters
    Url,
}

impl Alpha {
    fn alphabet(self) -> &'static Alphabet {
        match self {
            Alpha::Standard => &alphabet::STANDARD,
            Alpha::Url => &alphabet::URL_SAFE,
        }
    }
}

fn encode_engine(alpha: Alpha, pad: bool) -> GeneralPurpose {
    GeneralPurpose::new(
        alpha.alphabet(),
        GeneralPurposeConfig::new().with_encode_padding(pad),
    )
}

fn decode_engine(alpha: Alpha, pad: Option<bool>) -> GeneralPurpose {
    let mode = match pad {
        None => DecodePaddingMode::Indifferent,
        Some(true) => DecodePaddingMode::RequireCanonical,
        Some(false) => DecodePaddingMode::RequireNone,
    };
    GeneralPurpose::new(
        alpha.alphabet(),
        GeneralPurposeConfig::new().with_decode_padding_mode(mode),
    )
}

/// Symbols naming the alphabet choices
#[derive(Clone, Copy)]
struct AlphaSyms<'v> {
    standard: Sym<'v, 'v>,
    url: Sym<'v, 'v>,
    auto: Sym<'v, 'v>,
}

/// Resolve the `alphabet:` keyword argument.
///
/// Returns [`None`] for `:AUTO:`, which is only accepted when `allow_auto` is set.
fn alphabet_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
    syms: AlphaSyms<'v>,
    allow_auto: bool,
) -> Result<'v, 's, Option<Alpha>> {
    let Some(value) = value else {
        return Ok(if allow_auto {
            None
        } else {
            Some(Alpha::Standard)
        });
    };
    match value.view(strand.vm()) {
        View::Sym(sym) if sym == syms.standard => Ok(Some(Alpha::Standard)),
        View::Sym(sym) if sym == syms.url => Ok(Some(Alpha::Url)),
        View::Sym(sym) if allow_auto && sym == syms.auto => Ok(None),
        View::Sym(_) => Err(Error::value(
            strand,
            if allow_auto {
                "alphabet: expected :STANDARD:, :URL:, or :AUTO:"
            } else {
                "alphabet: expected :STANDARD: or :URL:"
            },
        )),
        _ => Err(Error::type_error(strand, "alphabet: expected Sym")),
    }
}

/// Resolve the `pad:` keyword argument.
fn pad_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<&Value<'v>>,
) -> Result<'v, 's, Option<bool>> {
    value
        .map(|value| {
            value
                .as_bool(strand)
                .ok_or_else(|| Error::type_error(strand, "pad: expected Bool"))
        })
        .transpose()
}

/// Determine which alphabet an encoded input uses.
///
/// The alphabets differ only in the last two characters, so the first of those
/// decides; the engine then reports a character of the other alphabet as
/// invalid. Input containing neither decodes identically under both.
fn detect_alphabet(bytes: &[u8]) -> Alpha {
    match bytes
        .iter()
        .find(|b| matches!(b, b'-' | b'_' | b'+' | b'/'))
    {
        Some(b'-' | b'_') => Alpha::Url,
        _ => Alpha::Standard,
    }
}

fn decode_bytes(
    bytes: &[u8],
    alpha: Option<Alpha>,
    pad: Option<bool>,
) -> std::result::Result<Vec<u8>, ::base64::DecodeError> {
    let alpha = alpha.unwrap_or_else(|| detect_alphabet(bytes));
    decode_engine(alpha, pad).decode(bytes)
}

/// Shows an input byte as a quoted character, or in hex if not printable.
fn show_byte(byte: u8) -> String {
    if byte.is_ascii_graphic() || byte == b' ' {
        format!("'{}'", char::from(byte))
    } else {
        format!("0x{byte:02x}")
    }
}

/// Raises `base64.DecodeError` for `error`.
fn decode_error<'v, 's>(
    strand: &mut Strand<'v, 's>,
    error: ::base64::DecodeError,
    pad: Option<bool>,
) -> Error<'v, 's> {
    use ::base64::DecodeError as E;
    let (kind, offset, message) = match error {
        E::InvalidByte(offset, byte) => (
            DecodeKind::Byte,
            Some(offset),
            format!(
                "invalid base64 character {} at offset {offset}",
                show_byte(byte)
            ),
        ),
        E::InvalidLastSymbol(offset, byte) => (
            DecodeKind::LastSymbol,
            Some(offset),
            format!(
                "last base64 character {} at offset {offset} has nonzero unused bits",
                show_byte(byte)
            ),
        ),
        E::InvalidLength(_) => (
            DecodeKind::Length,
            None,
            "invalid base64 length: the last group has a single character".to_owned(),
        ),
        E::InvalidPadding => (
            DecodeKind::Padding,
            None,
            match pad {
                Some(false) => "base64 padding is present with pad: false",
                _ => "base64 padding is missing or incomplete with pad: true",
            }
            .to_owned(),
        ),
    };
    let global = strand.state::<Tag>();
    Error::object_with_annex(
        strand,
        global.types.decode_error,
        DecodeError,
        DecodeAnnex {
            message,
            kind,
            offset,
        },
    )
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    let alphabet = builder.sym("alphabet");
    let pad = builder.sym("pad");
    let syms = AlphaSyms {
        standard: builder.sym("STANDARD"),
        url: builder.sym("URL"),
        auto: builder.sym("AUTO"),
    };

    builder
        .module("base64")
        .value("DecodeError", global.types.decode_error)
        .function("encode", async move |strand, args, out| {
            unpack!(strand, args => arg, ?:alphabet, ?:pad);
            let alpha = alphabet_arg(strand, alphabet.as_deref(), syms, false)?.expect("alphabet");
            // URL-safe base64 is conventionally unpadded (e.g. RFC 7515)
            let pad = pad_arg(strand, pad.as_deref())?.unwrap_or(alpha == Alpha::Standard);
            let engine = encode_engine(alpha, pad);
            let encoded = match arg.view(strand.vm()) {
                View::Str(str) => strand.access(|access| engine.encode(str.as_str(access))),
                View::Bin(bin) => strand.access(|access| engine.encode(bin.as_slice(access))),
                _ => return Err(Error::type_error(strand, "expected Str or Bin")),
            };
            Output::set(strand, out, encoded.as_str());
            Ok(())
        })
        .function("decode", async move |strand, args, out| {
            unpack!(strand, args => arg, ?:alphabet, ?:pad);
            let alpha = alphabet_arg(strand, alphabet.as_deref(), syms, true)?;
            let pad = pad_arg(strand, pad.as_deref())?;
            let decoded = match arg.view(strand.vm()) {
                View::Str(str) => {
                    strand.access(|access| decode_bytes(str.as_str(access).as_bytes(), alpha, pad))
                }
                View::Bin(bin) => {
                    strand.access(|access| decode_bytes(bin.as_slice(access), alpha, pad))
                }
                _ => return Err(Error::type_error(strand, "expected Str or Bin")),
            }
            .map_err(|e| decode_error(strand, e, pad))?;
            Output::set(strand, out, decoded.as_slice());
            Ok(())
        })
        .commit();
}

impl<'v> Object<'v> for DecodeError {
    const NAME: &'v str = "DecodeError";
    const MODULE: &'v str = "base64";
    type Annex = DecodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let byte = builder.sym("BYTE");
        let length = builder.sym("LENGTH");
        let last_symbol = builder.sym("LAST_SYMBOL");
        let padding = builder.sym("PADDING");
        builder
            .nominal_supertype(TypeObject::DataError)
            .get("kind", move |this, strand, out| {
                let kind = match this.annex().kind {
                    DecodeKind::Byte => byte,
                    DecodeKind::Length => length,
                    DecodeKind::LastSymbol => last_symbol,
                    DecodeKind::Padding => padding,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .get("offset", |this, strand, out| {
                match this.annex().offset {
                    Some(offset) => Output::set(strand, out, offset),
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
    }

    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "{}", this.annex().message)
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn Format<'v>,
    ) -> Result<'v, 's, ()> {
        fmt!(strand, w, "<base64.DecodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
}
