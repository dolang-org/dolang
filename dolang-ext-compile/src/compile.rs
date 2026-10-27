use std::{
    cell::Cell,
    collections::BTreeMap,
    fmt::Write as _,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem,
    path::Path,
};

use dolang::{
    compile::{self, Config, Diag, Mode},
    extension::CompilerExt,
    runtime::{
        Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, Type, Value,
        object::{Mut, Ref, TypeBuilder},
        unpack,
        value::{Array, AsSym, AsTuple, Dict, Empty, Nil, PinBin, PinStr, TypeObject, View},
        vm::{Register, StateTag},
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
    node: Type<'v, NodeObject<NodeTag>>,
    declaration: Type<'v, NodeObject<DeclarationTag>>,
    import: Type<'v, NodeObject<ImportTag>>,
    param: Type<'v, NodeObject<ParamTag>>,
    block: Type<'v, NodeObject<BlockTag>>,
    reference: Type<'v, NodeObject<ReferenceTag>>,
    binder: Type<'v, NodeObject<BinderTag>>,
    rest_binder: Type<'v, NodeObject<RestBinderTag>>,
    rest_param: Type<'v, NodeObject<RestParamTag>>,
    implicit_param: Type<'v, NodeObject<ImplicitParamTag>>,
    type_expr: Type<'v, NodeObject<TypeExprTag>>,
    type_arg: Type<'v, NodeObject<TypeArgTag>>,
    type_item: Type<'v, NodeObject<TypeItemTag>>,
    concrete_nodes: ConcreteNodeTypes<'v>,
    diagnostic: Type<'v, Diagnostic>,
    span: Type<'v, Span>,
    source_span: Type<'v, SourceSpan>,
    pos: Type<'v, Pos>,
    annotation: Type<'v, Annotation>,
    note: Type<'v, Note>,
    patch: Type<'v, Patch>,
    check: Type<'v, CheckObject>,
    typelib: Type<'v, TypelibObject<'v>>,
}

pub(crate) struct Syms<'v> {
    quant_opt: Sym<'v, 'v>,
    quant_star: Sym<'v, 'v>,
    quant_include: Sym<'v, 'v>,
    token_annotation: Sym<'v, 'v>,
    token_binder: Sym<'v, 'v>,
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
    token_type: Sym<'v, 'v>,
    token_type_key: Sym<'v, 'v>,
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
    alias: Type<'v, NodeObject<AliasTag>>,
    self_param: Type<'v, NodeObject<SelfParamTag>>,
    import_module: Type<'v, NodeObject<ImportModuleTag>>,
    import_item: Type<'v, NodeObject<ImportItemTag>>,
    prelude_module: Type<'v, NodeObject<PreludeModuleTag>>,
    prelude_item: Type<'v, NodeObject<PreludeItemTag>>,
    positional_param: Type<'v, NodeObject<PositionalParamTag>>,
    key_param: Type<'v, NodeObject<KeyParamTag>>,
    mixed_rest_param: Type<'v, NodeObject<MixedRestParamTag>>,
    pos_rest_param: Type<'v, NodeObject<PosRestParamTag>>,
    key_rest_param: Type<'v, NodeObject<KeyRestParamTag>>,
    input_param: Type<'v, NodeObject<InputParamTag>>,
    output_param: Type<'v, NodeObject<OutputParamTag>>,
    lambda: Type<'v, NodeObject<LambdaTag>>,
    if_node: Type<'v, NodeObject<IfTag>>,
    else_node: Type<'v, NodeObject<ElseTag>>,
    arm: Type<'v, NodeObject<ArmTag>>,
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
    type_node: Type<'v, NodeObject<TypeTag>>,
    pos_binder: Type<'v, NodeObject<PosBinderTag>>,
    key_binder: Type<'v, NodeObject<KeyBinderTag>>,
    mixed_rest_binder: Type<'v, NodeObject<MixedRestBinderTag>>,
    pos_rest_binder: Type<'v, NodeObject<PosRestBinderTag>>,
    key_rest_binder: Type<'v, NodeObject<KeyRestBinderTag>>,
    name_type: Type<'v, NodeObject<NameTypeTag>>,
    const_type: Type<'v, NodeObject<ConstTypeTag>>,
    app_type: Type<'v, NodeObject<AppTypeTag>>,
    schema_type: Type<'v, NodeObject<SchemaTypeTag>>,
    parens_type: Type<'v, NodeObject<ParensTypeTag>>,
    array_type: Type<'v, NodeObject<ArrayTypeTag>>,
    union_type: Type<'v, NodeObject<UnionTypeTag>>,
    func_type: Type<'v, NodeObject<FuncTypeTag>>,
    pos_type_arg: Type<'v, NodeObject<PosTypeArgTag>>,
    key_type_arg: Type<'v, NodeObject<KeyTypeArgTag>>,
    expand_type_arg: Type<'v, NodeObject<ExpandTypeArgTag>>,
    pos_type_item: Type<'v, NodeObject<PosTypeItemTag>>,
    key_type_item: Type<'v, NodeObject<KeyTypeItemTag>>,
    any_pos_type_item: Type<'v, NodeObject<AnyPosTypeItemTag>>,
    any_key_type_item: Type<'v, NodeObject<AnyKeyTypeItemTag>>,
    open_type_item: Type<'v, NodeObject<OpenTypeItemTag>>,
}

pub(crate) struct Tag;

impl StateTag for Tag {
    type State<'v> = Global<'v>;
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
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
        let binder = builder
            .build_type::<NodeObject<BinderTag>>((), ())
            .nominal_supertype(declaration)
            .build();
        let rest_param = builder
            .build_type::<NodeObject<RestParamTag>>((), ())
            .nominal_supertype(param)
            .build();
        let implicit_param = builder
            .build_type::<NodeObject<ImplicitParamTag>>((), ())
            .nominal_supertype(param)
            .build();
        let rest_binder = builder
            .build_type::<NodeObject<RestBinderTag>>((), ())
            .nominal_supertype(binder)
            .build();
        macro_rules! subtype {
            ($tag:ty, $base:expr) => {
                builder
                    .build_type::<NodeObject<$tag>>((), ())
                    .nominal_supertype($base)
                    .build()
            };
        }
        let type_expr = subtype!(TypeExprTag, node);
        let type_arg = subtype!(TypeArgTag, node);
        let type_item = subtype!(TypeItemTag, node);
        Self {
            types: Types {
                unit: builder.register_type(),
                diagnostic_iter: builder.register_type(),
                node_iter: builder.register_type(),
                token_iter: builder.register_type(),
                token: builder.register_type(),
                node_id: builder.register_type(),
                node,
                declaration,
                import,
                param,
                block,
                reference,
                binder,
                rest_binder,
                rest_param,
                implicit_param,
                type_expr,
                type_arg,
                type_item,
                concrete_nodes: ConcreteNodeTypes {
                    root: subtype!(RootTag, node),
                    class: subtype!(ClassTag, declaration),
                    function: subtype!(FunctionTag, declaration),
                    method: subtype!(MethodTag, declaration),
                    special_method: subtype!(SpecialMethodTag, declaration),
                    field: subtype!(FieldTag, declaration),
                    bind: subtype!(BindTag, declaration),
                    alias: subtype!(AliasTag, declaration),
                    self_param: subtype!(SelfParamTag, declaration),
                    import_module: subtype!(ImportModuleTag, import),
                    import_item: subtype!(ImportItemTag, import),
                    prelude_module: subtype!(PreludeModuleTag, import),
                    prelude_item: subtype!(PreludeItemTag, import),
                    positional_param: subtype!(PositionalParamTag, param),
                    key_param: subtype!(KeyParamTag, param),
                    mixed_rest_param: subtype!(MixedRestParamTag, rest_param),
                    pos_rest_param: subtype!(PosRestParamTag, rest_param),
                    key_rest_param: subtype!(KeyRestParamTag, rest_param),
                    input_param: subtype!(InputParamTag, implicit_param),
                    output_param: subtype!(OutputParamTag, implicit_param),
                    lambda: subtype!(LambdaTag, block),
                    if_node: subtype!(IfTag, block),
                    else_node: subtype!(ElseTag, block),
                    arm: subtype!(ArmTag, block),
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
                    type_node: subtype!(TypeTag, node),
                    pos_binder: subtype!(PosBinderTag, binder),
                    key_binder: subtype!(KeyBinderTag, binder),
                    mixed_rest_binder: subtype!(MixedRestBinderTag, rest_binder),
                    pos_rest_binder: subtype!(PosRestBinderTag, rest_binder),
                    key_rest_binder: subtype!(KeyRestBinderTag, rest_binder),
                    name_type: subtype!(NameTypeTag, type_expr),
                    const_type: subtype!(ConstTypeTag, type_expr),
                    app_type: subtype!(AppTypeTag, type_expr),
                    schema_type: subtype!(SchemaTypeTag, type_expr),
                    parens_type: subtype!(ParensTypeTag, type_expr),
                    array_type: subtype!(ArrayTypeTag, type_expr),
                    union_type: subtype!(UnionTypeTag, type_expr),
                    func_type: subtype!(FuncTypeTag, type_expr),
                    pos_type_arg: subtype!(PosTypeArgTag, type_arg),
                    key_type_arg: subtype!(KeyTypeArgTag, type_arg),
                    expand_type_arg: subtype!(ExpandTypeArgTag, type_arg),
                    pos_type_item: subtype!(PosTypeItemTag, type_item),
                    key_type_item: subtype!(KeyTypeItemTag, type_item),
                    any_pos_type_item: subtype!(AnyPosTypeItemTag, type_item),
                    any_key_type_item: subtype!(AnyKeyTypeItemTag, type_item),
                    open_type_item: subtype!(OpenTypeItemTag, type_item),
                },
                diagnostic: builder.register_type(),
                span: builder.register_type(),
                source_span: builder.register_type(),
                pos: builder.register_type(),
                annotation: builder.register_type(),
                note: builder.register_type(),
                patch: builder.register_type(),
                check: builder.register_type(),
                typelib: builder.register_type(),
            },
            syms: Syms {
                quant_opt: builder.sym("OPT"),
                quant_star: builder.sym("STAR"),
                quant_include: builder.sym("INCLUDE"),
                token_annotation: builder.sym("ANNOTATION"),
                token_binder: builder.sym("BINDER"),
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
                token_type: builder.sym("TYPE"),
                token_type_key: builder.sym("TYPE_KEY"),
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

pub(crate) struct UnitObject<'v> {
    // Fields are dropped in declaration order: the borrowing unit before its pin.
    unit: Option<compile::Unit<'static>>,
    _backing: PinStr<'v, 'static>,
    path: Box<Path>,
    identity: u64,
}

/// A module's typelib, decoded
pub(crate) struct TypelibObject<'v> {
    // Fields are dropped in declaration order: the borrowing typelib before its pin.
    /// Taken out while a check borrows it
    typelib: Option<compile::typeck::Typelib<'static>>,
    _backing: PinBin<'v, 'static>,
}

pub(crate) struct DiagnosticAnnex<'v> {
    global: State<'v, Global<'v>>,
    diag: Diag,
    #[cfg(feature = "diagnostic-rendering")]
    paths: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct SpanAnnex<'v> {
    global: State<'v, Global<'v>>,
    span: SpanData,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SourceSpanData {
    unit: Option<compile::UnitId>,
    span: SpanData,
}

pub(crate) struct SourceSpanAnnex<'v> {
    global: State<'v, Global<'v>>,
    location: SourceSpanData,
}

#[derive(Clone)]
pub(crate) struct PosAnnex {
    pos: PosData,
}

pub(crate) struct AnnotationAnnex<'v> {
    global: State<'v, Global<'v>>,
    kind: compile::AnnotationKind,
    span: SourceSpanData,
    message: String,
}

pub(crate) struct NoteAnnex<'v> {
    global: State<'v, Global<'v>>,
    kind: compile::NoteKind,
    message: String,
}

pub(crate) struct PatchAnnex<'v> {
    global: State<'v, Global<'v>>,
    span: SourceSpanData,
    message: String,
    sub: String,
}

pub(crate) struct DiagnosticIter {
    index: usize,
}

/// The result of checking units together
pub(crate) struct CheckObject;
pub(crate) struct CheckAnnex {
    validated: bool,
    /// A rendering of the checks the checker couldn't decide
    undecided: String,
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
pub(crate) trait NodeMarker {
    const NAME: &'static str;
}
pub(crate) struct NodeObject<T: NodeMarker> {
    /// The identity of the owning unit, as [`NodeIdAnnex::unit`]
    unit: u64,
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
    AliasTag=>"Alias",
    SelfParamTag=>"SelfParam", ImportModuleTag=>"ImportModule", ImportItemTag=>"ImportItem",
    PreludeModuleTag=>"PreludeModule", PreludeItemTag=>"PreludeItem", PositionalParamTag=>"PositionalParam",
    KeyParamTag=>"KeyParam", RestParamTag=>"RestParam", MixedRestParamTag=>"MixedRestParam",
    PosRestParamTag=>"PosRestParam", KeyRestParamTag=>"KeyRestParam",
    ImplicitParamTag=>"ImplicitParam", InputParamTag=>"InputParam", OutputParamTag=>"OutputParam",
    LambdaTag=>"Lambda", IfTag=>"If", ElseTag=>"Else", ArmTag=>"Arm",
    WhileTag=>"While", ForTag=>"For", TryTag=>"Try", CatchTag=>"Catch", FinallyTag=>"Finally",
    ForElemTag=>"ForElem", IfElemTag=>"IfElem", DecoratorTag=>"Decorator", BreakTag=>"Break",
    ContinueTag=>"Continue", ReturnTag=>"Return", TypeTag=>"Type", BinderTag=>"Binder",
    PosBinderTag=>"PosBinder", KeyBinderTag=>"KeyBinder", RestBinderTag=>"RestBinder",
    MixedRestBinderTag=>"MixedRestBinder", PosRestBinderTag=>"PosRestBinder",
    KeyRestBinderTag=>"KeyRestBinder", TypeExprTag=>"TypeExpr", NameTypeTag=>"NameType",
    ConstTypeTag=>"ConstType", AppTypeTag=>"AppType", SchemaTypeTag=>"SchemaType",
    ParensTypeTag=>"ParensType", ArrayTypeTag=>"ArrayType", UnionTypeTag=>"UnionType",
    FuncTypeTag=>"FuncType", TypeArgTag=>"TypeArg", PosTypeArgTag=>"PosTypeArg",
    KeyTypeArgTag=>"KeyTypeArg", ExpandTypeArgTag=>"ExpandTypeArg", TypeItemTag=>"TypeItem",
    PosTypeItemTag=>"PosTypeItem", KeyTypeItemTag=>"KeyTypeItem",
    AnyPosTypeItemTag=>"AnyPosTypeItem", AnyKeyTypeItemTag=>"AnyKeyTypeItem",
    OpenTypeItemTag=>"OpenTypeItem"
}
pub(crate) struct Diagnostic;
pub(crate) struct Span;
pub(crate) struct SourceSpan;
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

const CHECK_DIAGNOSTICS: usize = 0;

const TYPELIB_BYTES: usize = 0;

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
        compile::Token::Annotation => global.syms.token_annotation,
        compile::Token::Binder => global.syms.token_binder,
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
        compile::Token::Type => global.syms.token_type,
        compile::Token::TypeKey => global.syms.token_type_key,
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

fn source_span_data(location: compile::SourceSpan) -> SourceSpanData {
    SourceSpanData {
        unit: location.unit(),
        span: span_data(location.span()),
    }
}

fn create_source_span<'v>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, '_>,
    location: SourceSpanData,
    out: Slot<'v, '_>,
) {
    global.types.source_span.create_with_annex(
        strand,
        SourceSpan,
        SourceSpanAnnex { global, location },
        out,
    );
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
            span: source_span_data(annotation.span()),
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
            span: source_span_data(patch.span()),
            message: patch.message().to_owned(),
            sub: patch.sub().to_owned(),
        },
        out,
    );
}

/// The sources a diagnostic's locations refer to, for rendering
#[derive(Clone, Copy)]
// The paths are only read for rendering
#[cfg_attr(not(feature = "diagnostic-rendering"), allow(dead_code))]
enum DiagSources<'a, 'v> {
    /// One unit's path and source
    Unit(&'a str, &'a Value<'v>),
    /// The paths of checked units by unit ID, and an array of their sources
    /// that is never modified
    Checked(&'a [String], &'a Value<'v>),
}

fn create_diagnostic<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, 's>,
    sources: DiagSources<'_, 'v>,
    diag: Diag,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    global.types.diagnostic.create_with_annex(
        strand,
        Diagnostic,
        DiagnosticAnnex {
            global,
            diag,
            #[cfg(feature = "diagnostic-rendering")]
            paths: match sources {
                DiagSources::Unit(path, _) => vec![path.to_owned()],
                DiagSources::Checked(paths, _) => paths.to_vec(),
            },
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
                match sources {
                    DiagSources::Unit(..) => Output::set(
                        strand,
                        Mut::slot_mut::<DIAG_SOURCE>(&mut borrow),
                        Empty::Array,
                    ),
                    DiagSources::Checked(_, array) => {
                        Output::set(strand, Mut::slot_mut::<DIAG_SOURCE>(&mut borrow), array)
                    }
                }
            }

            let borrow = inst.borrow(strand)?;
            let annotations = Ref::slot::<DIAG_ANNOTATIONS>(&borrow)
                .as_array(strand)
                .unwrap();
            let notes = Ref::slot::<DIAG_NOTES>(&borrow).as_array(strand).unwrap();
            let patches = Ref::slot::<DIAG_PATCHES>(&borrow).as_array(strand).unwrap();

            if let DiagSources::Unit(_, source) = sources {
                let source_array = Ref::slot::<DIAG_SOURCE>(&borrow).as_array(strand).unwrap();
                source_array.push(strand, source)?;
            }
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
        .state::<Tag>()
        .types
        .unit
        .cast(value)
        .ok_or_else(|| Error::state_error(strand, "invalid unit reference"))?;
    cast.enter_sync(strand, f)
}

/// What `check` takes out of one of its inputs
enum Taken {
    Unit(Box<compile::Unit<'static>>),
    Typelib(Box<compile::typeck::Typelib<'static>>),
}

/// Check an array of units and typelibs together. Each input's compiler unit or
/// typelib is taken out of its object while the checker borrows them all, and put
/// back afterwards.
fn check_units<'v, 's>(
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, 's>,
    units: &Value<'v>,
    out: &mut Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let units = units
        .as_array(strand)
        .expect("units are collected into an array");
    strand.with_slots_sync(
        |strand, [mut item, mut taken_sources, mut sources, mut diagnostics]| {
            Output::set(strand, &mut taken_sources, Empty::Array);
            Output::set(strand, &mut sources, Empty::Array);
            Output::set(strand, &mut diagnostics, Empty::Array);
            let taken_array = taken_sources.as_array(strand).unwrap();
            let sources_array = sources.as_array(strand).unwrap();

            // What is taken from each input, by its position in `units`. Its source
            // is at the same position in `taken_sources`, or nil for a typelib.
            let mut taken = Vec::new();
            let mut outcome = Ok(());
            for index in 0..units.len(strand)? {
                units.get(strand, index, &mut item)?;
                let input = match global.types.typelib.cast(&item) {
                    Some(typelib) => typelib.enter_sync(strand, |strand, typelib| {
                        let Some(inner) = typelib.borrow_mut(strand)?.typelib.take() else {
                            return Err(Error::state_error(strand, "typelib is being checked"));
                        };
                        taken_array.push(strand, Nil)?;
                        Ok(Taken::Typelib(Box::new(inner)))
                    }),
                    None => with_unit(strand, &item, |strand, unit| {
                        let mut borrow = unit.borrow_mut(strand)?;
                        let Some(inner) = borrow.unit.take() else {
                            return Err(Error::state_error(strand, "unit was emitted"));
                        };
                        taken_array.push(strand, Mut::slot::<UNIT_SOURCE>(&borrow))?;
                        Ok(Taken::Unit(Box::new(inner)))
                    }),
                };
                match input {
                    Ok(input) => taken.push((index, input)),
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }

            let checked = outcome.and_then(|()| {
                let mut builder = compile::typeck::Builder::new();
                // The shell's pipelines connect stages with `proc`'s pipes
                builder.pipes(("proc", "PipeSender"), ("proc", "PipeReceiver"));
                // The units the checker accepted, by unit ID
                let mut ids = Vec::new();
                for (position, (_, input)) in taken.iter().enumerate() {
                    let added = match input {
                        Taken::Unit(unit) => builder.unit(unit),
                        Taken::Typelib(typelib) => builder.typelib(typelib),
                    };
                    match added {
                        Ok(id) => {
                            taken_array.get(strand, position, &mut item)?;
                            sources_array.push(strand, &*item)?;
                            ids.push(id);
                        }
                        // Its own diagnostics report it
                        Err(error) if matches!(error.kind(), compile::ErrorKind::Fail) => {}
                        Err(error) => return Err(Error::value(strand, error.to_string())),
                    }
                }
                let check = builder.check();
                let paths: Vec<String> = ids
                    .into_iter()
                    .map(|id| check.path(id).to_string_lossy().into_owned())
                    .collect();
                let diags: Vec<Diag> = check.diagnostics().cloned().collect();
                let undecided = render_undecided(&paths, check.undecided());
                Ok((paths, diags, check.validated(), undecided))
            });

            for (index, input) in taken {
                units.get(strand, index, &mut item)?;
                match input {
                    Taken::Unit(inner) => with_unit(strand, &item, |strand, unit| {
                        unit.borrow_mut(strand)?.unit = Some(*inner);
                        Ok(())
                    })?,
                    Taken::Typelib(inner) => {
                        let typelib = global.types.typelib.cast(&item).unwrap();
                        typelib.enter_sync(strand, |strand, typelib| {
                            typelib.borrow_mut(strand)?.typelib = Some(*inner);
                            Ok::<_, Error<'v, 's>>(())
                        })?
                    }
                }
            }
            let (paths, diags, validated, undecided) = checked?;

            let diagnostics_array = diagnostics.as_array(strand).unwrap();
            for diag in diags {
                create_diagnostic(
                    global,
                    strand,
                    DiagSources::Checked(&paths, &sources),
                    diag,
                    &mut item,
                )?;
                diagnostics_array.push(strand, &*item)?;
            }
            global.types.check.create_with_annex(
                strand,
                CheckObject,
                CheckAnnex {
                    validated,
                    undecided,
                },
                &mut *out,
            );
            global
                .types
                .check
                .cast(&*out)
                .unwrap()
                .enter_sync(strand, |strand, check| {
                    Output::set(
                        strand,
                        Mut::slot_mut::<CHECK_DIAGNOSTICS>(&mut check.borrow_mut_unwrap()),
                        &*diagnostics,
                    );
                });
            Ok(())
        },
    )
}

/// The undecided checks grouped by the kind of reason, each group's locations
/// sorted
fn render_undecided(paths: &[String], undecided: Vec<(String, compile::SourceSpan)>) -> String {
    // Reason names appear in sorted order in the user-facing report.
    let mut kinds: BTreeMap<String, Vec<(&str, u32, u32)>> = BTreeMap::new();
    for (kind, location) in undecided {
        let path = location
            .unit()
            .map_or("<unknown>", |unit| paths[unit.index()].as_str());
        let start = location.span().start();
        kinds
            .entry(kind)
            .or_default()
            .push((path, start.line_number(), start.column_number()));
    }
    let mut out = String::new();
    for (kind, mut locations) in kinds {
        locations.sort_unstable();
        let _ = writeln!(out, "{kind} ({}):", locations.len());
        for (path, line, column) in locations {
            let _ = writeln!(out, "  {path}:{line}:{column}");
        }
    }
    out
}

fn prelude_item_name<'a, 'v, 's>(
    strand: &mut Strand<'v, 's>,
    name: &'a str,
) -> Result<'v, 's, (&'a str, bool)> {
    match name.strip_prefix('@') {
        Some("") => Err(Error::value(
            strand,
            "type-only prelude item name cannot be empty",
        )),
        Some(name) => Ok((name, true)),
        None => Ok((name, false)),
    }
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
        let (item_name, type_only) = prelude_item_name(strand, &item_name)?;
        let builder = config.prelude().import_items(module_name);
        let builder = if type_only {
            builder.type_item(item_name)
        } else {
            builder.item(item_name)
        };
        builder.commit();
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
                let (item_name, type_only) = prelude_item_name(strand, &item_name)?;
                let builder = items_builder.take().unwrap();
                items_builder = Some(if type_only {
                    builder.type_item_with_name(item_name, &bind_name)
                } else {
                    builder.item_with_name(item_name, &bind_name)
                });
            }
            Ok(())
        },
    )?;

    if let Some(builder) = items_builder {
        let _prelude = builder.commit();
    }
    Ok(())
}

/// Applies a Do `compile.Prelude` value without initializing extension state.
pub fn apply_prelude_value<'v, 's>(
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

                    if k.as_int(strand).is_some() {
                        let name = match v.view(strand) {
                            View::Str(name) => name.to_string(),
                            View::Sym(name) => name.as_str(strand).to_owned(),
                            _ => {
                                return Err(Error::type_error(
                                    strand,
                                    "positional prelude entry must be Str or Sym",
                                ));
                            }
                        };
                        config.prelude().import_module(name);
                        continue;
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
                let ty = strand.state::<Tag>().types.diagnostic_iter;
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
                let ty = strand.state::<Tag>().types.node_iter;
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
            .get("root", |this, strand, mut out| {
                let root = {
                    let borrow = this.borrow(strand)?;
                    let Some(unit) = borrow.unit.as_ref() else {
                        return Err(Error::state_error(strand, "unit was emitted"));
                    };
                    unit.root()
                };
                match root {
                    Some(id) => create_node(strand, this, id, &mut out)?,
                    None => Output::set(strand, out, Nil),
                }
                Ok(())
            })
            .method("node", async move |this, strand, args, mut out| {
                let ([id], []) = unpack!(strand, args, 1, 0)?;
                let Some(id_obj) = strand.state::<Tag>().types.node_id.cast(&id) else {
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
                let global = strand.state::<Tag>();
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
            .method("imports", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let imports: Vec<String> = {
                    let borrow = this.borrow(strand)?;
                    let unit = borrow
                        .unit
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "unit was emitted"))?;
                    unit.imports().into_iter().map(str::to_owned).collect()
                };
                let imports: Vec<&str> = imports.iter().map(String::as_str).collect();
                Output::set(strand, out, AsTuple::new(imports));
                Ok(())
            })
            .method("typelib", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let typelib = {
                    let borrow = this.borrow(strand)?;
                    let unit = borrow
                        .unit
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "unit was emitted"))?;
                    compile::typeck::typelib(unit)
                };
                let typelib = typelib.map_err(|err| Error::compile(strand, err))?;
                Output::set(strand, out, typelib.as_slice());
                Ok(())
            })
    }
}

impl<'v> Object<'v> for CheckObject {
    const NAME: &'v str = "Check";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = CheckAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("diagnostics", |this, strand, out| {
                let borrow = this.borrow(strand)?;
                Output::set(strand, out, Ref::slot::<CHECK_DIAGNOSTICS>(&borrow));
                Ok(())
            })
            .get("validated", |this, strand, out| {
                Output::set(strand, out, this.annex().validated);
                Ok(())
            })
            .method("undecided", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Output::set(strand, out, this.annex().undecided.as_str());
                Ok(())
            })
    }
}

/// The typelib of a typelib object, which a running check may have taken
fn with_typelib<'v, 's, R>(
    strand: &mut Strand<'v, 's>,
    this: &Instance<'v, '_, TypelibObject<'v>>,
    f: impl FnOnce(&compile::typeck::Typelib<'static>) -> R,
) -> Result<'v, 's, R> {
    let borrow = this.borrow(strand)?;
    match &borrow.typelib {
        Some(typelib) => Ok(f(typelib)),
        None => Err(Error::state_error(strand, "typelib is being checked")),
    }
}

impl<'v> Object<'v> for TypelibObject<'v> {
    const NAME: &'v str = "Typelib";
    const MODULE: &'v str = "compile";
    const SLOTS: usize = 1;
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let ([bytes], []) = unpack!(strand, args, 1, 0)?;
        let View::Bin(bin) = bytes.view(strand) else {
            return Err(Error::type_error(strand, "bytes: expected `Bin`"));
        };
        // SAFETY: the binary is installed in TYPELIB_BYTES below. The GC root
        // supplies liveness and this retained pin supplies address stability.
        let backing: PinBin<'v, 'static> = unsafe { bin.pin().into_static_unchecked() };
        // SAFETY: the pin is retained after the borrowing typelib and dropped after
        // it.
        let static_bytes: &'static [u8] = unsafe { mem::transmute::<&[u8], _>(&*backing) };
        let typelib = compile::typeck::Typelib::read(static_bytes)
            .map_err(|error| Error::value(strand, error.to_string()))?;
        this.create(
            strand,
            TypelibObject {
                typelib: Some(typelib),
                _backing: backing,
            },
            &mut out,
        );
        this.cast(&out)
            .unwrap()
            .enter_sync(strand, |strand, typelib| {
                Output::set(
                    strand,
                    Mut::slot_mut::<TYPELIB_BYTES>(&mut typelib.borrow_mut_unwrap()),
                    bytes,
                )
            });
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("module", |this, strand, out| {
                let module = with_typelib(strand, &this, |typelib| typelib.module())?;
                Output::set(strand, out, module);
                Ok(())
            })
            .get("path", |this, strand, out| {
                let path = with_typelib(strand, &this, |typelib| {
                    typelib.path().to_string_lossy().into_owned()
                })?;
                Output::set(strand, out, path.as_str());
                Ok(())
            })
            .method("imports", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let imports = with_typelib(strand, &this, |typelib| typelib.imports())?;
                Output::set(strand, out, AsTuple::new(imports));
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
            create_diagnostic(
                strand.state::<Tag>(),
                strand,
                DiagSources::Unit(&path, source),
                diag,
                &mut out,
            )?;
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
                Output::set(strand, &mut out, AsTuple::new([&id_out, &node_out]));
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
        let Some(other) = strand.state::<Tag>().types.node_id.cast(other) else {
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
    strand.state::<Tag>().types.node_id.create_with_annex(
        strand,
        NodeIdObject,
        NodeIdAnnex { unit, id },
        out,
    )
}

fn create_typed_node<'v, T: NodeMarker + 'static>(
    strand: &mut Strand<'v, '_>,
    ty: Type<'v, NodeObject<T>>,
    owner: Instance<'v, '_, UnitObject<'v>>,
    unit: u64,
    id: compile::NodeId,
    out: &mut Slot<'v, '_>,
) {
    ty.create(
        strand,
        NodeObject {
            unit,
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
        Alias,
        SelfParam,
        ImportModule,
        ImportItem,
        PreludeModule,
        PreludeItem,
        PositionalParam,
        KeyParam,
        InputParam,
        OutputParam,
        MixedRestParam,
        PosRestParam,
        KeyRestParam,
        Lambda,
        If,
        Else,
        Arm,
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
        Type,
        PosBinder,
        KeyBinder,
        MixedRestBinder,
        PosRestBinder,
        KeyRestBinder,
        NameType,
        ConstType,
        AppType,
        SchemaType,
        ParensType,
        ArrayType,
        UnionType,
        FuncType,
        PosTypeArg,
        KeyTypeArg,
        ExpandTypeArg,
        PosTypeItem,
        KeyTypeItem,
        AnyPosTypeItem,
        AnyKeyTypeItem,
        OpenTypeItem,
    }
    let (kind, unit) = {
        let b = owner.borrow(strand)?;
        let kind = match b
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
            compile::Kind::Alias { .. } => Which::Alias,
            compile::Kind::SelfParam { .. } => Which::SelfParam,
            compile::Kind::ImportModule { .. } => Which::ImportModule,
            compile::Kind::ImportItem { .. } => Which::ImportItem,
            compile::Kind::PreludeModule { .. } => Which::PreludeModule,
            compile::Kind::PreludeItem { .. } => Which::PreludeItem,
            compile::Kind::PositionalParam { .. } => Which::PositionalParam,
            compile::Kind::KeyParam { .. } => Which::KeyParam,
            compile::Kind::ImplicitParam { kind } => match kind {
                compile::ImplicitKind::In => Which::InputParam,
                compile::ImplicitKind::Out => Which::OutputParam,
                _ => return Err(Error::not_supported(strand)),
            },
            compile::Kind::RestParam { kind, .. } => match kind {
                compile::RestKind::Mixed => Which::MixedRestParam,
                compile::RestKind::Pos => Which::PosRestParam,
                compile::RestKind::Key => Which::KeyRestParam,
                _ => return Err(Error::not_supported(strand)),
            },
            compile::Kind::Lambda => Which::Lambda,
            compile::Kind::If => Which::If,
            compile::Kind::Else => Which::Else,
            compile::Kind::Arm => Which::Arm,
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
            compile::Kind::Type { .. } => Which::Type,
            compile::Kind::Binder { kind, .. } => match kind {
                compile::BinderKind::Pos => Which::PosBinder,
                compile::BinderKind::Key => Which::KeyBinder,
                compile::BinderKind::Rest(compile::RestKind::Mixed) => Which::MixedRestBinder,
                compile::BinderKind::Rest(compile::RestKind::Pos) => Which::PosRestBinder,
                compile::BinderKind::Rest(compile::RestKind::Key) => Which::KeyRestBinder,
                _ => return Err(Error::not_supported(strand)),
            },
            compile::Kind::NameType { .. } => Which::NameType,
            compile::Kind::ConstType(_) => Which::ConstType,
            compile::Kind::AppType { .. } => Which::AppType,
            compile::Kind::SchemaType { .. } => Which::SchemaType,
            compile::Kind::ParensType { .. } => Which::ParensType,
            compile::Kind::ArrayType { .. } => Which::ArrayType,
            compile::Kind::UnionType { .. } => Which::UnionType,
            compile::Kind::FuncType { .. } => Which::FuncType,
            compile::Kind::TypeArg { kind, .. } => match kind {
                compile::TypeArgKind::Pos => Which::PosTypeArg,
                compile::TypeArgKind::Key { .. } => Which::KeyTypeArg,
                compile::TypeArgKind::Expand => Which::ExpandTypeArg,
                _ => return Err(Error::not_supported(strand)),
            },
            compile::Kind::TypeItem { kind, .. } => match kind {
                compile::TypeItemKind::Pos { .. } => Which::PosTypeItem,
                compile::TypeItemKind::Key { .. } => Which::KeyTypeItem,
                compile::TypeItemKind::AnyPos => Which::AnyPosTypeItem,
                compile::TypeItemKind::AnyKey => Which::AnyKeyTypeItem,
                compile::TypeItemKind::Open => Which::OpenTypeItem,
                _ => return Err(Error::not_supported(strand)),
            },
            _ => unreachable!(),
        };
        (kind, b.identity)
    };
    let t = &strand.state::<Tag>().types.concrete_nodes;
    macro_rules! make {
        ($ty:expr,$tag:ty) => {{
            let ty = $ty;
            create_typed_node::<$tag>(strand, ty, owner, unit, id, out)
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
        Which::Alias => make!(t.alias, AliasTag),
        Which::SelfParam => make!(t.self_param, SelfParamTag),
        Which::ImportModule => make!(t.import_module, ImportModuleTag),
        Which::ImportItem => make!(t.import_item, ImportItemTag),
        Which::PreludeModule => make!(t.prelude_module, PreludeModuleTag),
        Which::PreludeItem => make!(t.prelude_item, PreludeItemTag),
        Which::PositionalParam => make!(t.positional_param, PositionalParamTag),
        Which::KeyParam => make!(t.key_param, KeyParamTag),
        Which::InputParam => make!(t.input_param, InputParamTag),
        Which::OutputParam => make!(t.output_param, OutputParamTag),
        Which::MixedRestParam => make!(t.mixed_rest_param, MixedRestParamTag),
        Which::PosRestParam => make!(t.pos_rest_param, PosRestParamTag),
        Which::KeyRestParam => make!(t.key_rest_param, KeyRestParamTag),
        Which::Lambda => make!(t.lambda, LambdaTag),
        Which::If => make!(t.if_node, IfTag),
        Which::Else => make!(t.else_node, ElseTag),
        Which::Arm => make!(t.arm, ArmTag),
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
        Which::Type => make!(t.type_node, TypeTag),
        Which::PosBinder => make!(t.pos_binder, PosBinderTag),
        Which::KeyBinder => make!(t.key_binder, KeyBinderTag),
        Which::MixedRestBinder => make!(t.mixed_rest_binder, MixedRestBinderTag),
        Which::PosRestBinder => make!(t.pos_rest_binder, PosRestBinderTag),
        Which::KeyRestBinder => make!(t.key_rest_binder, KeyRestBinderTag),
        Which::NameType => make!(t.name_type, NameTypeTag),
        Which::ConstType => make!(t.const_type, ConstTypeTag),
        Which::AppType => make!(t.app_type, AppTypeTag),
        Which::SchemaType => make!(t.schema_type, SchemaTypeTag),
        Which::ParensType => make!(t.parens_type, ParensTypeTag),
        Which::ArrayType => make!(t.array_type, ArrayTypeTag),
        Which::UnionType => make!(t.union_type, UnionTypeTag),
        Which::FuncType => make!(t.func_type, FuncTypeTag),
        Which::PosTypeArg => make!(t.pos_type_arg, PosTypeArgTag),
        Which::KeyTypeArg => make!(t.key_type_arg, KeyTypeArgTag),
        Which::ExpandTypeArg => make!(t.expand_type_arg, ExpandTypeArgTag),
        Which::PosTypeItem => make!(t.pos_type_item, PosTypeItemTag),
        Which::KeyTypeItem => make!(t.key_type_item, KeyTypeItemTag),
        Which::AnyPosTypeItem => make!(t.any_pos_type_item, AnyPosTypeItemTag),
        Which::AnyKeyTypeItem => make!(t.any_key_type_item, AnyKeyTypeItemTag),
        Which::OpenTypeItem => make!(t.open_type_item, OpenTypeItemTag),
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
    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        // A unit projects a node to one class, so a node of another class is
        // another node.
        let Some(other) = this.ty(strand.vm()).cast(other) else {
            return Ok(false);
        };
        let this = this.borrow(strand)?;
        let key = (this.unit, this.id);
        let other_key = other.enter_sync(strand, |strand, other| {
            other.borrow(strand).map(|other| (other.unit, other.id))
        })?;
        Ok(key == other_key)
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        h: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        let this = this.borrow(strand)?;
        (this.unit, this.id).hash(h);
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder.get("id", |this, strand, mut out| {
            let (unit, id) = {
                let b = this.borrow(strand)?;
                (b.unit, b.id)
            };
            create_node_id(strand, unit, id, &mut out);
            Ok(())
        });
        builder = builder.get("children", |this, strand, out| {
            let ids: Vec<_> = with_node(this, strand, |n, _| n.children().collect())?;
            project_nodes(this, strand, ids, out)
        });
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
            let span = with_node(this, strand, |n, _| n.span().map(span_data))?;
            if let Some(span) = span {
                create_span(strand.state::<Tag>(), strand, span, out)
            } else {
                Output::set(strand, out, Nil)
            };
            Ok(())
        });
        // Every node answers, rather than only the kinds that can be
        // documented: which of them those are is the compiler's determination,
        // and a consumer that has to know it in advance gains nothing.
        builder = builder.get("doc", |this, strand, out| {
            let doc = with_node(this, strand, |n, _| n.doc().map(span_data))?;
            if let Some(doc) = doc {
                create_span(strand.state::<Tag>(), strand, doc, out)
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
                | "Alias"
                | "SelfParam"
                | "ImportModule"
                | "ImportItem"
                | "PreludeModule"
                | "PreludeItem"
                | "PositionalParam"
                | "KeyParam"
                | "MixedRestParam"
                | "PosRestParam"
                | "KeyRestParam"
                | "InputParam"
                | "OutputParam"
                | "PosBinder"
                | "KeyBinder"
                | "MixedRestBinder"
                | "PosRestBinder"
                | "KeyRestBinder"
        ) {
            builder = builder.get("name", |this, strand, out| project_name(this, strand, out));
        }
        if matches!(
            T::NAME,
            "Class"
                | "Function"
                | "Method"
                | "Field"
                | "Bind"
                | "Alias"
                | "ImportModule"
                | "ImportItem"
        ) {
            builder = builder.get("is_pub", |this, strand, out| project_pub(this, strand, out));
        }
        if matches!(T::NAME, "PositionalParam" | "KeyParam") {
            builder = builder.get("default", |this, strand, out| {
                project_default(this, strand, out)
            });
        }
        if matches!(T::NAME, "MixedRestParam" | "PosRestParam" | "KeyRestParam") {
            builder = builder.get("type_ellipsis", |this, strand, out| {
                let span = with_node(this, strand, |n, _| match n.kind() {
                    compile::Kind::RestParam { type_ellipsis, .. } => type_ellipsis.map(span_data),
                    _ => None,
                })?;
                if let Some(span) = span {
                    create_span(strand.state::<Tag>(), strand, span, out)
                } else {
                    Output::set(strand, out, Nil)
                };
                Ok(())
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
        if matches!(
            T::NAME,
            "Class"
                | "Function"
                | "Method"
                | "SpecialMethod"
                | "ImportModule"
                | "ImportItem"
                | "PreludeItem"
                | "Type"
        ) {
            builder = builder.get("type_only", |this, strand, out| {
                project_type_only(this, strand, out)
            });
        }
        if T::NAME == "Alias" {
            builder = builder.get("opaque", |this, strand, out| {
                let v = with_node(this, strand, |n, _| match n.kind() {
                    compile::Kind::Alias { opaque, .. } => opaque,
                    _ => unreachable!(),
                })?;
                Output::set(strand, out, v);
                Ok(())
            });
        }
        // A getter projecting the node a field of the kind refers to, or `nil`
        macro_rules! node_field {
            ($pat:pat => $id:expr) => {
                |this, strand, out| {
                    let id = with_node(this, strand, |n, _| match n.kind() {
                        $pat => $id,
                        _ => unreachable!(),
                    })?;
                    project_nodes(this, strand, id, out)
                }
            };
        }
        // A getter projecting the nodes a field of the kind lists, as an array
        macro_rules! node_list {
            ($pat:pat => $ids:expr) => {
                |this, strand, out| {
                    let ids: Vec<_> = with_node(this, strand, |n, _| match n.kind() {
                        $pat => $ids.collect(),
                        _ => unreachable!(),
                    })?;
                    project_nodes(this, strand, ids, out)
                }
            };
        }
        if T::NAME == "Type" {
            builder = builder.get(
                "expr",
                node_field!(compile::Kind::Type { expr, .. } => Some(expr)),
            );
        }
        if matches!(
            T::NAME,
            "PosBinder" | "KeyBinder" | "MixedRestBinder" | "PosRestBinder" | "KeyRestBinder"
        ) {
            builder = builder
                .get(
                    "bound",
                    node_field!(compile::Kind::Binder { bound, .. } => bound),
                )
                .get(
                    "default",
                    node_field!(compile::Kind::Binder { default, .. } => default),
                );
        }
        if matches!(T::NAME, "PosTypeArg" | "KeyTypeArg" | "ExpandTypeArg") {
            builder = builder.get(
                "ty",
                node_field!(compile::Kind::TypeArg { ty, .. } => Some(ty)),
            );
        }
        if matches!(T::NAME, "PosTypeItem" | "KeyTypeItem") {
            builder = builder.get("quant", |this, strand, out| {
                let quant = with_node(this, strand, |n, _| match n.kind() {
                    compile::Kind::TypeItem {
                        kind:
                            compile::TypeItemKind::Pos { quant }
                            | compile::TypeItemKind::Key { quant, .. },
                        ..
                    } => quant,
                    _ => unreachable!(),
                })?;
                let syms = &strand.state::<Tag>().syms;
                match quant {
                    Some(compile::TypeQuant::Opt) => Output::set(strand, out, syms.quant_opt),
                    Some(compile::TypeQuant::Star) => Output::set(strand, out, syms.quant_star),
                    Some(compile::TypeQuant::Include) => {
                        Output::set(strand, out, syms.quant_include)
                    }
                    _ => Output::set(strand, out, Nil),
                }
                Ok(())
            });
        }
        if matches!(T::NAME, "PosTypeItem" | "KeyTypeItem" | "AnyKeyTypeItem") {
            builder = builder.get("ty", node_field!(compile::Kind::TypeItem { ty, .. } => ty));
        }
        match T::NAME {
            "NameType" => builder
                .get("head", |this, strand, out| {
                    let head = with_node(this, strand, |n, _| match n.kind() {
                        compile::Kind::NameType { head, .. } => span_data(head),
                        _ => unreachable!(),
                    })?;
                    create_span(strand.state::<Tag>(), strand, head, out);
                    Ok(())
                })
                .get("target", |this, strand, mut out| {
                    let (target, unit) = with_node(this, strand, |n, unit| match n.kind() {
                        compile::Kind::NameType { target, .. } => (target, unit),
                        _ => unreachable!(),
                    })?;
                    match target {
                        Some(target) => create_node_id(strand, unit, target, &mut out),
                        None => Output::set(strand, out, Nil),
                    }
                    Ok(())
                }),
            "ConstType" => builder.get("value", |this, strand, out| {
                enum Value {
                    Sym(String),
                    Str(String),
                    Int(i128),
                    Bool(bool),
                    Nil,
                }
                let value = with_node(this, strand, |n, _| match n.kind() {
                    compile::Kind::ConstType(value) => Some(match value {
                        compile::TypeConst::Sym(name) => Value::Sym(name.to_owned()),
                        compile::TypeConst::Str(value) => Value::Str(value.to_owned()),
                        compile::TypeConst::Int(value) => Value::Int(value),
                        compile::TypeConst::Bool(value) => Value::Bool(value),
                        compile::TypeConst::Nil => Value::Nil,
                        _ => return None,
                    }),
                    _ => unreachable!(),
                })?;
                match value.ok_or_else(|| Error::not_supported(strand))? {
                    Value::Sym(name) => Output::set(strand, out, AsSym::new(&name)),
                    Value::Str(value) => Output::set(strand, out, value.as_str()),
                    Value::Int(value) => {
                        let value =
                            i64::try_from(value).map_err(|_| Error::not_supported(strand))?;
                        Output::set(strand, out, value)
                    }
                    Value::Bool(value) => Output::set(strand, out, value),
                    Value::Nil => Output::set(strand, out, Nil),
                }
                Ok(())
            }),
            "AppType" => builder
                .get(
                    "base",
                    node_field!(compile::Kind::AppType { base, .. } => Some(base)),
                )
                .get(
                    "args",
                    node_list!(compile::Kind::AppType { args, .. } => args),
                ),
            "SchemaType" => builder.get(
                "items",
                node_list!(compile::Kind::SchemaType { items } => items),
            ),
            "ParensType" => builder.get(
                "items",
                node_list!(compile::Kind::ParensType { items } => items),
            ),
            "ArrayType" => builder.get(
                "elem",
                node_field!(compile::Kind::ArrayType { elem } => Some(elem)),
            ),
            "UnionType" => builder.get(
                "members",
                node_list!(compile::Kind::UnionType { members } => members),
            ),
            "FuncType" => builder
                .get(
                    "items",
                    node_list!(compile::Kind::FuncType { items, .. } => items),
                )
                .get(
                    "input",
                    node_field!(compile::Kind::FuncType { input, .. } => input),
                )
                .get(
                    "output",
                    node_field!(compile::Kind::FuncType { output, .. } => output),
                )
                .get(
                    "ret",
                    node_field!(compile::Kind::FuncType { ret, .. } => Some(ret)),
                ),
            "KeyTypeArg" => builder.get("name", |this, strand, out| {
                let name = with_node(this, strand, |n, _| match n.kind() {
                    compile::Kind::TypeArg {
                        kind: compile::TypeArgKind::Key { name },
                        ..
                    } => span_data(name),
                    _ => unreachable!(),
                })?;
                create_span(strand.state::<Tag>(), strand, name, out);
                Ok(())
            }),
            "KeyTypeItem" => builder
                .get("key", |this, strand, out| {
                    let key = with_node(this, strand, |n, _| match n.kind() {
                        compile::Kind::TypeItem {
                            kind: compile::TypeItemKind::Key { key, .. },
                            ..
                        } => span_data(key),
                        _ => unreachable!(),
                    })?;
                    create_span(strand.state::<Tag>(), strand, key, out);
                    Ok(())
                })
                .get(
                    "key_ty",
                    node_field!(compile::Kind::TypeItem {
                        kind: compile::TypeItemKind::Key { key_ty, .. },
                        ..
                    } => key_ty),
                ),
            _ => builder,
        }
    }
}

/// Project nodes of `this`'s unit: one node or `nil` from an `Option`, or an array
/// from a `Vec`.
fn project_nodes<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    ids: impl ProjectNodes,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let b = this.borrow(strand)?;
    with_unit(strand, Ref::slot::<OWNER>(&b), |strand, owner| {
        ids.project(strand, owner, &mut out)
    })
}

trait ProjectNodes {
    fn project<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        owner: Instance<'v, '_, UnitObject<'v>>,
        out: &mut Slot<'v, '_>,
    ) -> Result<'v, 's, ()>;
}

impl ProjectNodes for Option<compile::NodeId> {
    fn project<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        owner: Instance<'v, '_, UnitObject<'v>>,
        out: &mut Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        match self {
            Some(id) => create_node(strand, owner, id, out),
            None => {
                Output::set(strand, out, Nil);
                Ok(())
            }
        }
    }
}

impl ProjectNodes for Vec<compile::NodeId> {
    fn project<'v, 's>(
        self,
        strand: &mut Strand<'v, 's>,
        owner: Instance<'v, '_, UnitObject<'v>>,
        out: &mut Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, &mut *out, Empty::Array);
        strand.with_slots_sync(|strand, [mut item]| {
            let array = out.as_array(strand).unwrap();
            for id in self {
                create_node(strand, owner, id, &mut item)?;
                array.push(strand, &mut item)?;
            }
            Ok(())
        })
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
        | compile::Kind::SpecialMethod { name, .. }
        | compile::Kind::Field { name, .. }
        | compile::Kind::Bind { name, .. }
        | compile::Kind::Alias { name, .. }
        | compile::Kind::SelfParam { name }
        | compile::Kind::ImportModule { name, .. }
        | compile::Kind::ImportItem { name, .. }
        | compile::Kind::PositionalParam { name, .. }
        | compile::Kind::KeyParam { name, .. }
        | compile::Kind::Binder { name, .. } => Name::Span(span_data(name)),
        compile::Kind::RestParam { name, .. } => {
            name.map_or(Name::None, |v| Name::Span(span_data(v)))
        }
        // An implicit names an ambient channel rather than binding anything
        compile::Kind::ImplicitParam { .. } => Name::None,
        compile::Kind::PreludeModule { name, .. } | compile::Kind::PreludeItem { name, .. } => {
            Name::Text(name.to_owned())
        }
        _ => unreachable!(),
    })?;
    match name {
        Name::Span(s) => create_span(strand.state::<Tag>(), strand, s, out),
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
        | compile::Kind::Alias { is_pub, .. }
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
        create_span(strand.state::<Tag>(), strand, v, out)
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
    create_span(strand.state::<Tag>(), strand, v, out);
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
        V::S(v) => create_span(strand.state::<Tag>(), strand, v, out),
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
fn project_type_only<'v, 's, T: NodeMarker + 'static>(
    this: Instance<'v, '_, NodeObject<T>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let v = with_node(this, strand, |n, _| match n.kind() {
        compile::Kind::Class { type_only, .. }
        | compile::Kind::Function { type_only, .. }
        | compile::Kind::Method { type_only, .. }
        | compile::Kind::SpecialMethod { type_only, .. }
        | compile::Kind::ImportModule { type_only, .. }
        | compile::Kind::ImportItem { type_only, .. }
        | compile::Kind::PreludeItem { type_only, .. }
        | compile::Kind::Type { type_only, .. } => type_only,
        _ => unreachable!(),
    })?;
    Output::set(strand, out, v);
    Ok(())
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
                        let array = Ref::slot::<DIAG_SOURCE>(&borrow).as_array(strand).unwrap();
                        let annex = this.annex();
                        let backings = strand.with_slots_sync(|strand, [mut item]| {
                            let mut backings = Vec::with_capacity(annex.paths.len());
                            for index in 0..annex.paths.len() {
                                array.get(strand, index, &mut item)?;
                                // SAFETY: the sources array is rooted in this diagnostic's
                                // DIAG_SOURCE slot and never modified after creation, so each
                                // source outlives its pin, which is dropped at the end of
                                // this method.
                                backings.push(match item.view(strand) {
                                    View::Str(s) => {
                                        Some(unsafe { s.pin().into_static_unchecked() })
                                    }
                                    // A unit checked from its typelib
                                    View::Nil => None,
                                    _ => {
                                        return Err(Error::type_error(
                                            strand,
                                            "source: expected `Str`",
                                        ));
                                    }
                                });
                            }
                            Ok(backings)
                        })?;
                        let texts: Vec<_> = backings.iter().map(Option::as_deref).collect();
                        let paths: Vec<_> = annex.paths.iter().map(String::as_str).collect();
                        crate::render::render_diag(
                            &paths,
                            &texts,
                            &annex.diag,
                            crate::render::ColorMode::Always,
                        )
                        .map_err(|_| {
                            Error::value(strand, "diagnostic refers to an unknown source")
                        })?
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
                create_source_span(
                    this.annex().global,
                    strand,
                    source_span_data(this.annex().diag.span()),
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

impl<'v> Object<'v> for SourceSpan {
    const NAME: &'v str = "SourceSpan";
    const MODULE: &'v str = "compile";
    type Annex = SourceSpanAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("unit", |this, strand, out| {
                if let Some(unit) = this.annex().location.unit {
                    Output::set(strand, out, unit.index());
                } else {
                    Output::set(strand, out, Nil);
                }
                Ok(())
            })
            .get("span", |this, strand, out| {
                create_span(
                    this.annex().global,
                    strand,
                    this.annex().location.span.clone(),
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
        if let Some(other) = this.annex().global.types.source_span.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().location == other.annex().location
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
        this.annex().location.hash(hasher);
        Ok(())
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        if let Some(other) = this.annex().global.types.source_span.cast(other) {
            Ok(other.enter_sync(strand, |_strand, other| {
                this.annex().location < other.annex().location
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
        if let Some(other) = strand.state::<Tag>().types.pos.cast(other) {
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
        if let Some(other) = strand.state::<Tag>().types.pos.cast(other) {
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
                create_source_span(this.annex().global, strand, this.annex().span.clone(), out);
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
                create_source_span(this.annex().global, strand, this.annex().span.clone(), out);
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

pub(crate) fn configure<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    let module = builder.sym("module");
    let prelude = builder.sym("prelude");
    let recover = builder.sym("recover");
    let document = builder.sym("document");
    let typecheck = builder.sym("typecheck");
    let strict = builder.sym("strict");

    builder
        .module("compile")
        .value("Unit", global.types.unit)
        .value("NodeId", global.types.node_id)
        .value("Token", global.types.token)
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
        .value("Alias", global.types.concrete_nodes.alias)
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
        .value("RestParam", global.types.rest_param)
        .value("ImplicitParam", global.types.implicit_param)
        .value("InputParam", global.types.concrete_nodes.input_param)
        .value("OutputParam", global.types.concrete_nodes.output_param)
        .value(
            "MixedRestParam",
            global.types.concrete_nodes.mixed_rest_param,
        )
        .value("PosRestParam", global.types.concrete_nodes.pos_rest_param)
        .value("KeyRestParam", global.types.concrete_nodes.key_rest_param)
        .value("Lambda", global.types.concrete_nodes.lambda)
        .value("If", global.types.concrete_nodes.if_node)
        .value("Else", global.types.concrete_nodes.else_node)
        .value("Arm", global.types.concrete_nodes.arm)
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
        .value("Type", global.types.concrete_nodes.type_node)
        .value("Binder", global.types.binder)
        .value("PosBinder", global.types.concrete_nodes.pos_binder)
        .value("KeyBinder", global.types.concrete_nodes.key_binder)
        .value("RestBinder", global.types.rest_binder)
        .value(
            "MixedRestBinder",
            global.types.concrete_nodes.mixed_rest_binder,
        )
        .value("PosRestBinder", global.types.concrete_nodes.pos_rest_binder)
        .value("KeyRestBinder", global.types.concrete_nodes.key_rest_binder)
        .value("TypeExpr", global.types.type_expr)
        .value("NameType", global.types.concrete_nodes.name_type)
        .value("ConstType", global.types.concrete_nodes.const_type)
        .value("AppType", global.types.concrete_nodes.app_type)
        .value("SchemaType", global.types.concrete_nodes.schema_type)
        .value("ParensType", global.types.concrete_nodes.parens_type)
        .value("ArrayType", global.types.concrete_nodes.array_type)
        .value("UnionType", global.types.concrete_nodes.union_type)
        .value("FuncType", global.types.concrete_nodes.func_type)
        .value("TypeArg", global.types.type_arg)
        .value("PosTypeArg", global.types.concrete_nodes.pos_type_arg)
        .value("KeyTypeArg", global.types.concrete_nodes.key_type_arg)
        .value("ExpandTypeArg", global.types.concrete_nodes.expand_type_arg)
        .value("TypeItem", global.types.type_item)
        .value("PosTypeItem", global.types.concrete_nodes.pos_type_item)
        .value("KeyTypeItem", global.types.concrete_nodes.key_type_item)
        .value(
            "AnyPosTypeItem",
            global.types.concrete_nodes.any_pos_type_item,
        )
        .value(
            "AnyKeyTypeItem",
            global.types.concrete_nodes.any_key_type_item,
        )
        .value("OpenTypeItem", global.types.concrete_nodes.open_type_item)
        .value("Diagnostic", global.types.diagnostic)
        .value("Span", global.types.span)
        .value("SourceSpan", global.types.source_span)
        .value("Pos", global.types.pos)
        .value("Annotation", global.types.annotation)
        .value("Note", global.types.note)
        .value("Patch", global.types.patch)
        .value("Check", global.types.check)
        .value("Typelib", global.types.typelib)
        .function_with_slots(
            "check",
            async move |strand, args, mut out, [mut iter, mut item, mut units]| {
                let ([iterable], []) = unpack!(strand, args, 1, 0)?;
                Output::set(strand, &mut units, Empty::Array);
                iterable.iter(strand, &mut iter).await?;
                while iter.next(strand, &mut item).await? {
                    units.as_array(strand).unwrap().push(strand, &*item)?;
                }
                check_units(global, strand, &units, &mut out)
            },
        )
        .function("compile", async move |strand, args, mut out| {
            let ([path, source], [module, prelude, recover, document, typecheck, strict]) = unpack!(
                strand,
                args,
                2,
                0,
                module = None,
                prelude = None,
                recover = None,
                document = None,
                typecheck = None,
                strict = None
            )?;

            let module = module
                .as_ref()
                .filter(|m| !m.is_nil())
                .map(|m| {
                    m.as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "module: expected `Str`"))
                        .map(|m| m.to_string())
                })
                .transpose()?;

            // SAFETY: the source value is installed in UNIT_SOURCE below. The
            // GC root supplies liveness and this retained pin supplies address stability.
            let View::Str(text) = source.view(strand) else {
                return Err(Error::type_error(strand, "source: expected `Str`"));
            };
            let backing: PinStr<'v, 'static> = unsafe { text.pin().into_static_unchecked() };
            // SAFETY: the pin is retained after the borrowing unit and dropped after it.
            let text: &'static str = unsafe { mem::transmute::<&str, _>(&*backing) };

            let path: Box<Path> = Path::new(&path.to_string(strand)?).into();
            let mut config = Config::new();
            config.mode(if let Some(module) = module {
                Mode::Module {
                    name: module.into(),
                }
            } else {
                Mode::Script
            });
            config.recover(
                recover
                    .map(|value| value.to_bool(strand))
                    .transpose()?
                    .unwrap_or(false),
            );
            config.document(
                document
                    .map(|value| value.to_bool(strand))
                    .transpose()?
                    .unwrap_or(false),
            );
            config.typecheck(
                typecheck
                    .map(|value| value.to_bool(strand))
                    .transpose()?
                    .unwrap_or(false),
            );
            if let Some(strict) = strict.filter(|strict| !strict.is_nil()) {
                config.strict(strict.to_bool(strand)?);
            }

            if let Some(prelude) = prelude {
                apply_prelude_value(strand, &mut config, &prelude)?;
            }

            for ext in config.extensions() {
                ext.apply(&mut config).unwrap();
            }

            let unit = config.unit(&path, text);
            let identity = global.next_unit_id.get();
            global.next_unit_id.set(identity.strict_add(1));
            global.types.unit.create(
                strand,
                UnitObject {
                    unit: Some(unit),
                    _backing: backing,
                    path,
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
