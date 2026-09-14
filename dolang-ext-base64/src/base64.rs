use ::base64::{
    Engine as _,
    alphabet::{self, Alphabet},
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

use dolang::runtime::{
    Error, Output, Result, Strand, Sym, Value, unpack, value::View, vm::Register,
};

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
/// The alphabets differ only in the last two characters, so input containing
/// neither decodes identically under both and is reported as `Standard`.
fn detect_alphabet(bytes: &[u8]) -> std::result::Result<Alpha, &'static str> {
    let url = bytes.iter().any(|b| matches!(b, b'-' | b'_'));
    let standard = bytes.iter().any(|b| matches!(b, b'+' | b'/'));
    match (url, standard) {
        (true, true) => Err("input mixes standard and URL-safe alphabet characters"),
        (true, false) => Ok(Alpha::Url),
        (false, _) => Ok(Alpha::Standard),
    }
}

fn decode_bytes(
    bytes: &[u8],
    alpha: Option<Alpha>,
    pad: Option<bool>,
) -> std::result::Result<Vec<u8>, String> {
    let alpha = match alpha {
        Some(alpha) => alpha,
        None => detect_alphabet(bytes).map_err(str::to_owned)?,
    };
    decode_engine(alpha, pad)
        .decode(bytes)
        .map_err(|e| e.to_string())
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>) {
    let alphabet_sym = builder.sym("alphabet");
    let pad_sym = builder.sym("pad");
    let syms = AlphaSyms {
        standard: builder.sym("STANDARD"),
        url: builder.sym("URL"),
        auto: builder.sym("AUTO"),
    };

    builder
        .module("base64")
        .function("encode", async move |strand, args, out| {
            let ([arg], [alphabet, pad]) =
                unpack!(strand, args, 1, 0, alphabet_sym = None, pad_sym = None)?;
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
            let ([arg], [alphabet, pad]) =
                unpack!(strand, args, 1, 0, alphabet_sym = None, pad_sym = None)?;
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
            .map_err(|e| Error::value(strand, e))?;
            Output::set(strand, out, decoded.as_slice());
            Ok(())
        })
        .commit();
}
