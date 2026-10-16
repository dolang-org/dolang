use crate::global::Tag;

use std::{cell::Cell, collections::HashSet, mem};

use dolang::runtime::{
    Error, Object, Output, State, call,
    error::ErrorKind,
    object::{Mut, Ref, TypeBuilder},
    unpack,
    value::Nil,
    vm::{Builder, Register},
};

use crate::global::Global;

/// Registers the importer that consults `load.import_handler` callbacks.
///
/// Importers can't be added once the VM is entered, so this runs eagerly. Until the `load`
/// module has been set up, no handler can have been registered, so the importer declines.
pub(crate) fn configure_importer<'v>(builder: &mut Builder<'v>) {
    builder.importer(async move |strand, name, mut out| {
        let Some(global) = strand.try_state::<Tag>() else {
            return Err(Error::import(strand, name));
        };
        let _executor = ImportExecutor::new(&global.executors);
        let handlers = global.handlers.as_array(strand).unwrap();
        // Appended handlers participate in the next resolution. Existing
        // indices cannot move until every executor has finished.
        let end = handlers.len(strand)?;
        strand
            .with_slots(async move |strand, [mut key, mut callback]| {
                for index in 0..end {
                    handlers.get(strand, index, &mut key)?;
                    if key.is_nil() {
                        continue;
                    }
                    let handler = global.types.import_handler.cast(&key).unwrap();
                    let result = handler
                        .enter(strand, async |strand, inst| {
                            Output::set(
                                strand,
                                &mut callback,
                                Ref::slot::<0>(&inst.borrow_unwrap()),
                            );
                            let result = call!(strand, &callback, &mut out, name).await;
                            if inst.annex().unregistered.get() {
                                // Unregistration raced this callback. Decline even
                                // success so the VM cannot cache its result.
                                return Err(Error::import(strand, name));
                            }
                            if result.is_ok() {
                                inst.borrow_mut_unwrap().loaded.insert(name.to_owned());
                            }
                            result
                        })
                        .await;
                    match result {
                        Ok(()) => return Ok(()),
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

            let handlers = global.handlers.as_array(strand).unwrap();
            if global.executors.get() == 0 && global.gravestones.get() {
                strand.with_slots_sync(|strand, [mut item]| {
                    for index in (0..handlers.len(strand)?).rev() {
                        handlers.get(strand, index, &mut item)?;
                        if item.is_nil() {
                            handlers.delete(strand, index)?;
                        }
                    }
                    Ok(())
                })?;
                global.gravestones.set(false);
            }
            global.types.import_handler.create_with_annex(
                strand,
                ImportHandler {
                    loaded: HashSet::new(),
                },
                ImportHandlerAnnex {
                    global,
                    unregistered: Cell::new(false),
                },
                &mut out,
            );
            global
                .types
                .import_handler
                .cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, inst| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<0>(&mut inst.borrow_mut_unwrap()),
                        callback,
                    );
                });
            handlers.push(strand, &out)?;

            Ok(())
        })
        .commit();
}

pub(crate) struct ImportHandler {
    loaded: HashSet<String>,
}

pub(crate) struct ImportHandlerAnnex<'v> {
    global: State<'v, Global<'v>>,
    unregistered: Cell<bool>,
}

impl<'v> Object<'v> for ImportHandler {
    const NAME: &'v str = "ImportHandler";
    const MODULE: &'v str = "load";
    const SLOTS: usize = 1;
    type Annex = ImportHandlerAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.method_with_slots(
            "unregister",
            async move |this, strand, args, _out, [mut item]| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                if annex.unregistered.replace(true) {
                    return Ok(());
                }
                let global = annex.global;
                let handlers = global.handlers.as_array(strand).unwrap();
                for index in 0..handlers.len(strand)? {
                    handlers.get(strand, index, &mut item)?;
                    if item.repr_eq(strand, this) {
                        if global.executors.get() == 0 {
                            handlers.delete(strand, index)?;
                        } else {
                            handlers.set(strand, index, Nil)?;
                            global.gravestones.set(true);
                        }
                        break;
                    }
                }
                let loaded = mem::take(&mut this.borrow_mut(strand)?.loaded);
                for name in loaded {
                    strand.vm().evict_import_cache(&name);
                }
                Ok(())
            },
        )
    }
}

struct ImportExecutor<'a>(&'a Cell<usize>);

impl<'a> ImportExecutor<'a> {
    fn new(count: &'a Cell<usize>) -> Self {
        count.set(count.get() + 1);
        Self(count)
    }
}

impl Drop for ImportExecutor<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}
