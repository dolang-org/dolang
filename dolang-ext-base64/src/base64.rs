use ::base64::{Engine as _, engine::general_purpose::STANDARD};

use dolang::runtime::{Output, error::Error, unpack, value::View, vm::Builder};

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder
        .module("base64")
        .function("encode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let encoded = match arg.view(strand.vm()) {
                View::Str(str) => strand.access(|access| STANDARD.encode(str.as_str(access))),
                View::Bin(bin) => strand.access(|access| STANDARD.encode(bin.as_slice(access))),
                _ => return Err(Error::type_error(strand, "expected str or bin")),
            };
            Output::set(strand, out, encoded.as_str());
            Ok(())
        })
        .function("decode", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            let decoded = match arg.view(strand.vm()) {
                View::Str(str) => strand
                    .access(|access| STANDARD.decode(str.as_str(access)))
                    .map_err(|e| Error::value(strand, e.to_string()))?,
                _ => return Err(Error::type_error(strand, "expected str")),
            };
            Output::set(strand, out, decoded.as_slice());
            Ok(())
        })
        .commit();
}
