use dolang::runtime::{Bytecode, Error, unpack, vm::Builder};

pub(crate) fn configure<'v>(builder: &mut Builder<'v>) {
    builder
        .module("load")
        .function("run", async move |strand, args, out| {
            let ([bytecode], []) = unpack!(strand, args, 1, 0)?;

            let bytecode = Bytecode::new(
                bytecode
                    .as_bin(strand)
                    .ok_or_else(|| Error::type_error(strand, "bytecode: expected bin"))?
                    .to_owned(),
            );
            bytecode.run(strand, out).await
        })
        .commit();
}
