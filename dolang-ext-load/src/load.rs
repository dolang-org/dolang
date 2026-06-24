use std::{collections::HashSet, mem};

use dolang::runtime::{
    Error, Object, State, call, error::ErrorKind, method, object::TypeBuilder, unpack, vm::Builder,
};

use crate::global::Global;

pub(crate) fn configure<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    builder.importer(async move |strand, name, mut out| {
        let dict = global
            .handlers
            .as_dict(strand)
            .expect("load handler registry must be a dict");
        let mut pairs = dict.pairs();

        strand
            .with_slots(async move |strand, [mut key, mut callback]| {
                loop {
                    if !pairs.next(strand, &mut key, &mut callback)? {
                        break;
                    }

                    match call!(strand, &callback, &mut out, name).await {
                        Ok(()) => {
                            let handler = global
                                .types
                                .import_handler
                                .downcast(&key)
                                .expect("load handler registry key must be an ImportHandler");
                            handler.borrow_mut(strand)?.loaded.insert(name.to_owned());
                            return Ok(());
                        }
                        Err(e) if e.kind() == ErrorKind::Import => (),
                        Err(e) => return Err(e),
                    }
                }

                Err(Error::import(strand, name))
            })
            .await
    });

    builder
        .module("load")
        .function("run", async move |strand, args, out| {
            let ([bytecode], []) = unpack!(strand, args, 1, 0)?;

            let bytecode = dolang::runtime::Bytecode::new(
                bytecode
                    .as_bin(strand)
                    .ok_or_else(|| Error::type_error(strand, "bytecode: expected bin"))?
                    .to_vec(),
            );
            bytecode.run(strand, out).await
        })
        .function("import_handler", async move |strand, args, mut out| {
            let ([callback], []) = unpack!(strand, args, 1, 0)?;

            global.types.import_handler.create_with_annex(
                strand,
                ImportHandler {
                    loaded: HashSet::new(),
                },
                ImportHandlerAnnex { global },
                &mut out,
            );

            {
                let dict = global
                    .handlers
                    .as_dict(strand)
                    .expect("load handler registry must be a dict");
                dict.insert(strand, &out, callback)?;
            }

            Ok(())
        })
        .commit();
}

pub(crate) struct ImportHandler {
    loaded: HashSet<String>,
}

pub(crate) struct ImportHandlerAnnex<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for ImportHandler {
    const NAME: &'v str = "ImportHandler";
    const MODULE: &'v str = "load";
    type Annex = ImportHandlerAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let delete = builder.sym("delete");
        builder.method_with_slots(
            "unregister",
            async move |this, strand, args, _out, [tmp]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let loaded = {
                    let mut this = this.borrow_mut(strand)?;
                    mem::take(&mut this.loaded)
                };
                let annex = this.annex();
                method!(strand, &annex.global.handlers, delete, tmp, this).await?;
                for name in loaded {
                    strand.vm().evict_import_cache(&name);
                }
                Ok(())
            },
        )
    }
}
