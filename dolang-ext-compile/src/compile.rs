use std::{
    convert::Infallible,
    hash::{Hash, Hasher},
    ops::ControlFlow,
    path::Path,
};

use dolang::{
    compile::{self, Compiler, Diag, ErrorKind, Mode},
    extension::CompilerExt,
    runtime::{
        Error, Input, Instance, Object, Output, Result, Slot, State, Strand, Sym, Type, Value,
        object::{Mut, Ref, TypeBuilder},
        unpack,
        value::{Array, Dict, Empty, Nil, View},
        vm::{Builder, Stateful},
    },
};

pub(crate) struct Types<'v> {
    result: Type<'v, ResultObject>,
    diagnostic: Type<'v, Diagnostic>,
    span: Type<'v, Span>,
    pos: Type<'v, Pos>,
    annotation: Type<'v, Annotation>,
    note: Type<'v, Note>,
    patch: Type<'v, Patch>,
}

pub(crate) struct Syms<'v> {
    error: Sym<'v, 'v>,
    warning: Sym<'v, 'v>,
    primary: Sym<'v, 'v>,
    context: Sym<'v, 'v>,
    info: Sym<'v, 'v>,
    help: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    types: Types<'v>,
    syms: Syms<'v>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        Self {
            types: Types {
                result: builder.register_type(),
                diagnostic: builder.register_type(),
                span: builder.register_type(),
                pos: builder.register_type(),
                annotation: builder.register_type(),
                note: builder.register_type(),
                patch: builder.register_type(),
            },
            syms: Syms {
                error: builder.sym("error"),
                warning: builder.sym("warning"),
                primary: builder.sym("primary"),
                context: builder.sym("context"),
                info: builder.sym("info"),
                help: builder.sym("help"),
            },
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PosData {
    byte_offset: usize,
    line: u32,
    column: u32,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SpanData {
    start: PosData,
    end: PosData,
}

pub(crate) struct ResultAnnex {
    bytecode: Option<Vec<u8>>,
}

pub(crate) struct DiagnosticAnnex<'v> {
    global: State<'v, Global<'v>>,
    diag: Diag,
    path: String,
}

#[derive(Clone)]
pub(crate) struct SpanAnnex<'v> {
    global: State<'v, Global<'v>>,
    span: SpanData,
}

#[derive(Clone)]
pub(crate) struct PosAnnex {
    pos: PosData,
}

pub(crate) struct AnnotationAnnex<'v> {
    global: State<'v, Global<'v>>,
    kind: compile::AnnotationKind,
    span: SpanData,
    message: String,
}

pub(crate) struct NoteAnnex<'v> {
    global: State<'v, Global<'v>>,
    kind: compile::NoteKind,
    message: String,
}

pub(crate) struct PatchAnnex<'v> {
    global: State<'v, Global<'v>>,
    span: SpanData,
    message: String,
    sub: String,
}

pub(crate) struct ResultObject;
pub(crate) struct Diagnostic;
pub(crate) struct Span;
pub(crate) struct Pos;
pub(crate) struct Annotation;
pub(crate) struct Note;
pub(crate) struct Patch;

const RESULT_DIAGNOSTICS: usize = 0;
const RESULT_SOURCE: usize = 1;

const DIAG_ANNOTATIONS: usize = 0;
const DIAG_NOTES: usize = 1;
const DIAG_PATCHES: usize = 2;
const DIAG_SOURCE: usize = 3;

fn pos_data(pos: compile::Pos) -> PosData {
    PosData {
        byte_offset: pos.byte_offset(),
        line: pos.line_offset(),
        column: pos.column_offset(),
    }
}

fn span_data(span: compile::Span) -> SpanData {
    SpanData {
        start: pos_data(span.start()),
        end: pos_data(span.end()),
    }
}

fn severity<'v>(global: State<'v, Global<'v>>, severity: compile::Severity) -> Sym<'v, 'v> {
    match severity {
        compile::Severity::Error => global.syms.error,
        compile::Severity::Warning => global.syms.warning,
        _ => global.syms.warning,
    }
}

pub fn extract_diagnostic<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, (Diag, String)> {
    let global = strand.state::<Global<'v>>();
    let Some(diag) = global.types.diagnostic.downcast(value) else {
        return Err(Error::type_error(strand, "expected compile.Diagnostic"));
    };
    let borrow = diag.borrow(strand)?;
    Output::set(strand, out, Ref::slot::<DIAG_SOURCE>(&borrow));
    let annex = diag.annex();
    Ok((annex.diag.clone(), annex.path.clone()))
}

fn create_pos<'v>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    pos: PosData,
    out: Slot<'v, '_>,
) {
    global
        .types
        .pos
        .create_with_annex(strand, Pos, PosAnnex { pos }, out);
}

fn create_span<'v>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    span: SpanData,
    out: Slot<'v, '_>,
) {
    global
        .types
        .span
        .create_with_annex(strand, Span, SpanAnnex { global, span }, out);
}

fn create_annotation<'v>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    annotation: compile::Annotation,
    out: Slot<'v, '_>,
) {
    global.types.annotation.create_with_annex(
        strand,
        Annotation,
        AnnotationAnnex {
            global,
            kind: annotation.kind(),
            span: span_data(annotation.span()),
            message: annotation.message().to_string(),
        },
        out,
    );
}

fn create_note<'v>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    note: compile::Note,
    out: Slot<'v, '_>,
) {
    global.types.note.create_with_annex(
        strand,
        Note,
        NoteAnnex {
            global,
            kind: note.kind(),
            message: note.message().to_string(),
        },
        out,
    );
}

fn create_patch<'v>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    patch: compile::Patch,
    out: Slot<'v, '_>,
) {
    global.types.patch.create_with_annex(
        strand,
        Patch,
        PatchAnnex {
            global,
            span: span_data(patch.span()),
            message: patch.message().to_owned(),
            sub: patch.sub().to_owned(),
        },
        out,
    );
}

fn create_diagnostic<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, 's>,
    path: &str,
    source: &Value<'v>,
    diag: Diag,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    global.types.diagnostic.create_with_annex(
        strand,
        Diagnostic,
        DiagnosticAnnex {
            global,
            diag,
            path: path.to_owned(),
        },
        &mut *out,
    );

    {
        let mut borrow = global
            .types
            .diagnostic
            .downcast(&*out)
            .unwrap()
            .borrow_mut_unwrap();
        Output::set(
            strand,
            Mut::slot_mut::<DIAG_ANNOTATIONS>(&mut borrow),
            Empty::Array,
        );
        Output::set(
            strand,
            Mut::slot_mut::<DIAG_NOTES>(&mut borrow),
            Empty::Array,
        );
        Output::set(
            strand,
            Mut::slot_mut::<DIAG_PATCHES>(&mut borrow),
            Empty::Array,
        );
        Output::set(strand, Mut::slot_mut::<DIAG_SOURCE>(&mut borrow), source);
    }

    let inst = global.types.diagnostic.downcast(&*out).unwrap();
    let borrow = inst.borrow(strand)?;
    let annotations = Ref::slot::<DIAG_ANNOTATIONS>(&borrow)
        .as_array(strand)
        .unwrap();
    let notes = Ref::slot::<DIAG_NOTES>(&borrow).as_array(strand).unwrap();
    let patches = Ref::slot::<DIAG_PATCHES>(&borrow).as_array(strand).unwrap();

    strand.with_slots_sync(|strand, [mut item]| {
        for annotation in inst.annex().diag.annotations() {
            create_annotation(global, strand, annotation, Slot::reborrow(&mut item));
            annotations.push(strand, &mut item)?;
        }
        for note in inst.annex().diag.notes() {
            create_note(global, strand, note, Slot::reborrow(&mut item));
            notes.push(strand, &mut item)?;
        }
        for patch in inst.annex().diag.patches() {
            create_patch(global, strand, patch, Slot::reborrow(&mut item));
            patches.push(strand, &mut item)?;
        }
        Ok(())
    })
}

fn create_result<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, 's>,
    path: &str,
    source: impl Input<'v>,
    bytecode: Option<Vec<u8>>,
    diagnostics: Vec<Diag>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    global.types.result.create_with_annex(
        strand,
        ResultObject,
        ResultAnnex { bytecode },
        &mut *out,
    );

    {
        let mut borrow = global
            .types
            .result
            .downcast(&*out)
            .unwrap()
            .borrow_mut_unwrap();
        Output::set(
            strand,
            Mut::slot_mut::<RESULT_DIAGNOSTICS>(&mut borrow),
            Empty::Array,
        );
        Output::set(strand, Mut::slot_mut::<RESULT_SOURCE>(&mut borrow), source);
    }

    let inst = global.types.result.downcast(&*out).unwrap();
    let borrow = inst.borrow(strand)?;
    let diagnostics_out = Ref::slot::<RESULT_DIAGNOSTICS>(&borrow)
        .as_array(strand)
        .unwrap();
    let source = Ref::slot::<RESULT_SOURCE>(&borrow);

    strand.with_slots_sync(|strand, [mut item]| {
        for diag in diagnostics {
            create_diagnostic(global, strand, path, source, diag, &mut item)?;
            diagnostics_out.push(strand, &mut item)?;
        }
        Ok(())
    })
}

fn apply_prelude_module_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    compiler: &mut Compiler,
    module_name: &str,
    arr: &Array<'v, '_>,
) -> std::result::Result<(), Error<'v, 's>> {
    let vm = strand.vm();
    let len = arr.len(strand)?;

    for i in 0..len {
        let item_name = strand.with_slots_sync(
            |strand, [mut elem]| -> std::result::Result<_, Error<'v, 's>> {
                arr.get(strand, i, &mut elem)?;
                match elem.view(vm) {
                    View::Str(s) => Ok(s.into()),
                    View::Sym(sym) => Ok(sym.as_str(vm).to_owned()),
                    _ => Err(Error::type_error(strand, "prelude item must be str or sym")),
                }
            },
        )?;
        let _prelude = compiler
            .prelude()
            .import_items(module_name)
            .item(item_name)
            .commit();
    }
    Ok(())
}

fn apply_prelude_dict_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    compiler: &mut Compiler,
    module_name: &str,
    dict: &Dict<'v, '_>,
) -> std::result::Result<(), Error<'v, 's>> {
    let vm = strand.vm();
    let mut pairs = dict.pairs();
    let mut items_builder = Some(compiler.prelude().import_items(module_name));

    strand.with_slots_sync(
        |strand, [mut k, mut v]| -> std::result::Result<_, Error<'v, 's>> {
            loop {
                let has_next = pairs.next(strand, &mut k, &mut v)?;
                if !has_next {
                    break;
                }

                let item_name = match k.view(vm) {
                    View::Str(s) => s.into(),
                    View::Sym(sym) => sym.as_str(vm).to_owned(),
                    _ => {
                        return Err(Error::type_error(
                            strand,
                            "prelude item key must be str or sym",
                        ));
                    }
                };
                let bind_name = match v.view(vm) {
                    View::Str(s) => s.into(),
                    View::Sym(sym) => sym.as_str(vm).to_owned(),
                    _ => {
                        return Err(Error::type_error(
                            strand,
                            "prelude item binding must be str or sym",
                        ));
                    }
                };
                items_builder = Some(
                    items_builder
                        .take()
                        .unwrap()
                        .item_with_name(&item_name, &bind_name),
                );
            }
            Ok(())
        },
    )?;

    if let Some(builder) = items_builder {
        let _prelude = builder.commit();
    }
    Ok(())
}

fn apply_prelude_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    compiler: &mut Compiler,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut elem, mut k, mut v]| {
        match value.view(strand.vm()) {
            View::Str(module) => {
                strand.access(|access| compiler.prelude().import_module(module.as_str(access)));
            }
            View::Sym(sym) => {
                compiler.prelude().import_module(sym.as_str(strand));
            }
            View::Array(arr) => {
                let len = arr.len(strand)?;
                for i in 0..len {
                    arr.get(strand, i, &mut elem)?;
                    let name = match elem.view(strand) {
                        View::Str(s) => s.into(),
                        View::Sym(sym) => sym.as_str(strand).to_owned(),
                        _ => {
                            return Err(Error::type_error(
                                strand,
                                "prelude array item must be str or sym",
                            ));
                        }
                    };
                    compiler.prelude().import_module(name);
                }
            }
            View::Dict(dict) => {
                let mut pairs = dict.pairs();
                loop {
                    let has_next = pairs.next(strand, &mut k, &mut v)?;
                    if !has_next {
                        break;
                    }

                    let module = match k.view(strand) {
                        View::Str(s) => s.into(),
                        View::Sym(sym) => sym.as_str(strand).to_owned(),
                        _ => {
                            return Err(Error::type_error(
                                strand,
                                "prelude module key must be str or sym",
                            ));
                        }
                    };

                    match v.view(strand.vm()) {
                        View::Str(bind) => {
                            strand.access(|access| {
                                compiler
                                    .prelude()
                                    .import_module_with_name(&module, bind.as_str(access));
                            });
                        }
                        View::Sym(bind) => {
                            compiler
                                .prelude()
                                .import_module_with_name(&module, bind.as_str(strand));
                        }
                        View::Array(arr) => {
                            apply_prelude_module_items(strand, compiler, &module, &arr)?;
                        }
                        View::Dict(dict) => {
                            apply_prelude_dict_items(strand, compiler, &module, &dict)?;
                        }
                        _ => {
                            return Err(Error::type_error(
                                strand,
                                "prelude module value must be str, sym, array, or dict",
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(Error::type_error(
                    strand,
                    "prelude must be a module name, array, or dict",
                ));
            }
        }

        Ok(())
    })
}

impl<'v> Object<'v> for ResultObject {
    const NAME: &'v str = "Result";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 2;
    type Annex = ResultAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("bytecode", |this, strand, out| {
                if let Some(bytecode) = &this.annex().bytecode {
                    Output::set(strand, out, bytecode.as_slice());
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("diagnostics", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<RESULT_DIAGNOSTICS>(&borrow));
                Ok(())
            })
            .get("ok", |this, strand, out| {
                Output::set(strand, out, this.annex().bytecode.is_some());
                Ok(())
            })
    }
}

impl<'v> Object<'v> for Diagnostic {
    const NAME: &'v str = "Diagnostic";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 4;
    type Annex = DiagnosticAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("severity", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    severity(this.annex().global, this.annex().diag.severity()),
                );
                Ok(())
            })
            .get("message", |this, strand, out| {
                let message = this.annex().diag.message().to_string();
                Output::set(strand, out, message.as_str());
                Ok(())
            })
            .get("span", |this, strand, out| {
                create_span(
                    this.annex().global,
                    strand,
                    span_data(this.annex().diag.span()),
                    out,
                );
                Ok(())
            })
            .get("annotations", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<DIAG_ANNOTATIONS>(&borrow));
                Ok(())
            })
            .get("notes", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<DIAG_NOTES>(&borrow));
                Ok(())
            })
            .get("patches", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<DIAG_PATCHES>(&borrow));
                Ok(())
            })
    }
}

impl<'v> Object<'v> for Span {
    const NAME: &'v str = "Span";
    const MODULE: &'v str = "compile";
    type Annex = SpanAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("start", |this, strand, out| {
                create_pos(
                    this.annex().global,
                    strand,
                    this.annex().span.start.clone(),
                    out,
                );
                Ok(())
            })
            .get("end", |this, strand, out| {
                create_pos(
                    this.annex().global,
                    strand,
                    this.annex().span.end.clone(),
                    out,
                );
                Ok(())
            })
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.span.downcast(other) {
            Ok(this.annex().span == other.annex().span)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().span.hash(hasher);
        Ok(())
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.span.downcast(other) {
            Ok(this.annex().span < other.annex().span)
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

impl<'v> Object<'v> for Pos {
    const NAME: &'v str = "Pos";
    const MODULE: &'v str = "compile";
    type Annex = PosAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("byte_offset", |this, strand, out| {
                Output::set(strand, out, this.annex().pos.byte_offset);
                Ok(())
            })
            .get("line", |this, strand, out| {
                Output::set(strand, out, this.annex().pos.line);
                Ok(())
            })
            .get("column", |this, strand, out| {
                Output::set(strand, out, this.annex().pos.column);
                Ok(())
            })
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = strand.state::<Global<'v>>().types.pos.downcast(other) {
            Ok(this.annex().pos == other.annex().pos)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().pos.hash(hasher);
        Ok(())
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = strand.state::<Global<'v>>().types.pos.downcast(other) {
            Ok(this.annex().pos < other.annex().pos)
        } else {
            Err(Error::not_supported(strand))
        }
    }
}

impl<'v> Object<'v> for Annotation {
    const NAME: &'v str = "Annotation";
    const MODULE: &'v str = "compile";
    type Annex = AnnotationAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("kind", |this, strand, out| {
                let global = this.annex().global;
                let kind = match &this.annex().kind {
                    compile::AnnotationKind::Primary => global.syms.primary,
                    compile::AnnotationKind::Context => global.syms.context,
                    _ => global.syms.context,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .get("span", |this, strand, out| {
                create_span(this.annex().global, strand, this.annex().span.clone(), out);
                Ok(())
            })
            .get("message", |this, strand, out| {
                Output::set(strand, out, this.annex().message.as_str());
                Ok(())
            })
    }
}

impl<'v> Object<'v> for Note {
    const NAME: &'v str = "Note";
    const MODULE: &'v str = "compile";
    type Annex = NoteAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("kind", |this, strand, out| {
                let kind = match &this.annex().kind {
                    compile::NoteKind::Info => this.annex().global.syms.info,
                    compile::NoteKind::Help => this.annex().global.syms.help,
                    _ => this.annex().global.syms.info,
                };
                Output::set(strand, out, kind);
                Ok(())
            })
            .get("message", |this, strand, out| {
                Output::set(strand, out, this.annex().message.as_str());
                Ok(())
            })
    }
}

impl<'v> Object<'v> for Patch {
    const NAME: &'v str = "Patch";
    const MODULE: &'v str = "compile";
    type Annex = PatchAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("span", |this, strand, out| {
                create_span(this.annex().global, strand, this.annex().span.clone(), out);
                Ok(())
            })
            .get("message", |this, strand, out| {
                Output::set(strand, out, this.annex().message.as_str());
                Ok(())
            })
            .get("sub", |this, strand, out| {
                Output::set(strand, out, this.annex().sub.as_str());
                Ok(())
            })
    }
}

pub(crate) fn configure<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let module = builder.sym("module");
    let prelude = builder.sym("prelude");

    builder
        .module("compile")
        .value("Result", global.types.result)
        .value("Diagnostic", global.types.diagnostic)
        .value("Span", global.types.span)
        .value("Pos", global.types.pos)
        .value("Annotation", global.types.annotation)
        .value("Note", global.types.note)
        .value("Patch", global.types.patch)
        .function("compile", async move |strand, args, mut out| {
            let ([path, source], [module, prelude]) =
                unpack!(strand, args, 2, 0, module = None, prelude = None)?;

            let module = module
                .as_ref()
                .map(|m| {
                    m.as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "module: expected `str`"))
                        .map(|m| m.to_string())
                })
                .transpose()?;

            let source_vec = match source.view(strand) {
                View::Str(s) => s.to_string().into(),
                View::Bin(b) => b.to_vec(),
                _ => return Err(Error::type_error(strand, "source: expected `str` or `bin`")),
            };

            let path = path.to_string(strand)?;
            let mut compiler = Compiler::new(Path::new(&path), &source_vec);
            compiler.mode(if let Some(module) = &module {
                Mode::Module { name: module }
            } else {
                Mode::Script
            });

            if let Some(prelude) = prelude {
                apply_prelude_value(strand, &mut compiler, &prelude)?;
            }

            for ext in compiler.extensions() {
                ext.apply(&mut compiler).unwrap();
            }

            let mut bytecode = Vec::new();
            let mut diagnostics = Vec::new();
            let compile_result = compiler.compile(&mut bytecode, &mut |diag| {
                diagnostics.push(diag);
                ControlFlow::<Infallible>::Continue(())
            });

            let bytecode = match compile_result {
                Ok(()) => Some(bytecode),
                Err(err) if matches!(err.kind(), ErrorKind::Fail) => None,
                Err(err) => return Err(Error::compile(strand, err)),
            };

            create_result(
                global,
                strand,
                &path,
                source,
                bytecode,
                diagnostics,
                &mut out,
            )?;
            Ok(())
        })
        .commit();
}
