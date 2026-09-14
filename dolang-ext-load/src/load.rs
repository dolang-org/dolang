use std::{collections::HashSet, mem};

use dolang::runtime::{
    Error, Object, State, call,
    error::ErrorKind,
    method,
    object::TypeBuilder,
    unpack,
    vm::{Builder, Register},
};

use crate::global::Global;

/// Registers the importer that consults `load.import_handler` callbacks.
///
/// Importers can't be added once the VM is entered, so this runs eagerly. Until the `load`
/// module has been set up, no handler can have been registered, so the importer declines.
pub(crate) fn configure_importer<'v>(builder: &mut Builder<'v>) {
    builder.importer(async move |strand, name, mut out| {
        let Some(global) = strand.try_state::<Global<'v>>() else {
            return Err(Error::import(strand, name));
        };
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
                                .cast(&key)
                                .expect("load handler registry key must be an ImportHandler");
                            handler
                                .enter(strand, async |strand, inst| {
                                    inst.borrow_mut(strand)?.loaded.insert(name.to_owned());
                                    Ok(())
                                })
                                .await?;
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
}

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    let importer_sym = builder.sym("importer");
    builder
        .module("load")
        .value("ImportHandler", global.types.import_handler)
        .function("run", async move |strand, args, out| {
            let ([bytecode], [importer]) = unpack!(strand, args, 1, 0, importer_sym = None)?;

            let bytecode = dolang::runtime::Bytecode::new(
                bytecode
                    .as_bin(strand)
                    .ok_or_else(|| Error::type_error(strand, "bytecode: expected Bin"))?
                    .to_vec(),
            );
            match importer {
                Some(importer) => bytecode.run_with_importer(strand, importer, out).await,
                None => bytecode.run(strand, out).await,
            }
        })
        .function("import", async move |strand, args, out| {
            let ([name], []) = unpack!(strand, args, 1, 0)?;
            let name = name
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "name: expected Str"))?;
            let name = strand.access(|access| name.as_str(access).to_owned());
            strand.import(&name, out).await
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
                dict.insert(strand, &out, callback, false)?;
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
