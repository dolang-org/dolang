use std::{
    cell::Cell,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem,
    path::Path,
};

use dolang::{
    compile::{self, Config, Diag, Mode},
    extension::CompilerExt,
    runtime::{
        Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Type, Value,
        object::{Mut, Ref, TypeBuilder},
        unpack,
        value::{Array, Dict, Empty, Nil, PinBin, PinStr, TypeObject, View},
        vm::{Builder, Stateful},
    },
};

#[cfg(feature = "diagnostic-rendering")]
use dolang::runtime::{error::ErrorKind as RuntimeErrorKind, method};

pub(crate) struct Types<'v> {
    unit: Type<'v, UnitObject<'v>>,
    diagnostic_iter: Type<'v, DiagnosticIter>,
    node_iter: Type<'v, NodeIter>,
    token_iter: Type<'v, TokenIter<'v>>,
    token: Type<'v, TokenObject>,
    node_id: Type<'v, NodeIdObject>,
    super_ref: Type<'v, SuperObject>,
    node: Type<'v, NodeObject<NodeTag>>,
    declaration: Type<'v, NodeObject<DeclarationTag>>,
    import: Type<'v, NodeObject<ImportTag>>,
    param: Type<'v, NodeObject<ParamTag>>,
    block: Type<'v, NodeObject<BlockTag>>,
    reference: Type<'v, NodeObject<ReferenceTag>>,
    concrete_nodes: ConcreteNodeTypes<'v>,
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
    token_comment: Sym<'v, 'v>,
    token_constant: Sym<'v, 'v>,
    token_delim: Sym<'v, 'v>,
    token_escape: Sym<'v, 'v>,
    token_field: Sym<'v, 'v>,
    token_method: Sym<'v, 'v>,
    token_key: Sym<'v, 'v>,
    token_module_name: Sym<'v, 'v>,
    token_module_item: Sym<'v, 'v>,
    token_keyword: Sym<'v, 'v>,
    token_literal: Sym<'v, 'v>,
    token_number: Sym<'v, 'v>,
    token_operator: Sym<'v, 'v>,
    token_string_delim: Sym<'v, 'v>,
    token_variable: Sym<'v, 'v>,
    token_sigil: Sym<'v, 'v>,
    token_context_call: Sym<'v, 'v>,
}

pub(crate) struct Global<'v> {
    types: Types<'v>,
    syms: Syms<'v>,
    next_unit_id: Cell<u64>,
}

pub(crate) struct ConcreteNodeTypes<'v> {
    root: Type<'v, NodeObject<RootTag>>,
    class: Type<'v, NodeObject<ClassTag>>,
    function: Type<'v, NodeObject<FunctionTag>>,
    method: Type<'v, NodeObject<MethodTag>>,
    special_method: Type<'v, NodeObject<SpecialMethodTag>>,
    field: Type<'v, NodeObject<FieldTag>>,
    bind: Type<'v, NodeObject<BindTag>>,
    self_param: Type<'v, NodeObject<SelfParamTag>>,
    import_module: Type<'v, NodeObject<ImportModuleTag>>,
    import_item: Type<'v, NodeObject<ImportItemTag>>,
    prelude_module: Type<'v, NodeObject<PreludeModuleTag>>,
    prelude_item: Type<'v, NodeObject<PreludeItemTag>>,
    positional_param: Type<'v, NodeObject<PositionalParamTag>>,
    key_param: Type<'v, NodeObject<KeyParamTag>>,
    rest_param: Type<'v, NodeObject<RestParamTag>>,
    lambda: Type<'v, NodeObject<LambdaTag>>,
    if_node: Type<'v, NodeObject<IfTag>>,
    else_node: Type<'v, NodeObject<ElseTag>>,
    while_node: Type<'v, NodeObject<WhileTag>>,
    for_node: Type<'v, NodeObject<ForTag>>,
    try_node: Type<'v, NodeObject<TryTag>>,
    catch: Type<'v, NodeObject<CatchTag>>,
    finally: Type<'v, NodeObject<FinallyTag>>,
    for_elem: Type<'v, NodeObject<ForElemTag>>,
    if_elem: Type<'v, NodeObject<IfElemTag>>,
    decorator: Type<'v, NodeObject<DecoratorTag>>,
    break_node: Type<'v, NodeObject<BreakTag>>,
    continue_node: Type<'v, NodeObject<ContinueTag>>,
    return_node: Type<'v, NodeObject<ReturnTag>>,
}

pub struct Tag;

impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        let node = builder.register_type();
        let declaration = builder
            .build_type::<NodeObject<DeclarationTag>>((), ())
            .nominal_supertype(node)
            .build();
        let import = builder
            .build_type::<NodeObject<ImportTag>>((), ())
            .nominal_supertype(declaration)
            .build();
        let param = builder
            .build_type::<NodeObject<ParamTag>>((), ())
            .nominal_supertype(node)
            .build();
        let block = builder
            .build_type::<NodeObject<BlockTag>>((), ())
            .nominal_supertype(node)
            .build();
        let reference = builder
            .build_type::<NodeObject<ReferenceTag>>((), ())
            .nominal_supertype(node)
            .build();
        macro_rules! subtype {
            ($tag:ty, $base:expr) => {
                builder
                    .build_type::<NodeObject<$tag>>((), ())
                    .nominal_supertype($base)
                    .build()
            };
        }
        Self {
            types: Types {
                unit: builder.register_type(),
                diagnostic_iter: builder.register_type(),
                node_iter: builder.register_type(),
                token_iter: builder.register_type(),
                token: builder.register_type(),
                node_id: builder.register_type(),
                super_ref: builder.register_type(),
                node,
                declaration,
                import,
                param,
                block,
                reference,
                concrete_nodes: ConcreteNodeTypes {
                    root: subtype!(RootTag, node),
                    class: subtype!(ClassTag, declaration),
                    function: subtype!(FunctionTag, declaration),
                    method: subtype!(MethodTag, declaration),
                    special_method: subtype!(SpecialMethodTag, declaration),
                    field: subtype!(FieldTag, declaration),
                    bind: subtype!(BindTag, declaration),
                    self_param: subtype!(SelfParamTag, declaration),
                    import_module: subtype!(ImportModuleTag, import),
                    import_item: subtype!(ImportItemTag, import),
                    prelude_module: subtype!(PreludeModuleTag, import),
                    prelude_item: subtype!(PreludeItemTag, import),
                    positional_param: subtype!(PositionalParamTag, param),
                    key_param: subtype!(KeyParamTag, param),
                    rest_param: subtype!(RestParamTag, param),
                    lambda: subtype!(LambdaTag, block),
                    if_node: subtype!(IfTag, block),
                    else_node: subtype!(ElseTag, block),
                    while_node: subtype!(WhileTag, block),
                    for_node: subtype!(ForTag, block),
                    try_node: subtype!(TryTag, block),
                    catch: subtype!(CatchTag, block),
                    finally: subtype!(FinallyTag, block),
                    for_elem: subtype!(ForElemTag, block),
                    if_elem: subtype!(IfElemTag, block),
                    decorator: subtype!(DecoratorTag, reference),
                    break_node: subtype!(BreakTag, reference),
                    continue_node: subtype!(ContinueTag, reference),
                    return_node: subtype!(ReturnTag, reference),
                },
                diagnostic: builder.register_type(),
                span: builder.register_type(),
                pos: builder.register_type(),
                annotation: builder.register_type(),
                note: builder.register_type(),
                patch: builder.register_type(),
            },
            syms: Syms {
                error: builder.sym("ERROR"),
                warning: builder.sym("WARNING"),
                primary: builder.sym("PRIMARY"),
                context: builder.sym("CONTEXT"),
                info: builder.sym("INFO"),
                help: builder.sym("HELP"),
                token_comment: builder.sym("COMMENT"),
                token_constant: builder.sym("CONSTANT"),
                token_delim: builder.sym("DELIM"),
                token_escape: builder.sym("ESCAPE"),
                token_field: builder.sym("FIELD"),
                token_method: builder.sym("METHOD"),
                token_key: builder.sym("KEY"),
                token_module_name: builder.sym("MODULE_NAME"),
                token_module_item: builder.sym("MODULE_ITEM"),
                token_keyword: builder.sym("KEYWORD"),
                token_literal: builder.sym("LITERAL"),
                token_number: builder.sym("NUMBER"),
                token_operator: builder.sym("OPERATOR"),
                token_string_delim: builder.sym("STRING_DELIM"),
                token_variable: builder.sym("VARIABLE"),
                token_sigil: builder.sym("SIGIL"),
                token_context_call: builder.sym("CALL"),
            },
            next_unit_id: Cell::new(1),
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

enum Backing<'v> {
    Str(PinStr<'v, 'static>),
    Bin(PinBin<'v, 'static>),
}

impl Backing<'_> {
    fn bytes(&self) -> &'static [u8] {
        // SAFETY: the pin is retained for the lifetime of the compiler unit and
        // its owning Do value is rooted in the unit's GC slot.
        unsafe {
            mem::transmute(match self {
                Self::Str(v) => v.as_bytes(),
                Self::Bin(v) => &**v,
            })
        }
    }
}

pub(crate) struct UnitObject<'v> {
    // Fields are dropped in declaration order: the borrowing unit before its pin.
    unit: Option<compile::Unit<'static>>,
    _backing: Backing<'v>,
    path: Box<Path>,
    _module: Option<String>,
    identity: u64,
}

pub(crate) struct DiagnosticAnnex<'v> {
    global: State<'v, Global<'v>>,
    diag: Diag,
    #[cfg(feature = "diagnostic-rendering")]
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

pub(crate) struct DiagnosticIter {
    index: usize,
}
pub(crate) struct NodeIter {
    cursor: Option<compile::NodeId>,
}
// Tokens are popped off the end, so `tokens` is stored in reverse emission order.
pub(crate) struct TokenIter<'v> {
    tokens: Vec<TokenAnnex<'v>>,
}
pub(crate) struct TokenObject;
pub(crate) struct TokenAnnex<'v> {
    global: State<'v, Global<'v>>,
    kind: compile::Token,
    span: SpanData,
    node: Option<compile::NodeId>,
    context: compile::Context,
}
pub(crate) struct NodeIdObject;

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub(crate) struct NodeIdAnnex {
    unit: u64,
    id: compile::NodeId,
}
pub(crate) struct SuperObject {
    span: SpanData,
    target: Option<compile::NodeId>,
}

pub(crate) trait NodeMarker {
    const NAME: &'static str;
}
pub(crate) struct NodeObject<T: NodeMarker> {
    id: compile::NodeId,
    marker: PhantomData<T>,
}

macro_rules! node_tags {
    ($($tag:ident => $name:literal),* $(,)?) => {$ (
        pub(crate) struct $tag;
        impl NodeMarker for $tag { const NAME: &'static str = $name; }
    )* };
}
node_tags! {
    NodeTag=>"Node", DeclarationTag=>"Declaration", ImportTag=>"Import", ParamTag=>"Param",
    BlockTag=>"Block", ReferenceTag=>"Reference", RootTag=>"Root", ClassTag=>"Class", FunctionTag=>"Function",
    MethodTag=>"Method", SpecialMethodTag=>"SpecialMethod", FieldTag=>"Field", BindTag=>"Bind",
    SelfParamTag=>"SelfParam", ImportModuleTag=>"ImportModule", ImportItemTag=>"ImportItem",
    PreludeModuleTag=>"PreludeModule", PreludeItemTag=>"PreludeItem", PositionalParamTag=>"PositionalParam",
    KeyParamTag=>"KeyParam", RestParamTag=>"RestParam", LambdaTag=>"Lambda", IfTag=>"If", ElseTag=>"Else",
    WhileTag=>"While", ForTag=>"For", TryTag=>"Try", CatchTag=>"Catch", FinallyTag=>"Finally",
    ForElemTag=>"ForElem", IfElemTag=>"IfElem", DecoratorTag=>"Decorator", BreakTag=>"Break",
    ContinueTag=>"Continue", ReturnTag=>"Return"
}
pub(crate) struct Diagnostic;
pub(crate) struct Span;
pub(crate) struct Pos;
pub(crate) struct Annotation;
pub(crate) struct Note;
pub(crate) struct Patch;

const OWNER: usize = 0;
const UNIT_SOURCE: usize = 0;

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

fn token_kind_sym<'v>(global: State<'v, Global<'v>>, token: compile::Token) -> Sym<'v, 'v> {
    match token {
        compile::Token::Comment => global.syms.token_comment,
        compile::Token::Constant => global.syms.token_constant,
        compile::Token::Delim => global.syms.token_delim,
        compile::Token::Escape => global.syms.token_escape,
        compile::Token::Field => global.syms.token_field,
        compile::Token::Method => global.syms.token_method,
        compile::Token::Key => global.syms.token_key,
        compile::Token::ModuleName => global.syms.token_module_name,
        compile::Token::ModuleItem => global.syms.token_module_item,
        compile::Token::Keyword => global.syms.token_keyword,
        compile::Token::Literal => global.syms.token_literal,
        compile::Token::Number => global.syms.token_number,
        compile::Token::Operator => global.syms.token_operator,
        compile::Token::StringDelim => global.syms.token_string_delim,
        compile::Token::Variable => global.syms.token_variable,
        compile::Token::Sigil => global.syms.token_sigil,
    }
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
    #[cfg(not(feature = "diagnostic-rendering"))]
    let _ = path;

    global.types.diagnostic.create_with_annex(
        strand,
        Diagnostic,
        DiagnosticAnnex {
            global,
            diag,
            #[cfg(feature = "diagnostic-rendering")]
            path: path.to_owned(),
        },
        &mut *out,
    );

    global
        .types
        .diagnostic
        .cast(&*out)
        .unwrap()
        .enter_sync(strand, |strand, inst| {
            {
                let mut borrow = inst.borrow_mut_unwrap();
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
        })
}

fn with_unit<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    f: impl for<'a> FnOnce(&mut Strand<'v, 's>, Instance<'v, 'a, UnitObject<'v>>) -> Result<'v, 's, R>,
) -> Result<'v, 's, R> {
    let cast = strand
        .state::<Global<'v>>()
        .types
        .unit
        .cast(value)
        .ok_or_else(|| Error::state_error(strand, "invalid unit reference"))?;
    cast.enter_sync(strand, f)
}

fn apply_prelude_module_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    config: &mut Config,
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
                    _ => Err(Error::type_error(strand, "prelude item must be Str or Sym")),
                }
            },
        )?;
        let _prelude = config
            .prelude()
            .import_items(module_name)
            .item(item_name)
            .commit();
    }
    Ok(())
}

fn apply_prelude_dict_items<'v, 's>(
    strand: &mut Strand<'v, 's>,
    config: &mut Config,
    module_name: &str,
    dict: &Dict<'v, '_>,
) -> std::result::Result<(), Error<'v, 's>> {
    let vm = strand.vm();
    let mut pairs = dict.pairs();
    let mut items_builder = Some(config.prelude().import_items(module_name));

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
                            "prelude item key must be Str or Sym",
                        ));
                    }
                };
                let bind_name = match v.view(vm) {
                    View::Str(s) => s.into(),
                    View::Sym(sym) => sym.as_str(vm).to_owned(),
                    _ => {
                        return Err(Error::type_error(
                            strand,
                            "prelude item binding must be Str or Sym",
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
    config: &mut Config,
    value: &Value<'v>,
) -> Result<'v, 's, ()> {
    strand.with_slots_sync(|strand, [mut elem, mut k, mut v]| {
        match value.view(strand.vm()) {
            View::Str(module) => {
                strand.access(|access| config.prelude().import_module(module.as_str(access)));
            }
            View::Sym(sym) => {
                config.prelude().import_module(sym.as_str(strand));
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
                                "prelude array item must be Str or Sym",
                            ));
                        }
                    };
                    config.prelude().import_module(name);
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
                                "prelude module key must be Str or Sym",
                            ));
                        }
                    };

                    match v.view(strand.vm()) {
                        View::Str(bind) => {
                            strand.access(|access| {
                                config
                                    .prelude()
                                    .import_module_with_name(&module, bind.as_str(access));
                            });
                        }
                        View::Sym(bind) => {
                            config
                                .prelude()
                                .import_module_with_name(&module, bind.as_str(strand));
                        }
                        View::Array(arr) => {
                            apply_prelude_module_items(strand, config, &module, &arr)?;
                        }
                        View::Dict(dict) => {
                            apply_prelude_dict_items(strand, config, &module, &dict)?;
                        }
                        _ => {
                            return Err(Error::type_error(
                                strand,
                                "prelude module value must be str, sym, array, or Dict",
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(Error::type_error(
                    strand,
                    "prelude must be a module name, array, or Dict",
                ));
            }
        }

        Ok(())
    })
}

impl<'v> Object<'v> for UnitObject<'v> {
    const NAME: &'v str = "Unit";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("diagnostics", async move |this, strand, args, mut out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if this.borrow(strand)?.unit.is_none() {
                    return Err(Error::state_error(strand, "unit was emitted"));
                }
                let ty = strand.state::<Global<'v>>().types.diagnostic_iter;
                ty.create(strand, DiagnosticIter { index: 0 }, &mut out);
                ty.cast(&out).unwrap().enter_sync(strand, |strand, iter| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<OWNER>(&mut iter.borrow_mut_unwrap()),
                        this,
                    );
                });
                Ok(())
            })
            .method("nodes", async move |this, strand, args, mut out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                if this.borrow(strand)?.unit.is_none() {
                    return Err(Error::state_error(strand, "unit was emitted"));
                }
                let ty = strand.state::<Global<'v>>().types.node_iter;
                ty.create(strand, NodeIter { cursor: None }, &mut out);
                ty.cast(&out).unwrap().enter_sync(strand, |strand, iter| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<OWNER>(&mut iter.borrow_mut_unwrap()),
                        this,
                    );
                });
                Ok(())
            })
            .method("node", async move |this, strand, args, mut out| {
                let ([id], []) = unpack!(strand, args, 1, 0)?;
                let Some(id_obj) = strand.state::<Global<'v>>().types.node_id.cast(&id) else {
                    return Err(Error::type_error(strand, "expected `compile.NodeId`"));
                };
                let id_data = id_obj.enter_sync(strand, |_strand, id| *id.annex());
                let borrow = this.borrow(strand)?;
                let Some(unit) = borrow.unit.as_ref() else {
                    return Err(Error::state_error(strand, "unit was emitted"));
                };
                if id_data.unit != borrow.identity || unit.node(id_data.id).is_none() {
                    Output::set(strand, out, Nil);
                    return Ok(());
                }
                create_node(strand, this, id_data.id, &mut out)
            })
            .method("tokens", async move |this, strand, args, mut out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = strand.state::<Global<'v>>();
                let mut tokens = {
                    let borrow = this.borrow(strand)?;
                    let Some(unit) = borrow.unit.as_ref() else {
                        return Err(Error::state_error(strand, "unit was emitted"));
                    };
                    let mut tokens = Vec::new();
                    unit.tokens(&mut |kind, span, node, context| {
                        tokens.push(TokenAnnex {
                            global,
                            kind,
                            span: span_data(span),
                            node,
                            context,
                        });
                    });
                    tokens
                };
                // Popped off the end by TokenIter::next, so store in reverse.
                tokens.reverse();
                let ty = global.types.token_iter;
                ty.create(strand, TokenIter { tokens }, &mut out);
                ty.cast(&out).unwrap().enter_sync(strand, |strand, iter| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<OWNER>(&mut iter.borrow_mut_unwrap()),
                        this,
                    );
                });
                Ok(())
            })
            .method("emit", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let unit = this
                    .borrow_mut(strand)?
                    .unit
                    .take()
                    .ok_or_else(|| Error::state_error(strand, "unit was emitted"))?;
                let mut bytecode = Vec::new();
                unit.emit(&mut bytecode)
                    .map_err(|err| Error::compile(strand, err))?;
                Output::set(strand, out, bytecode.as_slice());
                Ok(())
            })
    }
}

impl<'v> Object<'v> for DiagnosticIter {
    const NAME: &'v str = "DiagnosticIter";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }
    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }
    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut iter = this.borrow_mut(strand)?;
        let index = iter.index;
        iter.index += 1;
        let owner_value = Mut::slot::<OWNER>(&iter);
        with_unit(strand, owner_value, |strand, owner| {
            let unit_borrow = owner.borrow(strand)?;
            let Some(unit) = unit_borrow.unit.as_ref() else {
                return Err(Error::state_error(strand, "unit was emitted"));
            };
            let Some(diag) = unit.diagnostics().nth(index) else {
                return Ok(false);
            };
            let source = Ref::slot::<UNIT_SOURCE>(&unit_borrow);
            let path = unit_borrow.path.to_string_lossy();
            create_diagnostic(strand.state(), strand, &path, source, diag, &mut out)?;
            Ok(true)
        })
    }
}

impl<'v> Object<'v> for NodeIter {
    const NAME: &'v str = "NodeIter";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }
    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }
    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut iter = this.borrow_mut(strand)?;
        let cursor = iter.cursor;
        let result = with_unit(strand, Mut::slot::<OWNER>(&iter), |strand, owner| {
            let owner_borrow = owner.borrow(strand)?;
            let Some(unit) = owner_borrow.unit.as_ref() else {
                return Err(Error::state_error(strand, "unit was emitted"));
            };
            let Some(id) = unit.next_id(cursor) else {
                return Ok(None);
            };
            strand.with_slots_sync(|strand, [mut id_out, mut node_out]| {
                create_node_id(strand, owner_borrow.identity, id, &mut id_out);
                create_node(strand, owner, id, &mut node_out)?;
                Output::set(strand, &mut out, Empty::Array);
                let arr = out.as_array(strand).unwrap();
                arr.push(strand, &mut id_out)?;
                arr.push(strand, &mut node_out)?;
                Ok(())
            })?;
            Ok(Some(id))
        })?;
        let Some(id) = result else { return Ok(false) };
        iter.cursor = Some(id);
        Ok(true)
    }
}

impl<'v> Object<'v> for TokenIter<'v> {
    const NAME: &'v str = "TokenIter";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }
    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }
    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let mut iter = this.borrow_mut(strand)?;
        let Some(annex) = iter.tokens.pop() else {
            return Ok(false);
        };
        let ty = annex.global.types.token;
        with_unit(strand, Mut::slot::<OWNER>(&iter), |strand, owner| {
            ty.create_with_annex(strand, TokenObject, annex, &mut out);
            ty.cast(&out).unwrap().enter_sync(strand, |strand, tok| {
                Output::set(
                    strand,
                    Mut::slot_mut::<OWNER>(&mut tok.borrow_mut_unwrap()),
                    owner,
                )
            });
            Ok(())
        })?;
        Ok(true)
    }
}

impl<'v> Object<'v> for TokenObject {
    const NAME: &'v str = "Token";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = TokenAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("kind", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    token_kind_sym(this.annex().global, this.annex().kind),
                );
                Ok(())
            })
            .get("span", |this, strand, out| {
                create_span(this.annex().global, strand, this.annex().span.clone(), out);
                Ok(())
            })
            .get("node", |this, strand, mut out| {
                let b = this.borrow(strand)?;
                if let Some(id) = this.annex().node {
                    let u = with_unit(strand, Ref::slot::<OWNER>(&b), |strand, owner| {
                        Ok(owner.borrow(strand)?.identity)
                    })?;
                    create_node_id(strand, u, id, &mut out)
                } else {
                    Output::set(strand, out, Nil)
                };
                Ok(())
            })
            .get("context", |this, strand, out| {
                match this.annex().context {
                    compile::Context::Call => {
                        Output::set(strand, out, this.annex().global.syms.token_context_call)
                    }
                    _ => Output::set(strand, out, Nil),
                };
                Ok(())
            })
    }
}

impl<'v> Object<'v> for NodeIdObject {
    const NAME: &'v str = "NodeId";
    const MODULE: &'v str = "compile";
    type Annex = NodeIdAnnex;
    type Type = ();
    type TypeAnnex = ();
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let Some(other) = strand.state::<Global<'v>>().types.node_id.cast(other) else {
            return Ok(false);
        };
        Ok(other.enter_sync(strand, |_strand, other| *this.annex() == *other.annex()))
    }
    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        h: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().hash(h);
        Ok(())
    }
}

fn create_node_id<'v>(
    strand: &mut Strand<'v, '_>,
    unit: u64,
    id: compile::NodeId,
    out: &mut Slot<'v, '_>,
) {
    strand
        .state::<Global<'v>>()
        .types
        .node_id
        .create_with_annex(strand, NodeIdObject, NodeIdAnnex { unit, id }, out)
}

fn create_typed_node<'v, T: NodeMarker + 'static>(
    strand: &mut Strand<'v, '_>,
    ty: Type<'v, NodeObject<T>>,
    owner: Instance<'v, '_, UnitObject<'v>>,
    id: compile::NodeId,
    out: &mut Slot<'v, '_>,
) {
    ty.create(
        strand,
        NodeObject {
            id,
            marker: PhantomData,
        },
        &mut *out,
    );
    ty.cast(out).unwrap().enter_sync(strand, |strand, node| {
        Output::set(
            strand,
            Mut::slot_mut::<OWNER>(&mut node.borrow_mut_unwrap()),
            owner,
        )
    });
}

fn create_node<'v, 's>(
    strand: &mut Strand<'v, 's>,
    owner: Instance<'v, '_, UnitObject<'v>>,
    id: compile::NodeId,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    enum Which {
        Root,
        Class,
        Function,
        Method,
        SpecialMethod,
        Field,
        Bind,
        SelfParam,
        ImportModule,
        ImportItem,
        PreludeModule,
        PreludeItem,
        PositionalParam,
        KeyParam,
        RestParam,
        Lambda,
        If,
        Else,
        While,
        For,
        Try,
        Catch,
        Finally,
        ForElem,
        IfElem,
        Decorator,
        Break,
        Continue,
        Return,
    }
    let kind = {
        let b = owner.borrow(strand)?;
        match b
            .unit
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "unit was emitted"))?
            .node(id)
            .unwrap()
            .kind()
        {
            compile::Kind::Root => Which::Root,
            compile::Kind::Class { .. } => Which::Class,
            compile::Kind::Function { .. } => Which::Function,
            compile::Kind::Method { .. } => Which::Method,
            compile::Kind::SpecialMethod { .. } => Which::SpecialMethod,
            compile::Kind::Field { .. } => Which::Field,
            compile::Kind::Bind { .. } => Which::Bind,
            compile::Kind::SelfParam { .. } => Which::SelfParam,
            compile::Kind::ImportModule { .. } => Which::ImportModule,
            compile::Kind::ImportItem { .. } => Which::ImportItem,
            compile::Kind::PreludeModule { .. } => Which::PreludeModule,
            compile::Kind::PreludeItem { .. } => Which::PreludeItem,
            compile::Kind::PositionalParam { .. } => Which::PositionalParam,
            compile::Kind::KeyParam { .. } => Which::KeyParam,
            compile::Kind::RestParam { .. } => Which::RestParam,
            compile::Kind::Lambda => Which::Lambda,
            compile::Kind::If => Which::If,
            compile::Kind::Else => Which::Else,
            compile::Kind::While => Which::While,
            compile::Kind::For => Which::For,
            compile::Kind::Try => Which::Try,
            compile::Kind::Catch => Which::Catch,
            compile::Kind::Finally => Which::Finally,
            compile::Kind::ForElem => Which::ForElem,
            compile::Kind::IfElem => Which::IfElem,
            compile::Kind::Decorator { .. } => Which::Decorator,
            compile::Kind::Break { .. } => Which::Break,
            compile::Kind::Continue { .. } => Which::Continue,
            compile::Kind::Return { .. } => Which::Return,
            _ => unreachable!(),
        }
    };
    let t = &strand.state::<Global<'v>>().types.concrete_nodes;
    macro_rules! make {
        ($ty:expr,$tag:ty) => {{
            let ty = $ty;
            create_typed_node::<$tag>(strand, ty, owner, id, out)
        }};
    }
    match kind {
        Which::Root => make!(t.root, RootTag),
        Which::Class => make!(t.class, ClassTag),
        Which::Function => make!(t.function, FunctionTag),
        Which::Method => make!(t.method, MethodTag),
        Which::SpecialMethod => make!(t.special_method, SpecialMethodTag),
        Which::Field => make!(t.field, FieldTag),
        Which::Bind => make!(t.bind, BindTag),
        Which::SelfParam => make!(t.self_param, SelfParamTag),
        Which::ImportModule => make!(t.import_module, ImportModuleTag),
        Which::ImportItem => make!(t.import_item, ImportItemTag),
        Which::PreludeModule => make!(t.prelude_module, PreludeModuleTag),
        Which::PreludeItem => make!(t.prelude_item, PreludeItemTag),
        Which::PositionalParam => make!(t.positional_param, PositionalParamTag),
        Which::KeyParam => make!(t.key_param, KeyParamTag),
        Which::RestParam => make!(t.rest_param, RestParamTag),
        Which::Lambda => make!(t.lambda, LambdaTag),
        Which::If => make!(t.if_node, IfTag),
        Which::Else => make!(t.else_node, ElseTag),
        Which::While => make!(t.while_node, WhileTag),
        Which::For => make!(t.for_node, ForTag),
        Which::Try => make!(t.try_node, TryTag),
        Which::Catch => make!(t.catch, CatchTag),
        Which::Finally => make!(t.finally, FinallyTag),
        Which::ForElem => make!(t.for_elem, ForElemTag),
        Which::IfElem => make!(t.if_elem, IfElemTag),
        Which::Decorator => make!(t.decorator, DecoratorTag),
        Which::Break => make!(t.break_node, BreakTag),
        Which::Continue => make!(t.continue_node, ContinueTag),
        Which::Return => make!(t.return_node, ReturnTag),
    }
    Ok(())
}

impl<'v, T: NodeMarker + 'static> Object<'v> for NodeObject<T> {
    const NAME: &'v str = T::NAME;
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder.get("parent", |this, strand, mut out| {
            let (id, unit) = with_node(this, strand, |n, u| (n.parent(), u))?;
            if let Some(id) = id {
                create_node_id(strand, unit, id, &mut out)
            } else {
                Output::set(strand, out, Nil)
            };
            Ok(())
        });
        builder = builder.get("span", |this, strand, out| {
            let span = with_node(this, strand, |n, _| span_data(n.span()))?;
            create_span(strand.state(), strand, span, out);
            Ok(())
        });
        // Every node answers, rather than only the kinds that can be
        // documented: which of them those are is the compiler's determination,
        // and a consumer that has to know it in advance gains nothing.
        builder = builder.get("doc", |this, strand, out| {
            let doc = with_node(this, strand, |n, _| n.doc().map(span_data))?;
            if let Some(doc) = doc {
                create_span(strand.state(), strand, doc, out)
            } else {
                Output::set(strand, out, Nil)
            };
            Ok(())
        });
        if matches!(
            T::NAME,
            "Class"
                | "Function"
                | "Method"
                | "SpecialMethod"
                | "Field"
                | "Bind"
                | "SelfParam"
                | "ImportModule"
                | "ImportItem"
                | "PreludeModule"
                | "PreludeItem"
                | "PositionalParam"
                | "KeyParam"
                | "RestParam"
        ) {
            builder = builder.get("name", |this, strand, out| project_name(this, strand, out));
        }
        if matches!(
            T::NAME,
            "Class" | "Function" | "Method" | "Field" | "Bind" | "ImportModule" | "ImportItem"
        ) {
            builder = builder.get("is_pub", |this, strand, out| project_pub(this, strand, out));
        }
        if matches!(T::NAME, "PositionalParam" | "KeyParam") {
            builder = builder.get("default", |this, strand, out| {
                project_default(this, strand, out)
            });
        }
        if T::NAME == "KeyParam" {
            builder = builder.get("key", |this, strand, out| {
                project_span_field(this, strand, "key", out)
            });
        }
        if matches!(
            T::NAME,
            "ImportModule" | "ImportItem" | "PreludeModule" | "PreludeItem"
        ) {
            builder = builder.get("module", |this, strand, out| {
                project_import(this, strand, false, out)
            });
        }
        if matches!(T::NAME, "ImportItem" | "PreludeItem") {
            builder = builder.get("item", |this, strand, out| {
                project_import(this, strand, true, out)
            });
        }
        if matches!(T::NAME, "Decorator" | "Break" | "Continue" | "Return") {
            builder = builder.get("target", |this, strand, out| {
                project_target(this, strand, out)
            });
        }
        if T::NAME == "Class" {
            builder = builder.get("supers", |this, strand, out| {
                project_supers(this, strand, out)
            });
        }
        builder
    }
}

fn with_node<'v, 's, T: NodeMarker + 'static, R>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(compile::Node<'_>, u64) -> R,
) -> Result<'v, 's, R> {
    let b = this.borrow(strand)?;
    let id = b.id;
    with_unit(strand, Ref::slot::<OWNER>(&b), |strand, owner| {
        let ub = owner.borrow(strand)?;
        let unit = ub
            .unit
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "unit was emitted"))?;
        let node = unit
            .node(id)
            .ok_or_else(|| Error::state_error(strand, "unit was emitted"))?;
        Ok(f(node, ub.identity))
    })
}

fn project_name<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    enum Name {
        Span(SpanData),
        Text(String),
        None,
    }
    let name = with_node(this, strand, |n, _| match n.kind() {
        compile::Kind::Class { name, .. }
        | compile::Kind::Function { name, .. }
        | compile::Kind::Method { name, .. }
        | compile::Kind::SpecialMethod { name }
        | compile::Kind::Field { name, .. }
        | compile::Kind::Bind { name, .. }
        | compile::Kind::SelfParam { name }
        | compile::Kind::ImportModule { name, .. }
        | compile::Kind::ImportItem { name, .. }
        | compile::Kind::PositionalParam { name, .. }
        | compile::Kind::KeyParam { name, .. } => Name::Span(span_data(name)),
        compile::Kind::RestParam { name } => name.map_or(Name::None, |v| Name::Span(span_data(v))),
        compile::Kind::PreludeModule { name, .. } | compile::Kind::PreludeItem { name, .. } => {
            Name::Text(name.to_owned())
        }
        _ => unreachable!(),
    })?;
    match name {
        Name::Span(s) => create_span(strand.state(), strand, s, out),
        Name::Text(s) => Output::set(strand, out, s.as_str()),
        Name::None => Output::set(strand, out, Nil),
    };
    Ok(())
}
fn project_pub<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let v = with_node(this, strand, |n, _| match n.kind() {
        compile::Kind::Class { is_pub, .. }
        | compile::Kind::Function { is_pub, .. }
        | compile::Kind::Method { is_pub, .. }
        | compile::Kind::Field { is_pub, .. }
        | compile::Kind::Bind { is_pub, .. }
        | compile::Kind::ImportModule { is_pub, .. }
        | compile::Kind::ImportItem { is_pub, .. } => is_pub,
        _ => unreachable!(),
    })?;
    Output::set(strand, out, v);
    Ok(())
}
fn project_default<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let v = with_node(this, strand, |n, _| match n.kind() {
        compile::Kind::PositionalParam { default, .. }
        | compile::Kind::KeyParam { default, .. } => default.map(span_data),
        _ => unreachable!(),
    })?;
    if let Some(v) = v {
        create_span(strand.state(), strand, v, out)
    } else {
        Output::set(strand, out, Nil)
    };
    Ok(())
}
fn project_span_field<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    _field: &str,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let v = with_node(this, strand, |n, _| match n.kind() {
        compile::Kind::KeyParam { key, .. } => span_data(key),
        _ => unreachable!(),
    })?;
    create_span(strand.state(), strand, v, out);
    Ok(())
}
fn project_import<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    item: bool,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    enum V {
        S(SpanData),
        T(String),
    }
    let v = with_node(this, strand, |n, _| match (n.kind(), item) {
        (compile::Kind::ImportModule { module, .. }, false)
        | (compile::Kind::ImportItem { module, .. }, false) => V::S(span_data(module)),
        (compile::Kind::ImportItem { item, .. }, true) => V::S(span_data(item)),
        (compile::Kind::PreludeModule { module, .. }, false)
        | (compile::Kind::PreludeItem { module, .. }, false) => V::T(module.to_owned()),
        (compile::Kind::PreludeItem { item, .. }, true) => V::T(item.to_owned()),
        _ => unreachable!(),
    })?;
    match v {
        V::S(v) => create_span(strand.state(), strand, v, out),
        V::T(v) => Output::set(strand, out, v.as_str()),
    };
    Ok(())
}
fn project_target<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let (v, u) = with_node(this, strand, |n, u| {
        (
            match n.kind() {
                compile::Kind::Decorator { target }
                | compile::Kind::Break { target }
                | compile::Kind::Continue { target }
                | compile::Kind::Return { target } => target,
                _ => unreachable!(),
            },
            u,
        )
    })?;
    if let Some(v) = v {
        create_node_id(strand, u, v, &mut out)
    } else {
        Output::set(strand, out, Nil)
    };
    Ok(())
}
fn project_supers<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let (values, u) = with_node(this, strand, |n, u| {
        (
            match n.kind() {
                compile::Kind::Class { supers, .. } => supers
                    .map(|s| (span_data(s.span), s.target))
                    .collect::<Vec<_>>(),
                _ => unreachable!(),
            },
            u,
        )
    })?;
    Output::set(strand, &mut out, Empty::Array);
    let arr = out.as_array(strand).unwrap();
    strand.with_slots_sync(|strand, [mut value]| {
        for (span, target) in values {
            let ty = strand.state::<Global<'v>>().types.super_ref;
            ty.create(strand, SuperObject { span, target }, &mut value);
            ty.cast(&value).unwrap().enter_sync(strand, |strand, s| {
                Output::set(
                    strand,
                    Mut::slot_mut::<OWNER>(&mut s.borrow_mut_unwrap()),
                    Ref::slot::<OWNER>(&this.borrow_unwrap()),
                )
            });
            arr.push(strand, &mut value)?;
        }
        Ok(())
    })?;
    let _ = u;
    Ok(())
}

impl<'v> Object<'v> for SuperObject {
    const NAME: &'v str = "Super";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("span", |this, strand, out| {
                let span = this.borrow(strand)?.span.clone();
                create_span(strand.state(), strand, span, out);
                Ok(())
            })
            .get("target", |this, strand, mut out| {
                let b = this.borrow(strand)?;
                if let Some(id) = b.target {
                    let u = with_unit(strand, Ref::slot::<OWNER>(&b), |strand, owner| {
                        Ok(owner.borrow(strand)?.identity)
                    })?;
                    create_node_id(strand, u, id, &mut out)
                } else {
                    Output::set(strand, out, Nil)
                };
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
        #[cfg(feature = "diagnostic-rendering")]
        let mut builder = builder;

        #[cfg(feature = "diagnostic-rendering")]
        let builder = {
            let preformat = builder.sym("preformat");
            builder.method_with_slots(
                "render",
                async move |this, strand, args, out, [mut term]| {
                    let ([], []) = unpack!(strand, args, 0, 0)?;
                    let rendered = {
                        let borrow = this.borrow(strand)?;
                        let source = Ref::slot::<DIAG_SOURCE>(&borrow).view(strand.vm());
                        match source {
                            View::Str(_) => (),
                            View::Bin(bin) => {
                                if strand.access(|access| {
                                    std::str::from_utf8(bin.as_slice(access)).is_err()
                                }) {
                                    return Err(Error::type_error(
                                        strand,
                                        "source: expected valid utf-8",
                                    ));
                                }
                            }
                            _ => {
                                return Err(Error::type_error(
                                    strand,
                                    "source: expected `Str` or `Bin`",
                                ));
                            }
                        }
                        strand.access(|access| {
                            let source = match source {
                                View::Str(value) => value.as_str(access),
                                View::Bin(value) => {
                                    std::str::from_utf8(value.as_slice(access)).unwrap()
                                }
                                _ => unreachable!(),
                            };
                            crate::render::render_compile_diag(
                                &this.annex().path,
                                source,
                                &this.annex().diag,
                                crate::render::ColorMode::Always,
                            )
                        })
                    };

                    match strand.import("term", &mut term).await {
                        Ok(()) => method!(strand, &term, preformat, out, rendered.as_str()).await,
                        Err(error) if error.kind() == RuntimeErrorKind::Import => {
                            Output::set(strand, out, rendered.as_str());
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                },
            )
        };

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
        if let Some(other) = this.annex().global.types.span.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().span == other.annex().span
            }))
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
        if let Some(other) = this.annex().global.types.span.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().span < other.annex().span
            }))
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
        if let Some(other) = strand.state::<Global<'v>>().types.pos.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().pos == other.annex().pos
            }))
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
        if let Some(other) = strand.state::<Global<'v>>().types.pos.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().pos < other.annex().pos
            }))
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
    let recover = builder.sym("recover");
    let document = builder.sym("document");

    builder
        .module("compile")
        .value("Unit", global.types.unit)
        .value("NodeId", global.types.node_id)
        .value("Token", global.types.token)
        .value("Super", global.types.super_ref)
        .value("Node", global.types.node)
        .value("Declaration", global.types.declaration)
        .value("Import", global.types.import)
        .value("Param", global.types.param)
        .value("Block", global.types.block)
        .value("Reference", global.types.reference)
        .value("Root", global.types.concrete_nodes.root)
        .value("Class", global.types.concrete_nodes.class)
        .value("Function", global.types.concrete_nodes.function)
        .value("Method", global.types.concrete_nodes.method)
        .value("SpecialMethod", global.types.concrete_nodes.special_method)
        .value("Field", global.types.concrete_nodes.field)
        .value("Bind", global.types.concrete_nodes.bind)
        .value("SelfParam", global.types.concrete_nodes.self_param)
        .value("ImportModule", global.types.concrete_nodes.import_module)
        .value("ImportItem", global.types.concrete_nodes.import_item)
        .value("PreludeModule", global.types.concrete_nodes.prelude_module)
        .value("PreludeItem", global.types.concrete_nodes.prelude_item)
        .value(
            "PositionalParam",
            global.types.concrete_nodes.positional_param,
        )
        .value("KeyParam", global.types.concrete_nodes.key_param)
        .value("RestParam", global.types.concrete_nodes.rest_param)
        .value("Lambda", global.types.concrete_nodes.lambda)
        .value("If", global.types.concrete_nodes.if_node)
        .value("Else", global.types.concrete_nodes.else_node)
        .value("While", global.types.concrete_nodes.while_node)
        .value("For", global.types.concrete_nodes.for_node)
        .value("Try", global.types.concrete_nodes.try_node)
        .value("Catch", global.types.concrete_nodes.catch)
        .value("Finally", global.types.concrete_nodes.finally)
        .value("ForElem", global.types.concrete_nodes.for_elem)
        .value("IfElem", global.types.concrete_nodes.if_elem)
        .value("Decorator", global.types.concrete_nodes.decorator)
        .value("Break", global.types.concrete_nodes.break_node)
        .value("Continue", global.types.concrete_nodes.continue_node)
        .value("Return", global.types.concrete_nodes.return_node)
        .value("Diagnostic", global.types.diagnostic)
        .value("Span", global.types.span)
        .value("Pos", global.types.pos)
        .value("Annotation", global.types.annotation)
        .value("Note", global.types.note)
        .value("Patch", global.types.patch)
        .function("compile", async move |strand, args, mut out| {
            let ([path, source], [module, prelude, recover, document]) = unpack!(
                strand,
                args,
                2,
                0,
                module = None,
                prelude = None,
                recover = None,
                document = None
            )?;

            let module = module
                .as_ref()
                .map(|m| {
                    m.as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "module: expected `Str`"))
                        .map(|m| m.to_string())
                })
                .transpose()?;

            // SAFETY: the source value is installed in UNIT_SOURCE below. The
            // GC root supplies liveness and this retained pin supplies address stability.
            let backing = match source.view(strand) {
                View::Str(s) => Backing::Str(unsafe { s.pin().into_static_unchecked() }),
                View::Bin(b) => Backing::Bin(unsafe { b.pin().into_static_unchecked() }),
                _ => return Err(Error::type_error(strand, "source: expected `Str` or `Bin`")),
            };

            let path: Box<Path> = Path::new(&path.to_string(strand)?).into();
            // SAFETY: `path` and `module` are heap-backed fields retained after
            // the borrowing compiler unit and dropped after it.
            let static_path: &'static Path = unsafe { mem::transmute(path.as_ref()) };
            let static_module: Option<&'static str> = module
                .as_deref()
                .map(|name| unsafe { mem::transmute(name) });
            let mut config = Config::new();
            config.mode(if let Some(module) = static_module {
                Mode::Module { name: module }
            } else {
                Mode::Script
            });
            config.recover(recover.map(|value| value.to_bool(strand)).unwrap_or(false));
            config.document(document.map(|value| value.to_bool(strand)).unwrap_or(false));

            if let Some(prelude) = prelude {
                apply_prelude_value(strand, &mut config, &prelude)?;
            }

            for ext in config.extensions() {
                ext.apply(&mut config).unwrap();
            }

            let unit = config.unit(static_path, backing.bytes());
            let identity = global.next_unit_id.get();
            global.next_unit_id.set(identity.strict_add(1));
            global.types.unit.create(
                strand,
                UnitObject {
                    unit: Some(unit),
                    _backing: backing,
                    path,
                    _module: module,
                    identity,
                },
                &mut out,
            );
            global
                .types
                .unit
                .cast(&out)
                .unwrap()
                .enter_sync(strand, |strand, unit| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<UNIT_SOURCE>(&mut unit.borrow_mut_unwrap()),
                        source,
                    )
                });
            Ok(())
        })
        .commit();
}
