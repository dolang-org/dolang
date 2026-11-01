use crate::global::Tag;

use std::{cell::Cell, collections::HashSet, mem};

use dolang::runtime::{
    Error, Input, Instance, Object, Output, Result, State, Strand, Value, call,
    error::ErrorKind,
    object::{Mut, Ref, TypeBuilder, fmt},
    unpack,
    value::{Nil, TypeObject, fmt::Format},
    vm::{self, Builder, Register},
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
        .value("Bytecode", global.types.bytecode)
        .value("BytecodeError", global.types.bytecode_error)
        .value("ImportHandler", global.types.import_handler)
        .function("load", async move |strand, args, mut out| {
            let ([bytes], []) = unpack!(strand, args, 1, 0)?;
            let bytecode = decode(strand, &bytes)?;
            global.types.bytecode.create_with_annex(
                strand,
                Bytecode,
                BytecodeAnnex(Cell::new(Some(bytecode))),
                &mut out,
            );
            Ok(())
        })
        .function("run", async move |strand, args, out| {
            let ([bytecode], [importer]) = unpack!(strand, args, 1, 0, importer_sym = None)?;
            let bytecode = match global.types.bytecode.cast(&bytecode) {
                Some(cast) => cast.enter_sync(strand, |strand, inst| take(strand, inst))?,
                None => decode(strand, &bytecode)?,
            };
            run(strand, bytecode, importer, out).await
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

/// Decodes and verifies `bytes`, raising `load.BytecodeError` if they are invalid
fn decode<'v, 's>(strand: &mut Strand<'v, 's>, bytes: &Value<'v>) -> Result<'v, 's, vm::Bytecode> {
    let bytes = bytes
        .as_bin(strand)
        .ok_or_else(|| Error::type_error(strand, "bytecode: expected Bin or Bytecode"))?
        .to_vec();
    vm::Bytecode::new(bytes).map_err(|e| {
        let global = strand.state::<Tag>();
        Error::object_with_annex(
            strand,
            global.types.bytecode_error,
            BytecodeError,
            ErrorAnnex {
                message: e.to_string(),
            },
        )
    })
}

fn take<'v, 's>(
    strand: &mut Strand<'v, 's>,
    inst: Instance<'v, '_, Bytecode>,
) -> Result<'v, 's, vm::Bytecode> {
    inst.annex()
        .0
        .take()
        .ok_or_else(|| Error::state_error(strand, "bytecode already run"))
}

async fn run<'v, 's>(
    strand: &mut Strand<'v, 's>,
    bytecode: vm::Bytecode,
    importer: Option<impl Input<'v>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    match importer {
        Some(importer) => bytecode.run_with_importer(strand, importer, out).await,
        None => bytecode.run(strand, out).await,
    }
}

/// Decoded bytecode, emptied once run
pub(crate) struct Bytecode;

pub(crate) struct BytecodeAnnex(Cell<Option<vm::Bytecode>>);

impl<'v> Object<'v> for Bytecode {
    const NAME: &'v str = "Bytecode";
    const MODULE: &'v str = "load";
    type Annex = BytecodeAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let importer_sym = builder.sym("importer");
        builder.method("run", async move |this, strand, args, out| {
            let ([], [importer]) = unpack!(strand, args, 0, 0, importer_sym = None)?;
            let bytecode = take(strand, this)?;
            run(strand, bytecode, importer, out).await
        })
    }
}

pub(crate) struct BytecodeError;

pub(crate) struct ErrorAnnex {
    message: String,
}

impl<'v> Object<'v> for BytecodeError {
    const NAME: &'v str = "BytecodeError";
    const MODULE: &'v str = "load";
    type Annex = ErrorAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.nominal_supertype(TypeObject::RuntimeError)
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
        fmt!(strand, w, "<load.BytecodeError ")?;
        Self::display(this, strand, w)?;
        fmt!(strand, w, ">")
    }
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
