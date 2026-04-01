use ::base64::{Engine as _, engine::general_purpose::STANDARD};

use dolang::runtime::{
    Output,
    error::{Error, ResultExt},
    unpack,
    vm::Builder,
};

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder
        .module("base64")
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let bytes = arg
                .as_u8_slice(strand)
                .ok_or_else(|| Error::type_error(strand, "expected str or bin"))?;
            let encoded = STANDARD.encode(bytes);
            Output::set(strand, out, encoded.as_str());
            Ok(())
        })
        .function("decode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let encoded = arg
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "expected str"))?;
            let decoded = STANDARD.decode(encoded).into_do(strand)?;
            Output::set(strand, out, decoded.as_slice());
            Ok(())
        })
        .commit();
}
