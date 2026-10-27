use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    iter, mem,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, RwLock, Weak, mpsc},
    thread,
    time::{Duration, Instant},
};

use futures::future::{FutureExt, Shared};
use tokio::sync::{
    Mutex as AsyncMutex,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    oneshot,
};
use tower_lsp_server::{
    Client, ClientSocket, LanguageServer, LspService, jsonrpc::Result, ls_types::*,
};

use dolang_compile::{
    Config as CompileConfig, Context, ErrorKind, Kind, Mode, Node, NodeId, Token, Unit, UnitId,
    diag, source,
    typeck::{self, Role, Style},
};

use crate::{
    doc_index,
    lookup::{self, Modules, Target},
    settings::{CONFIG_FILE_NAME, Import},
    signature,
    workspace::{Check, Member, Round, Workspace, Workspaces},
};

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DEFAULT_LIBRARY,
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEFINITION,
    SemanticTokenModifier::STATIC,
];
/// How long a document's edits must settle before it is type checked
const CHECK_DELAY: Duration = Duration::from_millis(300);
/// The most module sources one check compiles from the module path
const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::STRING,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::NUMBER,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::COMMENT,
    SemanticTokenType::CLASS,
    SemanticTokenType::TYPE,
    SemanticTokenType::TYPE_PARAMETER,
];

const TT_CONSTANT: u32 = 0;
const TT_OPERATOR: u32 = 1;
const TT_STRING: u32 = 2;
const TT_PROPERTY: u32 = 3;
const TT_FUNCTION: u32 = 4;
const TT_KEYWORD: u32 = 5;
const TT_NUMBER: u32 = 6;
const TT_PARAMETER: u32 = 7;
const TT_VARIABLE: u32 = 8;
const TT_NAMESPACE: u32 = 9;
const TT_COMMENT: u32 = 10;
const TT_CLASS: u32 = 11;
const TT_TYPE: u32 = 12;
const TT_TYPE_PARAMETER: u32 = 13;

const MOD_PRELUDE: u32 = 1 << 0;
const MOD_DECLARATION: u32 = 1 << 1;
const MOD_DEFINITION: u32 = 1 << 2;
const MOD_STATIC: u32 = 1 << 3;

fn classify_token(token: Token, kind: Option<&Kind<'_>>, context: Context) -> (u32, u32) {
    match token {
        Token::Annotation => (TT_OPERATOR, 0),
        Token::Binder => (TT_TYPE_PARAMETER, 0),
        Token::Comment => (TT_COMMENT, 0),
        Token::Constant => (TT_CONSTANT, 0),
        Token::Delim | Token::Sigil => (TT_OPERATOR, 0),
        Token::Escape => (TT_STRING, 0),
        Token::Field => match context {
            Context::Call => (TT_FUNCTION, 0),
            Context::None => (TT_PROPERTY, 0),
        },
        Token::Method => (TT_FUNCTION, 0),
        Token::Key => (TT_PROPERTY, 0),
        Token::ModuleName => (TT_NAMESPACE, 0),
        Token::ModuleItem => (TT_PROPERTY, 0),
        Token::Keyword => (TT_KEYWORD, 0),
        Token::Literal => (TT_STRING, 0),
        Token::Number => (TT_NUMBER, 0),
        Token::Operator => (TT_OPERATOR, 0),
        Token::StringDelim => (TT_STRING, 0),
        Token::Type => match kind {
            Some(Kind::PreludeItem {
                type_only: true, ..
            }) => (TT_TYPE, MOD_PRELUDE),
            Some(Kind::Binder { .. }) => (TT_TYPE_PARAMETER, 0),
            _ => (TT_TYPE, 0),
        },
        Token::TypeKey => (TT_PROPERTY, 0),
        Token::Variable => match (context, kind) {
            (
                _,
                Some(
                    Kind::Class { .. }
                    | Kind::Binder { .. }
                    | Kind::ImportItem {
                        type_only: true, ..
                    },
                ),
            ) => (TT_CLASS, 0),
            (Context::Call, Some(Kind::PreludeItem { .. })) => (TT_FUNCTION, MOD_PRELUDE),
            (Context::Call, Some(Kind::PreludeModule { .. })) => (TT_FUNCTION, MOD_PRELUDE),
            (Context::Call, _) => (TT_FUNCTION, 0),
            (
                Context::None,
                Some(
                    Kind::PositionalParam { .. }
                    | Kind::KeyParam { .. }
                    | Kind::RestParam { .. }
                    | Kind::SelfParam { .. },
                ),
            ) => (TT_PARAMETER, 0),
            (
                Context::None,
                Some(Kind::Function { .. } | Kind::Method { .. } | Kind::SpecialMethod { .. }),
            ) => (TT_FUNCTION, 0),
            (Context::None, Some(Kind::PreludeItem { .. })) => (TT_VARIABLE, MOD_PRELUDE),
            (Context::None, Some(Kind::PreludeModule { .. })) => (TT_NAMESPACE, MOD_PRELUDE),
            (Context::None, Some(Kind::ImportModule { .. })) => (TT_NAMESPACE, 0),
            (Context::None, _) => (TT_VARIABLE, 0),
        },
    }
}

fn same_span(a: &diag::Span, b: &diag::Span) -> bool {
    a.start().byte_offset() == b.start().byte_offset()
        && a.end().byte_offset() == b.end().byte_offset()
}

/// Modifier bits a token earns from the declaration it names.
///
/// A declaration is its own definition in Do, so a token standing where the
/// name was declared claims both; `statics` holds the fields a `#[class]` or
/// `#[static]` decorator scoped to the class.
fn declaration_modifiers(
    span: &diag::Span,
    def: &diag::Span,
    id: NodeId,
    statics: &HashSet<NodeId>,
) -> u32 {
    let mut modifiers = 0;
    if same_span(span, def) {
        modifiers |= MOD_DECLARATION | MOD_DEFINITION;
    }
    if statics.contains(&id) {
        modifiers |= MOD_STATIC;
    }
    modifiers
}

/// The fields a `#[class]` or `#[static]` decorator gives class scope.
///
/// This is the test the elaborator itself applies: the decorator must resolve
/// to the prelude binding rather than to anything else spelled `class`, so the
/// server agrees with the compiler by identity instead of by source text.
fn static_fields(unit: &Unit<'_>) -> HashSet<NodeId> {
    unit.nodes()
        .filter_map(|(_, node)| match node.kind() {
            Kind::Decorator {
                target: Some(target),
            } => Some((node.parent()?, target)),
            _ => None,
        })
        .filter(|(_, target)| {
            matches!(
                unit.node(*target).map(|node| node.kind()),
                Some(Kind::PreludeItem {
                    module: "std",
                    item: "class" | "static",
                    ..
                })
            )
        })
        .map(|(field, _)| field)
        .collect()
}

/// The outline entry a node becomes, or `None` if it is not one.
///
/// The structural kinds are containment, not names: an `if` has nothing to put
/// in an outline, so it is skipped and whatever it holds reparents onto the
/// nearest declaration around it.  Prelude bindings are skipped for the
/// opposite reason — they are real declarations, but they have no source text
/// in this document to point at.  A binding spelled `_` is skipped because it
/// is how a result is discarded: it declares nothing anyone navigates to.
fn symbol_kind(kind: &Kind<'_>, content: &str) -> Option<SymbolKind> {
    Some(match kind {
        Kind::Class { .. } => SymbolKind::CLASS,
        Kind::Function { .. } => SymbolKind::FUNCTION,
        Kind::Method { .. } => SymbolKind::METHOD,
        Kind::SpecialMethod { name, .. } => {
            if span_text(content, name) == "(init)" {
                SymbolKind::CONSTRUCTOR
            } else {
                SymbolKind::METHOD
            }
        }
        Kind::Field { .. } => SymbolKind::FIELD,
        Kind::Bind { name, .. } if span_text(content, name) == "_" => return None,
        Kind::Bind { .. } => SymbolKind::VARIABLE,
        Kind::ImportModule { .. } | Kind::ImportItem { .. } => SymbolKind::NAMESPACE,
        _ => return None,
    })
}

pub(crate) fn span_text<'a>(content: &'a str, span: &diag::Span) -> &'a str {
    &content[span.start().byte_offset()..span.end().byte_offset()]
}

/// Remove the source-level comment markers while preserving Markdown layout.
fn normalize_doc(content: &str, span: &diag::Span) -> String {
    normalize_doc_text(span_text(content, span))
}

fn normalize_doc_text(doc: &str) -> String {
    doc.split('\n')
        .map(|line| {
            line.trim()
                .strip_prefix('#')
                .expect("a doc span contains only comments")
                .strip_prefix(' ')
                .unwrap_or_else(|| {
                    line.trim()
                        .strip_prefix('#')
                        .expect("a doc span contains only comments")
                })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn one_line_source(content: &str, span: &diag::Span) -> String {
    span_text(content, span)
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A parameter of a signature, from its declaration
fn parameter(content: &str, unit: &Unit<'_>, id: NodeId) -> Option<signature::Param> {
    let node = unit.node(id)?;
    let default = match node.kind() {
        Kind::PositionalParam { default, .. } | Kind::KeyParam { default, .. } => default,
        Kind::RestParam { .. } | Kind::SelfParam { .. } => None,
        _ => return None,
    };
    let span = node.span()?;
    let ty = node
        .children()
        .filter_map(|child| unit.node(child))
        .find(|child| matches!(child.kind(), Kind::Type { .. }))
        .and_then(|child| child.span());
    // The parameter as written ends where its annotation or default starts
    let end = [ty.as_ref(), default.as_ref()]
        .into_iter()
        .flatten()
        .map(|span| span.start().byte_offset())
        .min()
        .unwrap_or(span.end().byte_offset());
    let head = content.get(span.start().byte_offset()..end)?.trim_end();
    let head = head.strip_suffix('=').unwrap_or(head).trim_end();
    let head = head.strip_suffix('@').unwrap_or(head).trim_end();
    Some(signature::Param {
        head: head.lines().map(str::trim).collect::<Vec<_>>().join(" "),
        ty: ty.map(|span| one_line_source(content, &span)),
        default: default.is_some(),
    })
}

/// A node's children in source order.
fn children_in_order(unit: &Unit<'_>, node: &Node<'_>) -> Vec<NodeId> {
    let mut children: Vec<NodeId> = node.children().collect();
    // A prelude binding, having no span, sorts first
    children.sort_by_key(|id| {
        unit.node(*id)
            .expect("child node remains in its unit")
            .span()
            .map(|span| span.start().byte_offset())
    });
    children
}

/// The source text of the type describing a node, if it has one, given its children.
fn type_label(content: &str, unit: &Unit<'_>, children: &[NodeId]) -> Option<String> {
    children
        .iter()
        .filter_map(|child| unit.node(*child))
        .find(|child| matches!(child.kind(), Kind::Type { .. }))
        .and_then(|child| child.span())
        .map(|span| one_line_source(content, &span))
}

/// A declaration's signature, including any type it is annotated with.
fn declaration_label(content: &str, unit: &Unit<'_>, id: NodeId) -> Option<String> {
    let node = unit.node(id)?;
    let kind = node.kind();
    let children = children_in_order(unit, &node);
    let mut name = node
        .definition()
        .map(|span| span_text(content, &span).to_owned())?;
    let binders = children
        .iter()
        .filter_map(|child| unit.node(*child))
        .filter(|child| matches!(child.kind(), Kind::Binder { .. }))
        .filter_map(|child| child.span())
        .map(|span| one_line_source(content, &span))
        .collect::<Vec<_>>();
    if !binders.is_empty() {
        name.push('[');
        name.push_str(&binders.join(", "));
        name.push(']');
    }
    // A class's types are its superclasses, which describe nothing about it
    let ty = if matches!(kind, Kind::Class { .. }) {
        None
    } else {
        type_label(content, unit, &children)
    };
    let params = || {
        children
            .iter()
            .filter_map(|child| parameter(content, unit, *child))
            .collect::<Vec<_>>()
    };
    // Only the decorators the type checker designates say anything about the
    // declaration, and only when they are `std`'s
    let designated = |items: &[&str]| -> String {
        children
            .iter()
            .filter_map(|child| {
                let Kind::Decorator {
                    target: Some(target),
                } = unit.node(*child)?.kind()
                else {
                    return None;
                };
                let item = match unit.node(target)?.kind() {
                    Kind::PreludeItem {
                        module: "std",
                        item,
                        ..
                    } => item,
                    Kind::ImportItem { module, item, .. }
                        if span_text(content, &module) == "std" =>
                    {
                        span_text(content, &item)
                    }
                    _ => return None,
                };
                items.contains(&item).then(|| format!("#[{item}] "))
            })
            .collect()
    };
    let label = match kind {
        Kind::Class {
            is_pub, type_only, ..
        } => {
            let supers = children
                .iter()
                .filter_map(|child| unit.node(*child))
                .filter(|child| matches!(child.kind(), Kind::Type { .. }))
                .filter_map(|child| child.span())
                .map(|span| span_text(content, &span))
                .collect::<Vec<_>>();
            let decorators = designated(&["final", "mixable"]);
            format!(
                "{decorators}{}{}class {name}{}",
                if is_pub { "pub " } else { "" },
                if type_only { "@" } else { "" },
                if supers.is_empty() {
                    String::new()
                } else {
                    format!(": {}", supers.join(" "))
                }
            )
        }
        Kind::Function { is_pub, .. } | Kind::Method { is_pub, .. } => {
            let params = params();
            // Only a statement-level overload is always written with `@`; a protocol
            // member is type-only without one
            let at = matches!(
                kind,
                Kind::Function {
                    type_only: true,
                    ..
                }
            );
            // A getter or setter is used as a field, not called
            let decorators = match kind {
                Kind::Method { .. } => designated(&["getter", "setter"]),
                _ => String::new(),
            };
            let prefix = format!(
                "{decorators}{}{}def {name}",
                if is_pub && !at { "pub " } else { "" },
                if at { "@" } else { "" },
            );
            signature::def(&prefix, &params, ty.as_deref())
        }
        Kind::SpecialMethod { .. } => {
            signature::def(&format!("def {name}"), &params(), ty.as_deref())
        }
        Kind::Field { is_pub, .. } => signature::binding(
            &format!("{}field {name}", if is_pub { "pub " } else { "" }),
            ty.as_deref(),
        ),
        Kind::Bind { is_pub, .. } => signature::binding(
            &format!("{}let {name}", if is_pub { "pub " } else { "" }),
            ty.as_deref(),
        ),
        // A parameter is labeled as written, but for its default
        Kind::PositionalParam { .. }
        | Kind::KeyParam { .. }
        | Kind::RestParam { .. }
        | Kind::SelfParam { .. } => parameter(content, unit, id)?.alone(),
        _ => return None,
    };
    Some(label)
}

/// Hover text for a name that resolves outside this document -- an import or
/// a prelude binding -- built from the static doc index rather than a local
/// doc comment, since there is no declaration in this file to read one from.
fn external_hover(kind: Kind<'_>, content: &str) -> Vec<Described> {
    let (module, item) = match kind {
        Kind::ImportModule { module, .. } => (span_text(content, &module), ""),
        Kind::ImportItem { module, item, .. } => {
            (span_text(content, &module), span_text(content, &item))
        }
        Kind::PreludeModule { module, .. } => (module, ""),
        Kind::PreludeItem { module, item, .. } => (module, item),
        _ => return Vec::new(),
    };
    bundled_hover(module, item)
}

/// Hover text for an item of a bundled module, from the static doc index.
/// Overloads share a name, and each is described in turn.
fn bundled_hover(module: &str, item: &str) -> Vec<Described> {
    doc_index::lookup(module, item)
        .iter()
        .map(Described::entry)
        .collect()
}

/// One declaration's docs, as hover shows them
#[derive(Debug, Clone)]
struct Described {
    /// Its signature, fenced
    label: String,
    /// Where it's declared, as "in module `m`" or "in class `m.C`"
    note: Option<String>,
    /// Its doc comment
    prose: String,
    shape: Shape,
}

/// What a declaration's label says
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// A function, method or class, whose signature says what its checked
    /// type would
    Signature,
    /// A variable or parameter, whose checked type says all its label does
    Value,
    Other,
}

impl Described {
    /// An entry of the static doc index
    fn entry(entry: &doc_index::DocEntry) -> Self {
        let note = match (entry.kind, entry.item.rsplit_once('.')) {
            ("module", _) => None,
            (_, Some((class, _))) => Some(format!("in class `{}.{class}`", entry.module)),
            (_, None) => Some(format!("in module `{}`", entry.module)),
        };
        let shape = match entry.kind {
            "function" | "method" | "class" => Shape::Signature,
            "value" => Shape::Value,
            _ => Shape::Other,
        };
        Described {
            label: doc_index::signature(entry),
            note,
            prose: entry.doc.to_owned(),
            shape,
        }
    }

    /// The docs as markdown: a fenced label, where it's declared, then its doc
    /// comment. Below a `typed` block, a value's label repeats it, so only its
    /// doc comment is left, and nothing when it has none.
    fn render(&self, typed: bool) -> Option<String> {
        if typed && matches!(self.shape, Shape::Value) {
            return (!self.prose.is_empty()).then(|| self.prose.clone());
        }
        let label = &self.label;
        let fence = if label.contains("```") { "````" } else { "```" };
        let mut markdown = format!("{fence}dolang\n{label}\n{fence}");
        if let Some(note) = &self.note {
            markdown.push_str(&format!("\n\n*{note}*"));
        }
        if !self.prose.is_empty() {
            markdown.push_str("\n\n");
            markdown.push_str(&self.prose);
        }
        Some(markdown)
    }
}

/// Pre-render local hover text while the compiler unit and its spans live.
fn build_hovers(unit: &Unit<'_>, content: &str) -> HashMap<NodeId, Vec<Described>> {
    unit.nodes()
        .filter_map(|(id, node)| {
            let mut docs = external_hover(node.kind(), content);
            if docs.is_empty() {
                docs.extend(local_hover(unit, id, None));
            }
            (!docs.is_empty()).then_some((id, docs))
        })
        .collect()
}

/// Hover text for a declaration in a unit: its signature, where it's declared
/// unless that's `module`'s top level, and its doc comment. `module` is `None`
/// for the document's own module, whose name a note leaves out.
fn local_hover(unit: &Unit<'_>, id: NodeId, module: Option<&str>) -> Option<Described> {
    let content = unit.source();
    let label = declaration_label(content, unit, id)?;
    let node = unit.node(id)?;
    let shape = match node.kind() {
        Kind::Function { .. }
        | Kind::Method { .. }
        | Kind::SpecialMethod { .. }
        | Kind::Class { .. } => Shape::Signature,
        Kind::Bind { .. }
        | Kind::PositionalParam { .. }
        | Kind::KeyParam { .. }
        | Kind::RestParam { .. }
        | Kind::SelfParam { .. } => Shape::Value,
        _ => Shape::Other,
    };
    Some(Described {
        label,
        note: source_note(unit, id, module),
        prose: node
            .doc()
            .map(|span| normalize_doc(content, &span))
            .unwrap_or_default(),
        shape,
    })
}

/// Where a declaration in a unit is: in the classes around it, or at the top
/// level of `module`. A declaration in anything else, such as a function,
/// gets no note.
fn source_note(unit: &Unit<'_>, id: NodeId, module: Option<&str>) -> Option<String> {
    let root = unit.root()?;
    let mut path = Vec::new();
    let mut parent = unit.node(id)?.parent()?;
    while parent != root {
        let node = unit.node(parent)?;
        let Kind::Class { name, .. } = node.kind() else {
            return None;
        };
        path.push(span_text(unit.source(), &name));
        parent = node.parent()?;
    }
    let class = !path.is_empty();
    path.extend(module);
    path.reverse();
    match (class, &path[..]) {
        (_, []) => None,
        (true, _) => Some(format!("in class `{}`", path.join("."))),
        (false, _) => Some(format!("in module `{}`", path.join("."))),
    }
}

/// Assemble the document outline from the node table.
///
/// Parentage comes from the table, so this walks no syntax: an entry finds its
/// outline parent by following node parents until one of them is an entry too.
fn build_symbols(unit: &Unit<'_>, index: &DocumentIndex<'_>) -> Vec<DocumentSymbol> {
    let content = index.content;
    let mut ids = Vec::new();
    let mut symbols = Vec::new();
    let mut of_node = HashMap::new();

    for (id, node) in unit.nodes() {
        let kind = node.kind();
        let Some(symbol_kind) = symbol_kind(&kind, content) else {
            continue;
        };
        let (Some(name), Some(span)) = (node.definition(), node.span()) else {
            continue;
        };
        let selection_range = index.range_from_span(&name);
        // A client rejects an outline whose selection range escapes its range,
        // so take the union rather than trust the two to nest.
        let mut range = index.range_from_span(&span);
        range.start = range.start.min(selection_range.start);
        range.end = range.end.max(selection_range.end);
        of_node.insert(id, ids.len());
        ids.push(id);
        symbols.push(Some(DocumentSymbol {
            name: span_text(content, &name).to_owned(),
            detail: None,
            kind: symbol_kind,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range,
            selection_range,
            children: None,
        }));
    }

    // The outline parent is the nearest ancestor that is an entry too, so an
    // `if` or a `for` in between simply disappears.
    let parents: Vec<Option<usize>> = ids
        .iter()
        .map(|id| {
            let mut cur = unit.node(*id).and_then(|node| node.parent());
            while let Some(parent) = cur {
                if let Some(entry) = of_node.get(&parent) {
                    return Some(*entry);
                }
                cur = unit.node(parent).and_then(|node| node.parent());
            }
            None
        })
        .collect();

    // Fold children into parents from the back: a node is always allocated
    // after the node that contains it, so every child is finished first.
    let mut children: Vec<Vec<DocumentSymbol>> = vec![Vec::new(); symbols.len()];
    let mut roots = Vec::new();
    for entry in (0..symbols.len()).rev() {
        debug_assert!(
            parents[entry].is_none_or(|parent| parent < entry),
            "a node precedes the node that contains it"
        );
        let mut symbol = symbols[entry].take().expect("entry visited once");
        let mut kids = mem::take(&mut children[entry]);
        kids.sort_by_key(|child| child.range.start);
        symbol.children = (!kids.is_empty()).then_some(kids);
        match parents[entry] {
            Some(parent) => children[parent].push(symbol),
            None => roots.push(symbol),
        }
    }
    roots.sort_by_key(|symbol| symbol.range.start);
    roots
}

#[derive(Debug, Clone)]
struct Patch {
    diagnostic_range: Range,
    diagnostic_severity: DiagnosticSeverity,
    diagnostic_message: String,
    patch_range: Range,
    replacement: String,
    title: String,
}

/// A declaration, and every token in this document that names it.
///
/// The declaration's own name is one of the tokens the elaborator resolves to
/// it, so it is already in `uses`: `includeDeclaration` takes it out rather
/// than putting it in.
#[derive(Debug, Default)]
struct Decl {
    name_range: Range,
    uses: Vec<Range>,
    hover: Vec<Described>,
}

/// What the worker made of one revision of a document.
///
/// A newer projection replaces the whole document, so handlers hold an `Arc`
/// of one rather than a lock.
struct Document {
    /// The revision it projects
    member: Arc<Member>,
    /// The revision's unit, which holds the document's source
    unit: Arc<Unit<'static>>,
    /// The unit's compile diagnostics
    diagnostics: Vec<Diagnostic>,
    tokens: Vec<SemanticToken>,
    /// Token range to the declaration it names, sorted by range start
    refs: Vec<(Range, NodeId)>,
    decls: HashMap<NodeId, Decl>,
    symbols: Vec<DocumentSymbol>,
    patches: Vec<Patch>,
    /// What the latest check of its revision concluded. A check after an
    /// import's change replaces it.
    analyzed: Mutex<Option<Arc<Analyzed>>>,
}

/// What a check concluded about a document
struct Analyzed {
    /// The document's unit in the check
    unit: UnitId,
    /// The document's module name in the check
    name: String,
    /// What the check concluded about all its units
    analysis: Arc<typeck::Analysis>,
    /// The modules the check read from source
    modules: Arc<Modules>,
}

/// A document's check
#[derive(Clone, Default)]
pub(crate) struct Checked {
    diagnostics: Vec<Diagnostic>,
    /// `None` for a unit the checker can't take
    analyzed: Option<Arc<Analyzed>>,
}

impl Document {
    /// The token under `pos` naming a declaration, and the declaration
    fn reference_at(&self, pos: &Position) -> Option<(Range, NodeId, &Decl)> {
        let end = self.refs.partition_point(|(range, _)| &range.start <= pos);
        let (range, id) = self.refs.get(end.checked_sub(1)?)?;
        if &range.end <= pos {
            return None;
        }
        Some((*range, *id, self.decls.get(id)?))
    }

    /// What the latest check concluded, if it has been checked
    fn analyzed(&self) -> Option<Arc<Analyzed>> {
        self.analyzed
            .lock()
            .expect("analysis lock poisoned")
            .clone()
    }

    /// The span of the innermost node the latest check judged at `offset`, and
    /// the lines of what it concluded there. `reference` is the span of the
    /// doc-index reference under the cursor, which names a node at just its
    /// span that has no name of its own.
    fn checked_at(
        &self,
        analyzed: &Analyzed,
        offset: usize,
        reference: Option<source::Span>,
    ) -> Option<(source::Span, Vec<String>)> {
        let node = analyzed
            .analysis
            .at(analyzed.unit, offset.try_into().ok()?)?;
        let span = node.span();
        let named = (reference == Some(span))
            .then(|| {
                self.unit
                    .source()
                    .get(span.start as usize..span.end as usize)
            })
            .flatten();
        Some((span, label(node, named)?))
    }

    /// The declaration named by the token under `pos`, if any.
    ///
    /// `refs` is sorted by where each token starts, so the only candidate is
    /// the last one starting at or before the position.
    fn decl_at(&self, pos: &Position) -> Option<&Decl> {
        self.reference_at(pos).map(|(_, _, decl)| decl)
    }

    /// The declarations the name at `offset` stands for in the modules the
    /// check read from source, and the overload a call of it chose. That's a
    /// module, as its root; an item imported from a module, as the reference
    /// `id` names it; a function
    /// declared here, when a call chose one of its overloads; the members a
    /// member use reached, one for each class of a union receiver; or the
    /// item a path through a module names.
    fn targets<'a>(
        &self,
        analyzed: &'a Analyzed,
        offset: usize,
        reference: Option<NodeId>,
    ) -> (Vec<Target<'a>>, Option<u32>) {
        let modules = &analyzed.modules;
        let node = offset
            .try_into()
            .ok()
            .and_then(|offset| analyzed.analysis.at(analyzed.unit, offset));
        let chosen = node.and_then(chosen);
        let found = match reference {
            Some(id) => {
                let content = self.unit.source();
                let root = |module: &str| {
                    let module = modules.get(module)?;
                    Some(Target::Source(module, module.unit.root()?))
                };
                let target = match self.unit.node(id).map(|node| node.kind()) {
                    Some(Kind::ImportModule { module, .. }) => root(span_text(content, &module)),
                    Some(Kind::PreludeModule { module, .. }) => root(module),
                    Some(Kind::ImportItem { module, item, .. }) => lookup::item(
                        modules,
                        span_text(content, &module),
                        span_text(content, &item),
                    ),
                    Some(Kind::PreludeItem { module, item, .. }) => {
                        lookup::item(modules, module, item)
                    }
                    Some(Kind::Function { .. }) if chosen.is_some() => modules
                        .get(&analyzed.name)
                        .map(|module| Target::Source(module, id)),
                    _ => None,
                };
                target.into_iter().collect()
            }
            None => node
                .map(|node| node_targets(modules, node))
                .unwrap_or_default(),
        };
        // A union receiver's calls each chose their own
        let chosen = chosen.filter(|_| found.len() == 1);
        (found, chosen)
    }
}

/// What the analysis `node` names: the members a member use reached, or else
/// the item a path through a module names
fn node_targets<'a>(modules: &'a Modules, node: typeck::Node<'_>) -> Vec<Target<'a>> {
    let members: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Member)
        .filter_map(|judged| {
            let class = judged.declaration()?;
            lookup::member(modules, class.module(), class.name(), judged.name()?)
        })
        .collect();
    if !members.is_empty() {
        return members;
    }
    let item = node.judgments().find(|judged| judged.role() == Role::Item);
    let item = item.and_then(|judged| judged.declaration());
    item.and_then(|item| lookup::item(modules, item.module(), item.name()))
        .into_iter()
        .collect()
}

/// The overload chosen by the call of what `node` names. A method call is
/// judged at its member, and a function call at the whole call, which starts
/// where its callee does. A union receiver's call reaches several.
fn chosen(node: typeck::Node<'_>) -> Option<u32> {
    let start = node.span().start;
    let callees = node
        .ancestors()
        .take_while(|ancestor| ancestor.span().start == start)
        .map(|ancestor| {
            let callees = ancestor.judgments();
            callees
                .filter(|judged| judged.role() == Role::Callee)
                .collect::<Vec<_>>()
        })
        .find(|callees| !callees.is_empty())?;
    match callees[..] {
        [callee] => callee.overload(),
        _ => None,
    }
}

/// The declaration a target names, or for an overloaded function or method,
/// the overload `chosen`
fn chosen_node<'m>(
    target: &Target<'m>,
    chosen: Option<u32>,
) -> Option<(&'m lookup::Module, NodeId)> {
    let Target::Source(module, id) = *target else {
        return None;
    };
    let id = match chosen {
        Some(index) => lookup::overload(&module.unit, id, index),
        None => id,
    };
    Some((module, id))
}

/// Where a declaration found in a module's source is, as `encoding` counts
fn location(
    target: &Target<'_>,
    chosen: Option<u32>,
    encoding: PositionEncodingKind,
) -> Option<Location> {
    let (module, id) = chosen_node(target, chosen)?;
    let unit = &module.unit;
    let span = unit.node(id)?.definition()?;
    let index = DocumentIndex::new(unit.source(), encoding);
    Some(Location::new(
        module.uri.clone(),
        index.range_from_span(&span),
    ))
}

/// What hover shows of a declaration found in another module, or of the
/// overload `chosen` of it. An overload with no doc comment of its own has its
/// implementation's. A declaration in `own`, the document's module, is noted
/// as a local one is.
fn target_hover(target: &Target<'_>, chosen: Option<u32>, own: Option<&str>) -> Vec<Described> {
    match *target {
        Target::Source(module, implementation) => {
            let described = chosen_node(target, chosen).and_then(|(_, id)| {
                let unit = &module.unit;
                // A module is documented by the comment opening its source
                if unit.root() == Some(id) {
                    return Some(Described {
                        label: format!("module {}", module.name),
                        note: None,
                        prose: unit
                            .node(id)?
                            .doc()
                            .map(|span| normalize_doc(unit.source(), &span))
                            .unwrap_or_default(),
                        shape: Shape::Other,
                    });
                }
                let name = Some(&module.name[..]).filter(|&name| Some(name) != own);
                let mut described = local_hover(unit, id, name)?;
                if described.prose.is_empty()
                    && let Some(span) = unit.node(implementation)?.doc()
                {
                    described.prose = normalize_doc(unit.source(), &span);
                }
                Some(described)
            });
            described.into_iter().collect()
        }
        Target::Bundled(ref module, ref item) => {
            let entries = doc_index::lookup(module, item);
            // The index describes each overload in turn
            let entry = chosen
                .filter(|_| entries.len() > 1)
                .and_then(|index| entries.get(usize::try_from(index).ok()?));
            match entry {
                Some(entry) => vec![Described::entry(entry)],
                None => bundled_hover(module, item),
            }
        }
    }
}

/// What hover shows of a node, a line each, from the first of:
///
/// - the variable it reads or binds;
/// - the signatures a call reached, by its member's name for a method;
/// - the type of the member a member use reached, by its name in its class;
/// - the type of the item a path through a module names, by its module;
/// - its value, by `named` where given.
fn label(node: typeck::Node<'_>, named: Option<&str>) -> Option<Vec<String>> {
    let find = |role| node.judgments().find(|judged| judged.role() == role);
    let typed = |name: Option<&str>, ty: String| match name {
        Some(name) => format!("{name} @ {ty}"),
        None => ty,
    };
    // A member use's own name is the member's, but its type is the declaring
    // class
    let variable = node
        .judgments()
        .find(|judged| matches!(judged.role(), Role::Read | Role::Bind));
    if let Some(judged) = variable {
        return Some(vec![typed(Some(judged.name()?), judged.ty(Style::Reader))]);
    }
    // A member by the class declaring it, as `m.Box.item`; a union receiver's
    // reach one in each alternative's class
    let members: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Member)
        .filter_map(|judged| {
            let name = judged.name()?;
            Some(match judged.declaration() {
                Some(class) => format!("{}.{}.{name}", class.module(), class.name()),
                None => name.to_owned(),
            })
        })
        .collect();
    // An item by the module exporting it, as `m.f`
    let item = find(Role::Item)
        .and_then(|judged| judged.declaration())
        .map(|item| format!("{}.{}", item.module(), item.name()));
    let member = (!members.is_empty()).then(|| members.join(" | ")).or(item);
    let member = member.as_deref();
    let callees: Vec<_> = node
        .judgments()
        .filter(|judged| judged.role() == Role::Callee)
        .map(|judged| {
            let line = typed(member, judged.ty(Style::Reader));
            match judged.overload() {
                Some(index) => format!("{line}  # overload {}", index + 1),
                None => line,
            }
        })
        .collect();
    if !callees.is_empty() {
        return Some(callees);
    }
    let value = find(Role::Value)?.ty(Style::Reader);
    Some(vec![typed(member.or(named), value)])
}

#[derive(Debug, Default)]
struct Config {
    root: Option<PathBuf>,
    workspaces: Vec<PathBuf>,
    /// Whether the client lets the server register file watchers
    watch: bool,
}

/// The documents the client has open, and the workspaces they keep alive.
#[derive(Default)]
struct Documents {
    handles: HashMap<Uri, Handle>,
    workspaces: Workspaces,
}

/// A document the client has open.
struct Handle {
    workspace: Arc<Workspace>,
    path: PathBuf,
    encoding: PositionEncodingKind,
    /// Its current revision, the one its workspace holds
    member: Arc<Member>,
    /// The projection of `member`, once the worker has made it
    revision: Revision,
    /// The last projection published, for requests to fall back on
    projected: Option<Arc<Document>>,
}

/// A document's projection of one revision of its text, once the worker has
/// made it, or an error if the worker passed over the revision.
type Revision = Shared<oneshot::Receiver<Arc<Document>>>;

impl Documents {
    /// Stores the projection of a document's current revision, returning
    /// whether it is that.
    fn project(&mut self, uri: &Uri, document: &Arc<Document>) -> bool {
        match self.handles.get_mut(uri) {
            Some(handle) if Arc::ptr_eq(&handle.member, &document.member) => {
                handle.projected = Some(document.clone());
                true
            }
            _ => false,
        }
    }

    /// The stored projection a check of `member` belongs to, unless a newer
    /// revision has replaced it.
    fn checked(&self, uri: &Uri, member: &Arc<Member>) -> Option<Arc<Document>> {
        let handle = self.handles.get(uri)?;
        let document = handle.projected.as_ref()?;
        (Arc::ptr_eq(&handle.member, member) && Arc::ptr_eq(&document.member, member))
            .then(|| document.clone())
    }
}

pub(crate) struct Backend {
    documents: Arc<Mutex<Documents>>,
    worker: mpsc::Sender<Request>,
    client: Client,
    config: AsyncMutex<Config>,
    position_encoding: RwLock<PositionEncodingKind>,
}

/// A revision of an open document to compile and project.
struct Job {
    uri: Uri,
    workspace: Arc<Workspace>,
    member: Arc<Member>,
    encoding: PositionEncodingKind,
    /// Given the projection once it is published, and dropped if the worker
    /// passes over the revision
    done: oneshot::Sender<Arc<Document>>,
}

enum Request {
    Compile(Job),
    /// A workspace whose table changed, to check once it settles
    Due(Arc<Workspace>),
}

enum Event {
    Projected {
        uri: Uri,
        document: Document,
        diagnostics: Vec<Diagnostic>,
        done: oneshot::Sender<Arc<Document>>,
    },
    /// A check of a revision
    Checked {
        uri: Uri,
        member: Arc<Member>,
        checked: Checked,
    },
}

/// Compiles documents off the event loop, on a thread of its own, and type
/// checks their workspaces once their tables settle.
///
/// Projections and checks go to the applier task, which publishes those of
/// current revisions in the order the worker made them.
struct Worker {
    events: UnboundedSender<Event>,
    documents: Arc<Mutex<Documents>>,
    /// How long a workspace's table must settle before a check, or `None` not
    /// to check
    check_delay: Option<Duration>,
    /// When each workspace whose table changed is due to be checked
    pending: Vec<(Weak<Workspace>, Instant)>,
}

impl Worker {
    fn run(mut self, requests: mpsc::Receiver<Request>) {
        loop {
            // Few workspaces are live, so a scan finds the next check soon enough
            let due = (self.pending.iter().enumerate())
                .min_by_key(|(_, (_, deadline))| *deadline)
                .map(|(at, (_, deadline))| (at, *deadline));
            let first = match due {
                None => match requests.recv() {
                    Ok(first) => first,
                    Err(mpsc::RecvError) => break,
                },
                Some((at, deadline)) => {
                    match requests.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(first) => first,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let (workspace, _) = self.pending.swap_remove(at);
                            if let Some(workspace) = workspace.upgrade() {
                                self.check(&workspace);
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            };
            let batch: Vec<_> = iter::once(first).chain(requests.try_iter()).collect();
            // Only the newest revision of each document is worth compiling
            let superseded: Vec<bool> = (0..batch.len())
                .map(|i| match &batch[i] {
                    Request::Compile(job) => batch[i + 1..].iter().any(|later| match later {
                        Request::Compile(later) => later.uri == job.uri,
                        Request::Due(_) => false,
                    }),
                    Request::Due(_) => false,
                })
                .collect();
            for (request, superseded) in batch.into_iter().zip(superseded) {
                match request {
                    Request::Compile(job) if superseded => self.due(&job.workspace),
                    Request::Compile(job) => self.compile(job),
                    Request::Due(workspace) => self.due(&workspace),
                }
            }
        }
    }

    /// Puts a workspace's check off until its table settles again.
    fn due(&mut self, workspace: &Arc<Workspace>) {
        let Some(delay) = self.check_delay else {
            return;
        };
        let deadline = Instant::now() + delay;
        let pending = (self.pending.iter_mut())
            .find(|(pending, _)| pending.as_ptr() == Arc::as_ptr(workspace));
        match pending {
            Some((_, pending)) => *pending = deadline,
            None => self.pending.push((Arc::downgrade(workspace), deadline)),
        }
    }

    fn compile(&mut self, job: Job) {
        let Job {
            uri,
            workspace,
            member,
            encoding,
            done,
        } = job;
        self.due(&workspace);
        if !workspace.current(&member) {
            return;
        }
        let projected = panic::catch_unwind(AssertUnwindSafe(|| {
            let unit = workspace.unit(&member);
            project(&uri, member.clone(), unit, &encoding)
        }));
        // The panic hook has logged the panic itself
        let Ok(mut document) = projected else {
            log::error!("failed to compile {}", uri.as_str());
            return;
        };
        // The same text keeps its check until the workspace's is done
        let diagnostics = match member.checked() {
            Some(checked) => {
                *document.analyzed.get_mut().expect("analysis lock poisoned") =
                    checked.analyzed.clone();
                [&document.diagnostics[..], &checked.diagnostics].concat()
            }
            None => document.diagnostics.clone(),
        };
        _ = self.events.send(Event::Projected {
            uri,
            document,
            diagnostics,
            done,
        });
    }

    /// Checks a workspace whose table has settled.
    fn check(&mut self, workspace: &Arc<Workspace>) {
        let start = Instant::now();
        let round = panic::catch_unwind(AssertUnwindSafe(|| workspace.prepare()));
        let checks = match round {
            Ok(Round::Checks(checks)) => checks,
            // Its table changed, which puts the check off again
            Ok(Round::Changed) => return self.due(workspace),
            Err(_) => {
                log::error!("failed to prepare a check");
                return;
            }
        };
        if checks.is_empty() {
            return;
        }
        let encodings: HashMap<Uri, PositionEncodingKind> = {
            let documents = self.documents.lock().expect("documents lock poisoned");
            (documents.handles.iter())
                .map(|(uri, handle)| (uri.clone(), handle.encoding.clone()))
                .collect()
        };
        for check in checks {
            let checked = panic::catch_unwind(AssertUnwindSafe(|| {
                check_units(workspace, &check, &encodings)
            }));
            let Ok(checked) = checked else {
                log::error!("failed to check");
                continue;
            };
            for member in workspace.settle(check, checked) {
                // The compile diagnostics already published are all there is
                // to say about a unit the checker can't take
                if let Some(uri) = member.uri()
                    && let Some(checked) = member.checked()
                    && checked.analyzed.is_some()
                {
                    let uri = uri.clone();
                    _ = (self.events).send(Event::Checked {
                        uri,
                        member,
                        checked,
                    });
                }
            }
        }
        log::debug!("checked a workspace in {:?}", start.elapsed());
    }
}

/// Publishes what the worker made of current revisions, in order.
async fn publish(
    client: Client,
    documents: Arc<Mutex<Documents>>,
    mut events: UnboundedReceiver<Event>,
) {
    let lock = || documents.lock().expect("documents lock poisoned");
    while let Some(event) = events.recv().await {
        match event {
            Event::Projected {
                uri,
                document,
                diagnostics,
                done,
            } => {
                let document = Arc::new(document);
                let version = document.member.version();
                if lock().project(&uri, &document) {
                    client.publish_diagnostics(uri, diagnostics, version).await;
                }
                _ = done.send(document);
            }
            Event::Checked {
                uri,
                member,
                checked,
            } => {
                let document = lock().checked(&uri, &member);
                if let Some(document) = document {
                    *document.analyzed.lock().expect("analysis lock poisoned") = checked.analyzed;
                    let diagnostics = [&document.diagnostics[..], &checked.diagnostics].concat();
                    (client)
                        .publish_diagnostics(uri, diagnostics, member.version())
                        .await;
                }
            }
        }
    }
}

/// Compiles a document as the server projects it, as `module` if it is one,
/// with the configured prelude `imports`. The shell compiles a module under its
/// prelude as it does a script.
pub(crate) fn compile_document(
    path: &Path,
    text: String,
    imports: &[Import],
    module: Option<&str>,
) -> Unit<'static> {
    let mut config = CompileConfig::new();
    config.recover(true).document(true).typecheck(true);
    if let Some(module) = module {
        config.mode(Mode::Module {
            name: module.to_owned().into(),
        });
    }
    let mut prelude = config.prelude();
    for import in imports {
        match import {
            Import::Module(module) => {
                prelude = prelude.import_module(module.clone());
            }
            Import::TypeItem(module, item) => {
                prelude = prelude
                    .import_items(module.clone())
                    .type_item(item.clone())
                    .commit();
            }
            Import::TypeItemAs(module, item, bind) => {
                prelude = prelude
                    .import_items(module.clone())
                    .type_item_with_name(item.clone(), bind.clone())
                    .commit();
            }
            Import::Item(module, item) => {
                let items = prelude.import_items(module.clone());
                prelude = items.item(item.clone()).commit();
            }
            Import::ModuleAs(module, bind) => {
                prelude = prelude.import_module_with_name(module.clone(), bind.clone());
            }
            Import::ItemAs(module, item, bind) => {
                let items = prelude.import_items(module.clone());
                prelude = items.item_with_name(item.clone(), bind.clone()).commit();
            }
        }
    }
    config.unit(path, text)
}

/// Type checks a workspace's members together, against the bundled typelibs
/// of the rest of what they import.
///
/// Gives each member the checker's diagnostics located in it, if the client
/// has it open, and the check's analysis, or no analysis when its unit failed
/// to compile and can't be checked.  A module neither a member nor bundled is
/// reported as not found.
fn check_units(
    workspace: &Workspace,
    check: &Check,
    encodings: &HashMap<Uri, PositionEncodingKind>,
) -> Vec<Checked> {
    let units: Vec<_> = (check.members().iter())
        .map(|member| workspace.unit(member))
        .collect();
    let mut builder = typeck::Builder::new();
    builder.analysis(true);
    // The shell's pipelines connect stages with `proc`'s pipes
    builder.pipes(("proc", "PipeSender"), ("proc", "PipeReceiver"));
    let ids: Vec<_> = (check.members().iter().zip(&units))
        .map(|(member, unit)| match builder.unit(unit) {
            Ok(id) => Some(id),
            // A source that failed to compile is left out, and imports of its
            // module are reported as not found
            Err(err) if matches!(err.kind(), ErrorKind::Fail) => None,
            Err(err) => {
                log::error!("cannot check {}: {err}", member.path().display());
                None
            }
        })
        .collect();
    for typelib in check.typelibs() {
        if let Err(err) = builder.typelib(typelib) {
            log::error!("cannot check against {}: {err}", typelib.module());
        }
    }
    let checked = builder.check();
    let at: HashMap<UnitId, usize> = (ids.iter().enumerate())
        .filter_map(|(at, id)| Some(((*id)?, at)))
        .collect();
    // Where each member's diagnostics are located, if the client has it open
    let documents: Vec<_> = (check.members().iter().zip(&units))
        .map(|(member, unit)| {
            let uri = member.uri()?;
            let encoding = encodings.get(uri).cloned();
            let encoding = encoding.unwrap_or(PositionEncodingKind::UTF16);
            let index = DocumentIndex::new(unit.source(), encoding);
            Some((uri, index))
        })
        .collect();
    let mut diagnostics = vec![Vec::new(); units.len()];
    for diag in checked.diagnostics() {
        let Some((id, &at)) = (diag.span().unit()).and_then(|id| Some((id, at.get(&id)?))) else {
            continue;
        };
        if let Some((uri, index)) = &documents[at] {
            let relate = |range, span: &diag::SourceSpan, message: String| {
                relate_checked(uri, index, &checked, id, range, span, message)
            };
            convert_diag(index, diag, relate, &mut diagnostics[at]);
        }
    }
    let names: Vec<_> = (ids.iter())
        .map(|id| id.map(|id| checked.name(id)))
        .collect();
    let modules: Modules = (check.members().iter().zip(&units).zip(&ids))
        .filter_map(|((member, unit), id)| {
            let id = (*id)?;
            let uri = match member.uri() {
                Some(uri) => uri.clone(),
                None => Uri::from_file_path(checked.path(id))?,
            };
            let name = checked.name(id);
            let module = lookup::Module {
                uri,
                name: name.clone(),
                unit: unit.clone(),
            };
            Some((name, module))
        })
        .collect();
    let modules = Arc::new(modules);
    let analysis = checked.into_analysis().map(Arc::new);
    (ids.into_iter().zip(names).zip(diagnostics))
        .map(|((id, name), diagnostics)| {
            let analyzed = (id.zip(name).zip(analysis.as_ref())).map(|((unit, name), analysis)| {
                Arc::new(Analyzed {
                    unit,
                    name,
                    analysis: analysis.clone(),
                    modules: modules.clone(),
                })
            });
            Checked {
                diagnostics,
                analyzed,
            }
        })
        .collect()
}

/// Related information for a span a checker diagnostic in the document `id`
/// annotates, which may lie in another unit.
///
/// A typelib's unit has no source here; its spans are located by line and byte
/// column at the path its typelib was written from.  A bundled typelib's path
/// is relative to the source tree it was built in, so its spans are named in
/// the message, on the diagnostic's own range.
fn relate_checked(
    uri: &Uri,
    index: &DocumentIndex<'_>,
    check: &typeck::Check<'_>,
    id: UnitId,
    range: Range,
    span: &diag::SourceSpan,
    message: String,
) -> DiagnosticRelatedInformation {
    let unit = match span.unit() {
        Some(unit) if unit != id => unit,
        _ => {
            return DiagnosticRelatedInformation {
                location: Location::new(uri.clone(), index.range_from_span(&span.span())),
                message,
            };
        }
    };
    let path = check.path(unit);
    let (start, end) = (span.span().start(), span.span().end());
    match path
        .is_absolute()
        .then(|| Uri::from_file_path(path))
        .flatten()
    {
        Some(uri) => DiagnosticRelatedInformation {
            location: Location::new(
                uri,
                Range::new(
                    Position::new(start.line_offset(), start.column_offset()),
                    Position::new(end.line_offset(), end.column_offset()),
                ),
            ),
            message,
        },
        None => DiagnosticRelatedInformation {
            location: Location::new(uri.clone(), range),
            message: format!(
                "{}:{}:{}: {message}",
                path.display(),
                start.line_number(),
                start.column_number()
            ),
        },
    }
}

fn severity(severity: diag::Severity) -> DiagnosticSeverity {
    match severity {
        diag::Severity::Error => DiagnosticSeverity::ERROR,
        diag::Severity::Warning => DiagnosticSeverity::WARNING,
        _ => DiagnosticSeverity::INFORMATION,
    }
}

/// Converts a diagnostic located in the document, appending it and its notes
/// to `out`.
///
/// `relate` makes related information for an annotation from the
/// diagnostic's range, the annotation's span and its message.
fn convert_diag(
    index: &DocumentIndex<'_>,
    diag: &diag::Diag,
    relate: impl Fn(Range, &diag::SourceSpan, String) -> DiagnosticRelatedInformation,
    out: &mut Vec<Diagnostic>,
) {
    let range = index.range_from_span(&diag.span().span());
    let mut converted = Diagnostic::new_simple(range, diag.message().to_string());
    converted.severity = Some(severity(diag.severity()));
    converted.related_information = Some(
        diag.annotations()
            .map(|ann| relate(range, &ann.span(), ann.message().to_string()))
            .collect(),
    );
    out.push(converted);
    for note in diag.notes() {
        let mut converted = Diagnostic::new_simple(range, note.message().to_string());
        converted.severity = Some(match note.kind() {
            diag::NoteKind::Help => DiagnosticSeverity::HINT,
            _ => DiagnosticSeverity::INFORMATION,
        });
        out.push(converted);
    }
}

#[derive(Debug)]
struct DocumentIndex<'a> {
    content: &'a str,
    line_starts: Vec<usize>,
    position_encoding: PositionEncodingKind,
}

impl<'a> DocumentIndex<'a> {
    fn new(content: &'a str, position_encoding: PositionEncodingKind) -> Self {
        let mut line_starts = vec![0];
        for (offset, ch) in content.char_indices() {
            if ch == '\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            content,
            line_starts,
            position_encoding,
        }
    }

    fn line_start(&self, offset: usize) -> (u32, usize) {
        debug_assert!(offset <= self.content.len());
        debug_assert!(self.content.is_char_boundary(offset));
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        (line as u32, self.line_starts[line])
    }

    fn position_from_offset(&self, offset: usize) -> Position {
        let (line, line_start) = self.line_start(offset);
        let character = if self.position_encoding == PositionEncodingKind::UTF8 {
            (offset - line_start) as u32
        } else if self.position_encoding == PositionEncodingKind::UTF16 {
            self.content[line_start..offset].encode_utf16().count() as u32
        } else {
            unreachable!("unsupported position encoding")
        };
        Position::new(line, character)
    }

    /// The byte offset of `position`, or `None` past the end of its line or
    /// the document.
    fn offset_from_position(&self, position: &Position) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = (self.line_starts.get(position.line as usize + 1))
            .map_or(self.content.len(), |&next| next - 1);
        let line = &self.content[line_start..line_end];
        let character = position.character as usize;
        let within = if self.position_encoding == PositionEncodingKind::UTF8 {
            line.is_char_boundary(character).then_some(character)
        } else if self.position_encoding == PositionEncodingKind::UTF16 {
            let mut units = 0;
            (line.char_indices())
                .map(|(offset, ch)| {
                    let at = units;
                    units += ch.len_utf16();
                    (at, offset)
                })
                .chain(iter::once((line.encode_utf16().count(), line.len())))
                .find(|&(at, _)| at == character)
                .map(|(_, offset)| offset)
        } else {
            unreachable!("unsupported position encoding")
        };
        Some(line_start + within?)
    }

    fn range_from_offsets(&self, start: usize, end: usize) -> Range {
        Range::new(
            self.position_from_offset(start),
            self.position_from_offset(end),
        )
    }

    fn range_from_span(&self, span: &diag::Span) -> Range {
        self.range_from_offsets(span.start().byte_offset(), span.end().byte_offset())
    }

    fn token_length_from_offsets(&self, start: usize, end: usize) -> u32 {
        debug_assert!(self.content.is_char_boundary(start));
        debug_assert!(self.content.is_char_boundary(end));
        if self.position_encoding == PositionEncodingKind::UTF8 {
            (end - start) as u32
        } else if self.position_encoding == PositionEncodingKind::UTF16 {
            self.content[start..end].encode_utf16().count() as u32
        } else {
            unreachable!("unsupported position encoding")
        }
    }

    fn token_length(&self, span: &diag::Span) -> u32 {
        self.token_length_from_offsets(span.start().byte_offset(), span.end().byte_offset())
    }
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self::with_check_delay(client, Some(CHECK_DELAY))
    }

    /// A backend that type checks documents once their edits have settled for
    /// `check_delay`, or never.
    fn with_check_delay(client: Client, check_delay: Option<Duration>) -> Self {
        let documents = Arc::<Mutex<Documents>>::default();
        let (worker, requests) = mpsc::channel();
        let (events, applied) = unbounded_channel();
        let worker_state = Worker {
            events,
            documents: documents.clone(),
            check_delay,
            pending: Vec::new(),
        };
        // The thread exits once the backend drops its sender, and the applier
        // once the thread drops its own
        thread::Builder::new()
            .name("dolang-lsp-compile".to_owned())
            .stack_size(8 << 20)
            .spawn(move || worker_state.run(requests))
            .expect("failed to spawn the compile worker");
        tokio::spawn(publish(client.clone(), documents.clone(), applied));
        Self {
            documents,
            worker,
            client,
            config: Default::default(),
            position_encoding: RwLock::new(PositionEncodingKind::UTF16),
        }
    }

    /// The projection of the newest revision of a document the client has
    /// sent, once the worker has made it.
    ///
    /// A request's positions refer to the text the client last sent, so
    /// answering from an older projection would misplace them.
    async fn document(&self, uri: &Uri) -> Option<Arc<Document>> {
        let revision = self.documents().handles.get(uri)?.revision.clone();
        if let Ok(document) = revision.await {
            return Some(document);
        }
        // The worker passed over the revision, superseded or failing to
        // compile it, so the last projection published is the best there is
        self.documents().handles.get(uri)?.projected.clone()
    }

    fn documents(&self) -> MutexGuard<'_, Documents> {
        self.documents.lock().expect("documents lock poisoned")
    }

    /// Sends a document's new revision to the worker, returning its future
    /// projection.
    fn submit(
        &self,
        uri: &Uri,
        workspace: &Arc<Workspace>,
        member: &Arc<Member>,
        encoding: &PositionEncodingKind,
    ) -> Revision {
        let (done, revision) = oneshot::channel();
        self.send(Request::Compile(Job {
            uri: uri.clone(),
            workspace: workspace.clone(),
            member: member.clone(),
            encoding: encoding.clone(),
            done,
        }));
        revision.shared()
    }

    /// Makes `text` the current revision of an open document as `version`,
    /// unless the client has sent a newer one, returning its future
    /// projection.
    fn revise(&self, uri: &Uri, version: i32, text: String) -> Option<Revision> {
        let mut documents = self.documents();
        let handle = documents.handles.get_mut(uri)?;
        let path = handle.path.clone();
        let member = handle.workspace.revise(uri.clone(), path, version, text)?;
        handle.revision = self.submit(uri, &handle.workspace, &member, &handle.encoding);
        handle.member = member;
        Some(handle.revision.clone())
    }

    fn send(&self, request: Request) {
        if self.worker.send(request).is_err() {
            log::error!("the compile worker has exited");
        }
    }

    fn choose_position_encoding(params: &InitializeParams) -> PositionEncodingKind {
        let offered = params
            .capabilities
            .general
            .as_ref()
            .and_then(|general| general.position_encodings.as_ref());
        if offered.is_some_and(|encodings| encodings.contains(&PositionEncodingKind::UTF8)) {
            PositionEncodingKind::UTF8
        } else {
            PositionEncodingKind::UTF16
        }
    }

    fn position_encoding(&self) -> PositionEncodingKind {
        self.position_encoding
            .read()
            .expect("position encoding lock poisoned")
            .clone()
    }
}

/// Projects a compiled document into what requests answer from.
fn project(
    uri: &Uri,
    member: Arc<Member>,
    unit: Arc<Unit<'static>>,
    encoding: &PositionEncodingKind,
) -> Document {
    let mut diags = Vec::new();
    let mut tokens = Vec::new();
    let mut refs = Vec::new();
    let mut decls: HashMap<NodeId, Decl> = HashMap::new();
    let mut patches = Vec::new();

    let content = unit.source();
    let index = DocumentIndex::new(content, encoding.clone());
    let hovers = build_hovers(&unit, content);
    for diag in unit.diagnostics() {
        let relate = |_, span: &diag::SourceSpan, message: String| DiagnosticRelatedInformation {
            location: Location::new(uri.clone(), index.range_from_span(&span.span())),
            message,
        };
        convert_diag(&index, &diag, relate, &mut diags);

        let diagnostic_range = index.range_from_span(&diag.span().span());
        let diagnostic_message = diag.message().to_string();
        let diagnostic_severity = severity(diag.severity());

        for patch in diag.patches() {
            patches.push(Patch {
                diagnostic_range,
                diagnostic_severity,
                diagnostic_message: diagnostic_message.clone(),
                patch_range: index.range_from_span(&patch.span().span()),
                replacement: patch.sub().to_string(),
                title: patch.message().to_string(),
            });
        }
    }
    let statics = static_fields(&unit);
    unit.tokens(
        &mut |leaf, span: diag::Span, node: Option<NodeId>, context: Context| {
            // Punctuation and sigils have no semantic token type, so
            // they are left to the client's own syntax highlighting.
            if span.start().byte_offset() != span.end().byte_offset()
                && !matches!(leaf, Token::Delim | Token::Sigil)
            {
                let doc_node = node.and_then(|id| unit.node(id));
                let kind = doc_node.map(|node| node.kind());
                let (token_type, mut modifiers) = classify_token(leaf, kind.as_ref(), context);
                if let Some(id) = node {
                    let range = index.range_from_span(&span);
                    match doc_node.and_then(|node| node.definition()) {
                        Some(def) => {
                            modifiers |= declaration_modifiers(&span, &def, id, &statics);
                            refs.push((range, id));
                            decls
                                .entry(id)
                                .or_insert_with(|| Decl {
                                    name_range: index.range_from_span(&def),
                                    uses: Vec::new(),
                                    hover: hovers.get(&id).cloned().unwrap_or_default(),
                                })
                                .uses
                                .push(range);
                        }
                        // A prelude binding has no source text, so
                        // there is nowhere in this file to jump to --
                        // but it may still have hover text pulled
                        // from the static doc index (see
                        // external_hover), which has nothing to do
                        // with a definition span. Index it too, using
                        // its own span as a stand-in "declaration"
                        // location since there is no real one; this
                        // also gets it references/highlight for free.
                        None if hovers.contains_key(&id) => {
                            refs.push((range, id));
                            decls
                                .entry(id)
                                .or_insert_with(|| Decl {
                                    name_range: range,
                                    uses: Vec::new(),
                                    hover: hovers.get(&id).cloned().unwrap_or_default(),
                                })
                                .uses
                                .push(range);
                        }
                        None => {}
                    }
                }
                tokens.push((token_type, modifiers, span));
            }
        },
    );

    let mut pre_line = 0;
    let mut pre_start = 0;

    tokens.sort_by_key(|(_, _, range)| range.start().byte_offset());
    refs.sort_by_key(|(range, _)| range.start);
    for decl in decls.values_mut() {
        decl.uses.sort_by_key(|range| range.start);
    }
    let symbols = build_symbols(&unit, &index);

    let tokens = tokens
        .into_iter()
        .map(|(token_type, modifiers, range)| {
            let start = index.position_from_offset(range.start().byte_offset());
            let delta_line = start.line - pre_line;
            let token = SemanticToken {
                delta_line,
                delta_start: if delta_line == 0 {
                    start.character - pre_start
                } else {
                    start.character
                },
                length: index.token_length(&range),
                token_type,
                token_modifiers_bitset: modifiers,
            };
            pre_line = start.line;
            pre_start = start.character;
            token
        })
        .collect();

    Document {
        member,
        unit,
        diagnostics: diags,
        tokens,
        refs,
        decls,
        symbols,
        patches,
        analyzed: Mutex::new(None),
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let position_encoding = Self::choose_position_encoding(&params);
        {
            let mut guard = self.config.lock().await;
            guard.root = params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first())
                .and_then(|workspace| uri_to_file_path(&workspace.uri))
                .map(Cow::into_owned);
            if let Some(root) = guard.root.as_deref() {
                log::info!("project root: {}", root.display())
            } else {
                log::info!("project root: <not specified>")
            }
            guard.workspaces = params
                .workspace_folders
                .as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|w| uri_to_file_path(&w.uri).map(Cow::into_owned))
                .collect();
            for workspace in guard.workspaces.iter() {
                log::info!("workspace: {}", workspace.display())
            }
            guard.watch = (params.capabilities.workspace.as_ref())
                .and_then(|workspace| workspace.did_change_watched_files.as_ref())
                .and_then(|watched| watched.dynamic_registration)
                .unwrap_or(false);
        }
        *self
            .position_encoding
            .write()
            .expect("position encoding lock poisoned") = position_encoding.clone();
        Ok(InitializeResult {
            server_info: None,
            offset_encoding: (position_encoding == PositionEncodingKind::UTF8)
                .then(|| "utf-8".to_owned()),
            capabilities: ServerCapabilities {
                position_encoding: Some(position_encoding),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                        SemanticTokensRegistrationOptions {
                            text_document_registration_options: TextDocumentRegistrationOptions {
                                document_selector: Some(vec![DocumentFilter {
                                    language: Some("dolang".to_string()),
                                    scheme: Some("file".to_string()),
                                    pattern: None,
                                }]),
                            },
                            semantic_tokens_options: SemanticTokensOptions {
                                work_done_progress_options: WorkDoneProgressOptions::default(),
                                legend: SemanticTokensLegend {
                                    token_types: LEGEND_TYPES.to_vec(),
                                    token_modifiers: TOKEN_MODIFIERS.to_vec(),
                                },
                                range: Some(false),
                                full: Some(SemanticTokensFullOptions::Bool(true)),
                            },
                            static_registration_options: StaticRegistrationOptions::default(),
                        },
                    ),
                ),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if !self.config.lock().await.watch {
            return;
        }
        let watchers = ["**/*.dol", "**/.dolang-lsp.toml"].map(|pattern| FileSystemWatcher {
            glob_pattern: GlobPattern::String(pattern.to_owned()),
            kind: None,
        });
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: watchers.into(),
        };
        let registration = Registration {
            id: "watched-files".to_owned(),
            method: "workspace/didChangeWatchedFiles".to_owned(),
            register_options: Some(serde_json::to_value(options).expect("options serialize")),
        };
        if let Err(err) = self.client.register_capability(vec![registration]).await {
            log::error!("failed to watch files: {err}");
        }
    }

    // Opening, changing and closing a document update the tables before
    // their first `await`, so they apply in the order the client sent them
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let TextDocumentItem {
            uri, text, version, ..
        } = params.text_document;
        let Some(path) = source_path(&uri) else {
            return;
        };
        let encoding = self.position_encoding();
        let mut documents = self.documents();
        // Opened again without a close, it starts over
        if let Some(old) = documents.handles.remove(&uri) {
            old.workspace.close(&old.path);
            self.send(Request::Due(old.workspace));
        }
        let workspace = documents.workspaces.open(&path);
        // Unless another URI has a newer version of the same file open
        if let Some(member) = workspace.revise(uri.clone(), path.clone(), version, text) {
            let revision = self.submit(&uri, &workspace, &member, &encoding);
            let handle = Handle {
                workspace,
                path,
                encoding,
                member,
                revision,
                projected: None,
            };
            documents.handles.insert(uri, handle);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let VersionedTextDocumentIdentifier { uri, version } = params.text_document;
        let text = params.content_changes.into_iter().next().unwrap().text;
        self.revise(&uri, version, text);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(text) = params.text else {
            return;
        };
        // The saved text, as the version the document is at
        let version =
            (self.documents().handles.get(&uri)).and_then(|handle| handle.member.version());
        let Some(saved) = version.and_then(|version| self.revise(&uri, version, text)) else {
            return;
        };
        // Refresh once the saved text's tokens are published, or the worker
        // has passed over it
        _ = saved.await;
        _ = self.client.semantic_tokens_refresh().await;
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(document) = self.document(&params.text_document.uri).await else {
            return Ok(None);
        };
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            data: document.tokens.clone(),
            ..Default::default()
        })))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        let Some(document) = self.document(&params.text_document.uri).await else {
            return Ok(None);
        };

        let patches = &document.patches;
        let mut actions = Vec::new();

        for patch in patches {
            let cursor_in_diagnostic =
                range_contains_position(patch.diagnostic_range, params.range.start);
            let cursor_in_patch = range_contains_position(patch.patch_range, params.range.start);

            if cursor_in_diagnostic || cursor_in_patch {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: patch.title.clone(),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some({
                            let mut changes = std::collections::HashMap::new();
                            changes.insert(
                                params.text_document.uri.clone(),
                                vec![TextEdit {
                                    range: patch.patch_range,
                                    new_text: patch.replacement.clone(),
                                }],
                            );
                            changes
                        }),
                        ..Default::default()
                    }),
                    diagnostics: Some(vec![Diagnostic {
                        range: patch.diagnostic_range,
                        severity: Some(patch.diagnostic_severity),
                        message: patch.diagnostic_message.clone(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }));
            }
        }

        Ok(Some(actions))
    }

    async fn shutdown(&self) -> Result<()> {
        log::debug!("shutting down");
        Ok(())
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let handle = self.documents().handles.remove(&params.text_document.uri);
        if let Some(handle) = handle {
            handle.workspace.close(&handle.path);
            self.send(Request::Due(handle.workspace));
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        log::debug!("change config: {params:?}")
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        log::debug!("change workspace: {params:?}")
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut configs = Vec::new();
        for event in params.changes {
            let Some(path) = uri_to_file_path(&event.uri).map(Cow::into_owned) else {
                continue;
            };
            if path
                .file_name()
                .is_some_and(|name| name == CONFIG_FILE_NAME)
            {
                configs.extend(path.parent().map(Path::to_owned));
            }
        }
        let mut documents = self.documents();
        let Documents {
            handles,
            workspaces,
        } = &mut *documents;
        // A check looks at the files its workspace loaded or looked for
        for workspace in workspaces.live() {
            self.send(Request::Due(workspace));
        }
        // A changed configuration moves the documents of the workspaces it
        // affects to the workspaces they have now
        for dir in &configs {
            workspaces.retire(dir);
        }
        for (uri, handle) in handles {
            if workspaces.current(&handle.workspace) {
                continue;
            }
            handle.workspace.close(&handle.path);
            let workspace = workspaces.open(&handle.path);
            let (path, member) = (handle.path.clone(), &handle.member);
            let (Some(version), Some(text)) = (member.version(), member.text()) else {
                continue;
            };
            let text = text.to_owned();
            let Some(member) = workspace.revise(uri.clone(), path, version, text) else {
                continue;
            };
            handle.revision = self.submit(uri, &workspace, &member, &handle.encoding);
            handle.workspace = workspace;
            handle.member = member;
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some(document) = self
            .document(&params.text_document_position_params.text_document.uri)
            .await
        else {
            return Ok(None);
        };
        let pos = &params.text_document_position_params.position;
        let encoding = self.position_encoding();
        let reference = document.reference_at(pos);
        let index = DocumentIndex::new(document.unit.source(), encoding.clone());
        let analyzed = document.analyzed();
        let mut found: Vec<Location> = Vec::new();
        if let (Some(analyzed), Some(offset)) =
            (analyzed.as_deref(), index.offset_from_position(pos))
        {
            let (targets, chosen) =
                document.targets(analyzed, offset, reference.map(|(_, id, _)| id));
            for target in &targets {
                let Some(location) = location(target, chosen, encoding.clone()) else {
                    continue;
                };
                if !found.contains(&location) {
                    found.push(location);
                }
            }
        }
        match found.len() {
            0 => {}
            1 => return Ok(found.pop().map(GotoDefinitionResponse::Scalar)),
            _ => return Ok(Some(GotoDefinitionResponse::Array(found))),
        }
        // A name declared here, or in a module with no source
        let Some((_, _, decl)) = reference else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: params
                .text_document_position_params
                .text_document
                .uri
                .clone(),
            range: decl.name_range,
        })))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let Some(document) = self
            .document(&params.text_document_position_params.text_document.uri)
            .await
        else {
            return Ok(None);
        };
        let pos = &params.text_document_position_params.position;
        let reference = document.reference_at(pos);
        let index = DocumentIndex::new(document.unit.source(), self.position_encoding());
        let referenced = reference.and_then(|(range, ..)| {
            let start = index.offset_from_position(&range.start)?;
            let end = index.offset_from_position(&range.end)?;
            Some(source::Span {
                start: start.try_into().ok()?,
                end: end.try_into().ok()?,
            })
        });
        let analyzed = document.analyzed();
        // A module has no node of its own, so the check's is the path through
        // it, which says nothing about the module
        let module = reference.is_some_and(|(_, id, _)| {
            let kind = document.unit.node(id).map(|node| node.kind());
            matches!(
                kind,
                Some(Kind::ImportModule { .. } | Kind::PreludeModule { .. })
            )
        });
        let (checked, (targets, chosen)) =
            match (analyzed.as_deref(), index.offset_from_position(pos)) {
                (Some(analyzed), Some(offset)) => (
                    (!module)
                        .then(|| document.checked_at(analyzed, offset, referenced))
                        .flatten(),
                    document.targets(analyzed, offset, reference.map(|(_, id, _)| id)),
                ),
                _ => (None, (Vec::new(), None)),
            };
        // What a check found has its own docs, each union alternative's in
        // turn; any other reference's were described with the document
        let own = analyzed.as_deref().map(|analyzed| &analyzed.name[..]);
        let found: Vec<_> = targets
            .iter()
            .map(|target| target_hover(target, chosen, own))
            .collect();
        let (docs, complete) = match found.iter().any(|docs| !docs.is_empty()) {
            true => {
                let complete = found.iter().all(|docs| !docs.is_empty());
                (found.into_iter().flatten().collect(), complete)
            }
            false => {
                let docs = reference.map(|(_, _, decl)| decl.hover.clone());
                (docs.unwrap_or_default(), true)
            }
        };
        // Signatures say what the checked type would
        let signatures = complete
            && !docs.is_empty()
            && docs.iter().all(|doc| matches!(doc.shape, Shape::Signature));
        let render = |typed| {
            let docs: Vec<_> = docs.iter().filter_map(|doc| doc.render(typed)).collect();
            (!docs.is_empty()).then(|| docs.join("\n\n---\n\n"))
        };
        let reference = reference.map(|(range, ..)| range);
        let (range, value) = match checked {
            None => match (reference, render(false)) {
                (Some(range), Some(markdown)) => (range, markdown),
                _ => return Ok(None),
            },
            Some((span, lines)) => {
                let typed = format!("```dolang\n{}\n```", lines.join("\n"));
                let range = reference.unwrap_or_else(|| {
                    index.range_from_offsets(span.start as usize, span.end as usize)
                });
                match render(!signatures) {
                    Some(markdown) if signatures => (range, markdown),
                    Some(markdown) => (range, format!("{typed}\n\n---\n\n{markdown}")),
                    None => (range, typed),
                }
            }
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(document) = self.document(&params.text_document.uri).await else {
            return Ok(None);
        };
        let symbols = document.symbols.clone();
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let Some(document) = self.document(uri).await else {
            return Ok(None);
        };
        let Some(decl) = document.decl_at(&params.text_document_position.position) else {
            return Ok(None);
        };
        let include_declaration = params.context.include_declaration;
        Ok(Some(
            decl.uses
                .iter()
                .filter(|range| include_declaration || **range != decl.name_range)
                .map(|range| Location::new(uri.clone(), *range))
                .collect(),
        ))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let Some(document) = self.document(uri).await else {
            return Ok(None);
        };
        let Some(decl) = document.decl_at(&params.text_document_position_params.position) else {
            return Ok(None);
        };
        Ok(Some(
            decl.uses
                .iter()
                .map(|range| DocumentHighlight {
                    range: *range,
                    // The only write a single file can be sure of is the one
                    // that introduced the name.
                    kind: Some(if *range == decl.name_range {
                        DocumentHighlightKind::WRITE
                    } else {
                        DocumentHighlightKind::TEXT
                    }),
                })
                .collect(),
        ))
    }
}

pub(crate) fn build_service() -> (LspService<Backend>, ClientSocket) {
    LspService::new(Backend::new)
}

fn uri_to_file_path(uri: &Uri) -> Option<Cow<'_, Path>> {
    (uri.scheme().as_str() == "file")
        .then_some(())
        .and_then(|()| uri.to_file_path())
}

/// The path of a document the server compiles.
fn source_path(uri: &Uri) -> Option<PathBuf> {
    let path = uri_to_file_path(uri)?;
    is_dolang_source(&path).then(|| path.into_owned())
}

fn is_dolang_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        None | Some("dol")
    )
}

fn range_contains_position(range: Range, position: Position) -> bool {
    (range.start.line < position.line
        || (range.start.line == position.line && range.start.character <= position.character))
        && (range.end.line > position.line
            || (range.end.line == position.line && range.end.character >= position.character))
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use futures::{SinkExt, StreamExt};
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};
    use tower::{Service, ServiceExt};
    use tower_lsp_server::jsonrpc::{Request, Response};
    use tower_lsp_server::ls_types::{notification, request};
    use tower_lsp_server::ls_types::{notification::Notification as _, request::Request as _};

    use super::*;
    use crate::{module_path::tests::Directory, typelib_index};

    struct Harness {
        service: LspService<Backend>,
        socket: ClientSocket,
        next_id: i64,
    }

    impl Harness {
        /// A server that doesn't type check, so each change publishes once.
        fn new() -> Self {
            Self::with_check_delay(None)
        }

        /// A server that type checks documents once edits settle for `delay`.
        fn checking(delay: Duration) -> Self {
            Self::with_check_delay(Some(delay))
        }

        fn with_check_delay(delay: Option<Duration>) -> Self {
            let (service, socket) =
                LspService::new(move |client| Backend::with_check_delay(client, delay));
            Self {
                service,
                socket,
                next_id: 1,
            }
        }

        async fn send_request<R>(&mut self, params: R::Params) -> R::Result
        where
            R: request::Request,
            R::Params: Serialize,
            R::Result: DeserializeOwned,
        {
            let id = self.next_id;
            self.next_id += 1;
            let response = self
                .service
                .ready()
                .await
                .unwrap()
                .call(
                    Request::build(R::METHOD)
                        .params(serde_json::to_value(params).unwrap())
                        .id(id)
                        .finish(),
                )
                .await
                .unwrap()
                .unwrap();
            let (_, body) = response.into_parts();
            serde_json::from_value(body.unwrap()).unwrap()
        }

        async fn send_notification<N>(&mut self, params: N::Params)
        where
            N: notification::Notification,
            N::Params: Serialize,
        {
            let response = self
                .service
                .ready()
                .await
                .unwrap()
                .call(
                    Request::build(N::METHOD)
                        .params(serde_json::to_value(params).unwrap())
                        .finish(),
                )
                .await
                .unwrap();
            assert!(response.is_none());
        }

        async fn next_client_request(&mut self) -> Request {
            tokio::time::timeout(Duration::from_secs(1), self.socket.next())
                .await
                .unwrap()
                .unwrap()
        }

        async fn next_client_notification<N>(&mut self) -> N::Params
        where
            N: notification::Notification,
            N::Params: DeserializeOwned,
        {
            let request = self.next_client_request().await;
            assert_eq!(request.method(), N::METHOD);
            assert!(request.id().is_none());
            serde_json::from_value(request.params().cloned().unwrap_or(Value::Null)).unwrap()
        }

        async fn initialize(&mut self, offered: Vec<PositionEncodingKind>) -> InitializeResult {
            let result = self
                .send_request::<request::Initialize>(InitializeParams {
                    capabilities: ClientCapabilities {
                        general: Some(GeneralClientCapabilities {
                            position_encodings: Some(offered),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await;
            self.send_notification::<notification::Initialized>(InitializedParams {})
                .await;
            result
        }

        async fn open(&mut self, uri: Uri, text: &str, version: i32) -> PublishDiagnosticsParams {
            self.send_notification::<notification::DidOpenTextDocument>(
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: "dolang".to_owned(),
                        version,
                        text: text.to_owned(),
                    },
                },
            )
            .await;
            self.next_client_notification::<notification::PublishDiagnostics>()
                .await
        }

        async fn change(&mut self, uri: Uri, text: &str, version: i32) {
            self.send_notification::<notification::DidChangeTextDocument>(
                DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier { uri, version },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: text.to_owned(),
                    }],
                },
            )
            .await;
        }

        async fn close(&mut self, uri: Uri) {
            self.send_notification::<notification::DidCloseTextDocument>(
                DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri },
                },
            )
            .await;
        }

        async fn watched(&mut self, changes: &[(&Path, FileChangeType)]) {
            let changes = (changes.iter())
                .map(|&(path, typ)| FileEvent::new(Uri::from_file_path(path).unwrap(), typ))
                .collect();
            self.send_notification::<notification::DidChangeWatchedFiles>(
                DidChangeWatchedFilesParams { changes },
            )
            .await;
        }

        /// The next diagnostics the server publishes for `uri`, passing over
        /// those of other documents.
        async fn next_publish_for(&mut self, uri: &Uri) -> PublishDiagnosticsParams {
            loop {
                let published = self.next_publish().await;
                if published.uri == *uri {
                    return published;
                }
            }
        }

        /// Asserts the server sends the client nothing more for a while.
        async fn quiet(&mut self) {
            let next = tokio::time::timeout(Duration::from_millis(250), self.socket.next()).await;
            assert!(next.is_err(), "unexpected message: {next:?}");
        }

        /// The next diagnostics the server publishes, waiting long enough for a
        /// debug build's check on a loaded machine.
        async fn next_publish(&mut self) -> PublishDiagnosticsParams {
            let request = tokio::time::timeout(Duration::from_secs(30), self.socket.next())
                .await
                .expect("no diagnostics published")
                .unwrap();
            assert_eq!(request.method(), notification::PublishDiagnostics::METHOD);
            serde_json::from_value(request.params().cloned().unwrap()).unwrap()
        }
    }

    /// Undo the delta encoding so a token can be named by where it starts.
    fn absolute_tokens(data: &[SemanticToken]) -> Vec<((u32, u32), u32, u32)> {
        let mut absolute = Vec::new();
        let (mut line, mut start) = (0, 0);
        for token in data {
            if token.delta_line != 0 {
                line += token.delta_line;
                start = 0;
            }
            start += token.delta_start;
            absolute.push((
                (line, start),
                token.token_type,
                token.token_modifiers_bitset,
            ));
        }
        absolute
    }

    fn token_at(absolute: &[((u32, u32), u32, u32)], line: u32, col: u32) -> (u32, u32) {
        absolute
            .iter()
            .find(|((l, c), _, _)| (*l, *c) == (line, col))
            .map(|(_, ty, modifiers)| (*ty, *modifiers))
            .unwrap_or_else(|| panic!("no token at {line}:{col} in {absolute:?}"))
    }

    async fn semantic_tokens(harness: &mut Harness, uri: Uri) -> Vec<((u32, u32), u32, u32)> {
        let tokens = harness
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri },
            })
            .await
            .unwrap();
        match tokens {
            SemanticTokensResult::Tokens(tokens) => absolute_tokens(&tokens.data),
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        }
    }

    async fn hover_at(harness: &mut Harness, uri: Uri, line: u32, character: u32) -> Option<Hover> {
        harness
            .send_request::<request::HoverRequest>(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position::new(line, character),
                },
                work_done_progress_params: Default::default(),
            })
            .await
    }

    /// A symbol reduced to what a test cares about: name, kind, and children.
    type Outline = Vec<(String, SymbolKind, Vec<(String, SymbolKind)>)>;

    fn outline(symbols: &[DocumentSymbol]) -> Outline {
        symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.name.clone(),
                    symbol.kind,
                    symbol
                        .children
                        .iter()
                        .flatten()
                        .map(|child| (child.name.clone(), child.kind))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn utf8_positions_use_byte_offsets_within_line() {
        let content = "a😀b\nx";
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF8);
        let offset = content.find('b').unwrap();

        assert_eq!(index.position_from_offset(offset), Position::new(0, 5));
    }

    #[test]
    fn utf16_positions_count_code_units_within_line() {
        let content = "a😀b\nx";
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF16);
        let offset = content.find('b').unwrap();

        assert_eq!(index.position_from_offset(offset), Position::new(0, 3));
    }

    #[test]
    fn utf16_token_length_counts_code_units() {
        let content = "😀x";
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF16);

        assert_eq!(index.token_length_from_offsets(0, "😀".len()), 2);
    }

    #[test]
    fn utf16_range_handles_mixed_content_lines() {
        let content = "pre😀fix\nsecond";
        let start = content.find("fix").unwrap();
        let end = start + "fix".len();
        let index = DocumentIndex::new(content, PositionEncodingKind::UTF16);

        assert_eq!(
            index.range_from_offsets(start, end),
            Range::new(Position::new(0, 5), Position::new(0, 8))
        );
    }

    #[test]
    fn doc_comments_preserve_markdown_after_removing_markers() {
        assert_eq!(
            normalize_doc_text("  # Summary.  \n  #\n  #   indented"),
            "Summary.\n\n  indented"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_negotiates_position_encoding() {
        let mut harness = Harness::new();
        let result = harness
            .initialize(vec![
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8,
            ])
            .await;

        assert_eq!(
            result.capabilities.position_encoding,
            Some(PositionEncodingKind::UTF8)
        );

        let mut harness = Harness::new();
        let result = harness.initialize(vec![PositionEncodingKind::UTF16]).await;

        assert_eq!(
            result.capabilities.position_encoding,
            Some(PositionEncodingKind::UTF16)
        );
        assert_eq!(
            result.capabilities.hover_provider,
            Some(HoverProviderCapability::Simple(true))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hover_shows_local_declarations_types_and_docs() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///hover-test.dol".parse().unwrap();
        let source = concat!(
            "\n",
            "# A widget.\n",
            "class Widget[T]\n",
            "  # The count.\n",
            "  pub field count @ Int = 0\n",
            "\n",
            "  # Gets the count.\n",
            "  pub def get[U] self -> Int\n",
            "    self.count\n",
            "\n",
            "# Builds one.\n",
            "pub def build[T, :K, ...R]\n",
            "  # Number of widgets.\n",
            "  value @ Int\n",
            "  :mode = fast\n",
            "  ...rest\n",
            "do -> Widget\n",
            "  let local @ Int = value\n",
            "  local\n",
            "\n",
            "let widget = Widget\n",
            "echo $widget.get()\n",
            "echo $ build 1\n",
            "\n",
            "#[final]\n",
            "pub class Leaf\n",
            "pub class Shape\n",
            "  #[getter]\n",
            "  pub def area _self -> Int\n",
            "    1\n",
        );
        harness.open(uri.clone(), source, 1).await;

        let markdown = |value: &str| {
            HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: value.to_owned(),
            })
        };

        let class_hover = hover_at(&mut harness, uri.clone(), 20, 14).await.unwrap();
        assert_eq!(
            class_hover.contents,
            markdown("```dolang\nclass Widget[T]\n```\n\nA widget.")
        );

        let final_hover = hover_at(&mut harness, uri.clone(), 25, 11).await.unwrap();
        assert_eq!(
            final_hover.contents,
            markdown("```dolang\n#[final] pub class Leaf\n```")
        );

        let getter_hover = hover_at(&mut harness, uri.clone(), 28, 11).await.unwrap();
        assert_eq!(
            getter_hover.contents,
            markdown("```dolang\n#[getter] pub def area _self -> Int\n```\n\n*in class `Shape`*")
        );

        let field_hover = hover_at(&mut harness, uri.clone(), 4, 13).await.unwrap();
        assert_eq!(
            field_hover.contents,
            markdown("```dolang\npub field count @ Int\n```\n\n*in class `Widget`*\n\nThe count.")
        );

        let method_hover = hover_at(&mut harness, uri.clone(), 7, 11).await.unwrap();
        assert_eq!(
            method_hover.contents,
            markdown(concat!(
                "```dolang\npub def get[U] self -> Int\n```\n\n",
                "*in class `Widget`*\n\nGets the count."
            ))
        );

        let function_hover = hover_at(&mut harness, uri.clone(), 22, 8).await.unwrap();
        assert_eq!(
            function_hover.contents,
            markdown(concat!(
                "```dolang\n",
                "pub def build[T, :K, ...R] value@Int :mode = … ...rest -> Widget\n",
                "```\n\n",
                "Builds one."
            ))
        );

        let parameter_hover = hover_at(&mut harness, uri.clone(), 17, 20).await.unwrap();
        assert_eq!(
            parameter_hover.contents,
            markdown("```dolang\nvalue @ Int\n```\n\nNumber of widgets.")
        );

        let local_hover = hover_at(&mut harness, uri.clone(), 18, 3).await.unwrap();
        assert_eq!(
            local_hover.contents,
            markdown("```dolang\nlet local @ Int\n```")
        );

        assert!(hover_at(&mut harness, uri, 0, 3).await.is_none());
    }

    /// Regression test: a prelude/import binding has no `definition()` span
    /// (see `Kind::definition`), so the token-indexing loop used to skip it
    /// entirely -- meaning `external_hover`'s doc-index lookup ran and built
    /// a markdown string that nothing in `refs`/`decls` ever pointed at, and
    /// hover silently returned `None` for every prelude/import identifier.
    #[tokio::test(flavor = "current_thread")]
    async fn hover_resolves_prelude_items_from_the_static_doc_index() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///prelude-hover.dol".parse().unwrap();
        harness.open(uri.clone(), "echo hello\n", 1).await;

        let hover = hover_at(&mut harness, uri, 0, 2).await.unwrap();
        assert_eq!(
            hover.contents,
            HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: concat!(
                    "```dolang\ndef echo ...args -> nil\n```\n\n",
                    "*in module `term`*\n\nWrites to standard output."
                )
                .to_owned(),
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hover_resolves_type_only_prelude_items() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///type-prelude-hover.dol".parse().unwrap();
        harness
            .open(
                uri.clone(),
                "def fail() -> Never\n  throw std.ValueError \"failed\"\n",
                1,
            )
            .await;
        let tokens = semantic_tokens(&mut harness, uri.clone()).await;
        assert_eq!(token_at(&tokens, 0, 14), (TT_TYPE, MOD_PRELUDE));
        let hover = hover_at(&mut harness, uri, 0, 14).await.unwrap();
        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup")
        };
        assert!(content.value.contains("@let Never = Union[]"));
        assert!(content.value.contains("no values"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hover_returns_the_utf16_range_of_the_reference() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///hover-range-test.dol".parse().unwrap();
        let source = "let value = 1\necho 😀$value\n";
        harness.open(uri.clone(), source, 1).await;

        let hover = hover_at(&mut harness, uri, 1, 9).await.unwrap();
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(1, 8), Position::new(1, 13)))
        );

        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF8]).await;
        let uri: Uri = "file:///hover-range-utf8-test.dol".parse().unwrap();
        harness.open(uri.clone(), source, 1).await;

        let hover = hover_at(&mut harness, uri, 1, 11).await.unwrap();
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(1, 10), Position::new(1, 15)))
        );
    }

    /// The markdown of a hover, or `None` without one.
    async fn hover_text(
        harness: &mut Harness,
        uri: Uri,
        line: u32,
        character: u32,
    ) -> Option<String> {
        let hover = hover_at(harness, uri, line, character).await?;
        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup: {:?}", hover.contents);
        };
        Some(content.value)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hover_shows_the_checked_type_of_a_variable() {
        let mut harness = Harness::checking(Duration::from_millis(50));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///checked-hover.dol".parse().unwrap();
        let text = "let w = 0\n# The x\nlet x = 1\nlet y = x\n";
        harness.open(uri.clone(), text, 1).await;
        let unchecked = hover_text(&mut harness, uri.clone(), 2, 4).await.unwrap();
        assert_eq!(unchecked, "```dolang\nlet x\n```\n\nThe x");
        harness.next_publish().await;

        // The type says what `let x` would, so only the doc comment follows
        let typed = "```dolang\nx @ std.Int\n```\n\n---\n\nThe x";
        // The binding and the use
        let bound = hover_text(&mut harness, uri.clone(), 2, 4).await.unwrap();
        assert_eq!(bound, typed);
        let read = hover_text(&mut harness, uri.clone(), 3, 8).await.unwrap();
        assert_eq!(read, typed);
        // Without a doc comment, the type is all there is
        let y = hover_text(&mut harness, uri.clone(), 3, 4).await.unwrap();
        assert_eq!(y, "```dolang\ny @ std.Int\n```");
        // A keyword has no node
        assert_eq!(hover_text(&mut harness, uri, 3, 1).await, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hover_shows_a_narrowed_type_in_a_branch() {
        // Only `dodo lsp-test` builds with the bundled typelibs
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let mut harness = Harness::checking(Duration::from_millis(50));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///narrowed-hover.dol".parse().unwrap();
        let text = "def f v @ (Int | Str)\n  if (type(v, Int))\n    echo $v\n";
        harness.open(uri.clone(), text, 1).await;
        harness.next_publish().await;

        let typed = |hover: Option<String>| {
            let hover = hover.unwrap();
            hover.lines().nth(1).unwrap().to_owned()
        };
        let param = hover_text(&mut harness, uri.clone(), 0, 6).await;
        assert_eq!(typed(param), "v @ std.Int | std.Str");
        let narrowed = hover_text(&mut harness, uri, 2, 10).await;
        assert_eq!(typed(narrowed), "v @ std.Int");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hover_shows_members_calls_and_values() {
        // Only `dodo lsp-test` builds with the bundled typelibs
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let mut harness = Harness::checking(Duration::from_millis(50));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///member-hover.dol".parse().unwrap();
        let text = "\
class Box
  pub field item @ Int = 0
  pub def doubled self -> Int
    (self.item * 2)
@def twice x @ Int -> Int
@def twice x @ Str -> Str
def twice x
  x
def pick[T] x @ T -> T
  x
def uses b @ Box n @ Int
  let got = b.item
  let method = b.doubled()
  let overloaded = twice(\"s\")
  let generic = pick $n
  let sum = (n + 1)
  [got, method, overloaded, generic, sum]
";
        harness.open(uri.clone(), text, 1).await;
        harness.next_publish().await;

        let typed = async |harness: &mut Harness, line, character| {
            let hover = hover_text(harness, uri.clone(), line, character).await;
            let hover = hover.unwrap();
            let block = hover.split("```").nth(1).unwrap();
            block
                .strip_prefix("dolang\n")
                .unwrap()
                .trim_end()
                .to_owned()
        };
        // A class's signature says what its type would
        let class = hover_text(&mut harness, uri.clone(), 0, 7).await;
        assert_eq!(class.unwrap(), "```dolang\nclass Box\n```");
        // A field's type at the use
        assert_eq!(
            typed(&mut harness, 11, 14).await,
            "member-hover.Box.item @ std.Int"
        );
        // A method found by the check is its declared signature
        assert_eq!(
            typed(&mut harness, 12, 17).await,
            "pub def doubled self -> Int"
        );
        // The paren of a call to an overloaded function
        assert_eq!(
            typed(&mut harness, 13, 24).await,
            "(std.Str) -> std.Str  # overload 2"
        );
        // The space of a juxtaposed call
        assert_eq!(typed(&mut harness, 14, 20).await, "(std.Int) -> std.Int");
        // An operator's value
        assert_eq!(typed(&mut harness, 15, 15).await, "std.Int");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn did_open_publishes_diagnostics() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///diagnostic-test.dol".parse().unwrap();
        let diagnostics = harness.open(uri, "\"\\q\"", 1).await;

        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "unexpected escape sequence"
        );
        assert_eq!(
            diagnostics.diagnostics[0].range,
            Range::new(Position::new(0, 1), Position::new(0, 3))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_tokens_use_utf8_and_utf16_lengths() {
        let source = "# 😀\n";
        let uri: Uri = "file:///semantic-token-test.dol".parse().unwrap();

        let mut utf8 = Harness::new();
        utf8.initialize(vec![PositionEncodingKind::UTF8]).await;
        utf8.open(uri.clone(), source, 1).await;
        let utf8_tokens = utf8
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await
            .unwrap();

        let mut utf16 = Harness::new();
        utf16.initialize(vec![PositionEncodingKind::UTF16]).await;
        utf16.open(uri, source, 1).await;
        let utf16_tokens = utf16
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier {
                    uri: "file:///semantic-token-test.dol".parse().unwrap(),
                },
            })
            .await
            .unwrap();

        let utf8_data = match utf8_tokens {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        };
        let utf16_data = match utf16_tokens {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        };

        assert_eq!(utf8_data.len(), 1);
        assert_eq!(utf16_data.len(), 1);
        assert_eq!(utf8_data[0].length, 6);
        assert_eq!(utf16_data[0].length, 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn binding_else_definitions_and_tokens() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///binding-else-test.dol".parse().unwrap();
        let source = concat!(
            "let name = 1\n",
            "let Int(name) = nil\n",
            "else throw std.ValueError (str name)\n",
            "echo $name\n",
        );
        harness.open(uri.clone(), source, 1).await;
        for (line, col, expected) in [
            (2, 31, Range::new(Position::new(0, 4), Position::new(0, 8))),
            (3, 6, Range::new(Position::new(1, 8), Position::new(1, 12))),
        ] {
            let response = harness
                .send_request::<request::GotoDefinition>(GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position: Position::new(line, col),
                    },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                })
                .await
                .unwrap();
            match response {
                GotoDefinitionResponse::Scalar(location) => assert_eq!(location.range, expected),
                other => panic!("unexpected definition response: {other:?}"),
            }
        }
        let tokens = harness
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri },
            })
            .await
            .unwrap();
        let SemanticTokensResult::Tokens(tokens) = tokens else {
            panic!("partial tokens")
        };
        let absolute = absolute_tokens(&tokens.data);
        assert_eq!(token_at(&absolute, 2, 0), (TT_KEYWORD, 0));
    }

    /// Go-to-definition for the kinds of name a document node can describe.
    ///
    /// The old origin annotations distinguished these variants too, but only the
    /// `let` case was covered; each arm here is a separate `Kind` the server
    /// must map back to a declaration span.
    #[tokio::test(flavor = "current_thread")]
    async fn goto_definition_covers_every_kind_of_declaration() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///definition-kinds-test.dol".parse().unwrap();
        let source = concat!(
            "import std:\n",
            "  - str\n",
            "\n",
            "def double n\n",
            "  (n * 2)\n",
            "\n",
            "class Box\n",
            "  pub field v = 0\n",
            "\n",
            "  pub def get self\n",
            "    self.v\n",
            "\n",
            "echo $ str $ double 2\n",
            "let b = Box\n",
            "echo $b.get()\n",
        );
        harness.open(uri.clone(), source, 1).await;

        let mut definition_at = async |line: u32, col: u32| -> Range {
            let response = harness
                .send_request::<request::GotoDefinition>(GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position: Position::new(line, col),
                    },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                })
                .await
                .unwrap();
            match response {
                GotoDefinitionResponse::Scalar(location) => location.range,
                other => panic!("unexpected definition response shape: {other:?}"),
            }
        };

        // A parameter, from its use in the body.
        assert_eq!(
            definition_at(4, 3).await,
            Range::new(Position::new(3, 11), Position::new(3, 12))
        );
        // A function, from a call.
        assert_eq!(
            definition_at(12, 14).await,
            Range::new(Position::new(3, 4), Position::new(3, 10))
        );
        // An imported item, from a call.
        assert_eq!(
            definition_at(12, 8).await,
            Range::new(Position::new(1, 4), Position::new(1, 7))
        );
        // A class, from the name that constructs it.
        assert_eq!(
            definition_at(13, 9).await,
            Range::new(Position::new(6, 6), Position::new(6, 9))
        );
        // `self`, from its use in a method body.
        assert_eq!(
            definition_at(10, 5).await,
            Range::new(Position::new(9, 14), Position::new(9, 18))
        );
    }

    /// Semantic tokens carry the type and modifiers a client colors by.
    ///
    /// The token type for a name depends on the declaration it refers to, so
    /// this is what proves the node stream reaches the client.
    #[tokio::test(flavor = "current_thread")]
    async fn semantic_tokens_classify_names_by_what_they_refer_to() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///semantic-token-kind-test.dol".parse().unwrap();
        let source = concat!("def double n\n", "  (n * 2)\n", "\n", "echo $ double 2\n");
        harness.open(uri.clone(), source, 1).await;

        let tokens = harness
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri },
            })
            .await
            .unwrap();
        let data = match tokens {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        };

        let absolute = absolute_tokens(&data);
        let at = |line: u32, col: u32| token_at(&absolute, line, col);

        assert_eq!(at(0, 0), (TT_KEYWORD, 0));
        // The declared name and its use both classify as a function; only the
        // declaration carries the declaration bits.
        assert_eq!(at(0, 4), (TT_FUNCTION, MOD_DECLARATION | MOD_DEFINITION));
        assert_eq!(at(3, 7), (TT_FUNCTION, 0));
        // The parameter, at its declaration and in the body.
        assert_eq!(at(0, 11), (TT_PARAMETER, MOD_DECLARATION | MOD_DEFINITION));
        assert_eq!(at(1, 3), (TT_PARAMETER, 0));
        // `echo` is a prelude binding, which the client can style differently.
        assert_eq!(at(3, 0), (TT_FUNCTION, MOD_PRELUDE));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_tokens_classify_type_syntax() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///semantic-token-type-test.dol".parse().unwrap();
        let source = "def first[T] value@T -> Array[T]\n  value\n";
        harness.open(uri.clone(), source, 1).await;

        let tokens = harness
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri },
            })
            .await
            .unwrap();
        let data = match tokens {
            SemanticTokensResult::Tokens(tokens) => tokens.data,
            SemanticTokensResult::Partial(_) => panic!("unexpected partial tokens"),
        };
        let absolute = absolute_tokens(&data);
        let at = |col: u32| token_at(&absolute, 0, col);

        assert_eq!(
            at(10),
            (TT_TYPE_PARAMETER, MOD_DECLARATION | MOD_DEFINITION)
        );
        assert_eq!(at(18), (TT_OPERATOR, 0));
        assert_eq!(at(19), (TT_TYPE_PARAMETER, 0));
        assert_eq!(at(24), (TT_TYPE, 0));
        assert_eq!(at(30), (TT_TYPE_PARAMETER, 0));
    }

    /// The outline follows the node table's parentage, not the source nesting.
    ///
    /// `if`, `for` and the rest are containment rather than names, so a `let`
    /// written inside one is reported under the declaration that encloses it —
    /// which is the level an outline pane can navigate to.
    #[tokio::test(flavor = "current_thread")]
    async fn document_symbol_nests_declarations_under_what_declares_them() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///document-symbol-test.dol".parse().unwrap();
        let source = concat!(
            "import std:\n",
            "  - str\n",
            "\n",
            "def describe n\n",
            "  if (n > 0)\n",
            "    let sign = positive\n",
            "    echo $sign\n",
            "  str $n\n",
            "\n",
            "class Box\n",
            "  pub field v = 0\n",
            "\n",
            "  pub def (init) self v\n",
            "    self.v = v\n",
            "\n",
            "  pub def get self\n",
            "    self.v\n",
            "\n",
            "let b = Box 1\n",
        );
        harness.open(uri.clone(), source, 1).await;

        let response = harness
            .send_request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();
        let symbols = match response {
            DocumentSymbolResponse::Nested(symbols) => symbols,
            DocumentSymbolResponse::Flat(_) => panic!("expected a nested symbol response"),
        };

        assert_eq!(
            outline(&symbols),
            vec![
                ("str".to_owned(), SymbolKind::NAMESPACE, vec![]),
                (
                    "describe".to_owned(),
                    SymbolKind::FUNCTION,
                    // `sign` is written inside the `if`, which is not a symbol.
                    vec![("sign".to_owned(), SymbolKind::VARIABLE)],
                ),
                (
                    "Box".to_owned(),
                    SymbolKind::CLASS,
                    vec![
                        ("v".to_owned(), SymbolKind::FIELD),
                        ("(init)".to_owned(), SymbolKind::CONSTRUCTOR),
                        ("get".to_owned(), SymbolKind::METHOD),
                    ],
                ),
                ("b".to_owned(), SymbolKind::VARIABLE, vec![]),
            ]
        );

        // The name a client selects must lie within the range it reveals.
        let describe = &symbols[1];
        assert!(describe.range.start <= describe.selection_range.start);
        assert!(describe.selection_range.end <= describe.range.end);
    }

    /// A special method is named the way it is written, and `_` is not named.
    ///
    /// The node's name span covers the identifier alone, so the outline has to
    /// put the parentheses back to say the method implements a protocol.  A
    /// binding spelled `_` goes the other way: it is how a result is thrown
    /// away, so an outline pane has nothing to offer for it.
    #[tokio::test(flavor = "current_thread")]
    async fn document_symbol_parenthesizes_special_methods_and_omits_discards() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///document-symbol-names-test.dol".parse().unwrap();
        let source = concat!(
            "class Box\n",
            "  pub def (init) self\n",
            "    self\n",
            "\n",
            "  pub def (str) self\n",
            "    empty\n",
            "\n",
            "let _ = Box()\n",
            "let _kept = 1\n",
        );
        harness.open(uri.clone(), source, 1).await;

        let response = harness
            .send_request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();
        let symbols = match response {
            DocumentSymbolResponse::Nested(symbols) => symbols,
            DocumentSymbolResponse::Flat(_) => panic!("expected a nested symbol response"),
        };

        assert_eq!(
            outline(&symbols),
            vec![
                (
                    "Box".to_owned(),
                    SymbolKind::CLASS,
                    vec![
                        ("(init)".to_owned(), SymbolKind::CONSTRUCTOR),
                        ("(str)".to_owned(), SymbolKind::METHOD),
                    ],
                ),
                // `_` is discarded; only a name that starts with it survives.
                ("_kept".to_owned(), SymbolKind::VARIABLE, vec![]),
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn references_list_every_use_of_a_declaration() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///references-test.dol".parse().unwrap();
        let source = concat!("def square n\n", "  (n * n)\n", "\n", "echo $ square 3\n");
        harness.open(uri.clone(), source, 1).await;

        let mut references_at = async |line: u32, col: u32, declaration: bool| -> Vec<Range> {
            harness
                .send_request::<request::References>(ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position: Position::new(line, col),
                    },
                    context: ReferenceContext {
                        include_declaration: declaration,
                    },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                })
                .await
                .unwrap()
                .into_iter()
                .map(|location| location.range)
                .collect()
        };

        let uses = vec![
            Range::new(Position::new(1, 3), Position::new(1, 4)),
            Range::new(Position::new(1, 7), Position::new(1, 8)),
        ];
        let declaration = Range::new(Position::new(0, 11), Position::new(0, 12));

        // From a use of the parameter, and from its declaration: both name the
        // same node, so both answer with the same list.
        assert_eq!(references_at(1, 3, false).await, uses);
        assert_eq!(references_at(0, 11, false).await, uses);
        assert_eq!(
            references_at(1, 7, true).await,
            vec![declaration, uses[0], uses[1]]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn document_highlight_marks_the_declaration_as_a_write() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///document-highlight-test.dol".parse().unwrap();
        let source = concat!("def square n\n", "  (n * n)\n", "\n", "echo $ square 3\n");
        harness.open(uri.clone(), source, 1).await;

        let highlights = harness
            .send_request::<request::DocumentHighlightRequest>(DocumentHighlightParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position::new(1, 3),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();

        assert_eq!(
            highlights
                .into_iter()
                .map(|highlight| (highlight.range, highlight.kind))
                .collect::<Vec<_>>(),
            vec![
                (
                    Range::new(Position::new(0, 11), Position::new(0, 12)),
                    Some(DocumentHighlightKind::WRITE),
                ),
                (
                    Range::new(Position::new(1, 3), Position::new(1, 4)),
                    Some(DocumentHighlightKind::TEXT),
                ),
                (
                    Range::new(Position::new(1, 7), Position::new(1, 8)),
                    Some(DocumentHighlightKind::TEXT),
                ),
            ]
        );
    }

    /// A field's scope decorator colors its declaration.
    ///
    /// The decorator resolves to the prelude `class`, which is the same test
    /// the elaborator applies, so this cannot drift from the language.  Field
    /// *access* is dynamic — `self.total` names no declaration the compiler
    /// resolved — so only the declaring token carries anything.
    #[tokio::test(flavor = "current_thread")]
    async fn semantic_token_modifiers_mark_class_scoped_fields() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///token-modifier-test.dol".parse().unwrap();
        let source = concat!(
            "class Counter\n",
            "  #[class]\n",
            "  pub field total = 0\n",
            "\n",
            "  pub field n = 0\n",
            "\n",
            "  pub def bump self\n",
            "    self.total = (self.total + 1)\n",
            "    self.n = (self.n + 1)\n",
        );
        harness.open(uri.clone(), source, 1).await;

        let absolute = semantic_tokens(&mut harness, uri).await;
        let at = |line: u32, col: u32| token_at(&absolute, line, col);

        assert_eq!(
            at(2, 12),
            (TT_PROPERTY, MOD_DECLARATION | MOD_DEFINITION | MOD_STATIC)
        );
        // The instance field beside it is a declaration but not class-scoped.
        assert_eq!(at(4, 12), (TT_PROPERTY, MOD_DECLARATION | MOD_DEFINITION));
        // Accessing either resolves to nothing, so neither claims a modifier.
        assert_eq!(at(7, 9), (TT_PROPERTY, 0));
        assert_eq!(at(8, 9), (TT_PROPERTY, 0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn goto_definition_and_code_action_round_trip() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///definition-code-action-test.dol".parse().unwrap();
        let source = "let x = 5\nx\necho $x\n";
        let diagnostics = harness.open(uri.clone(), source, 1).await;

        assert!(!diagnostics.diagnostics.is_empty());
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "statement with no effect"
        );

        let definition = harness
            .send_request::<request::GotoDefinition>(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(2, 6),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();

        let location = match definition {
            GotoDefinitionResponse::Scalar(location) => location,
            _ => panic!("unexpected definition response shape"),
        };
        assert_eq!(
            location.range,
            Range::new(Position::new(0, 4), Position::new(0, 5))
        );

        let actions = harness
            .send_request::<request::CodeActionRequest>(CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range::new(Position::new(1, 0), Position::new(1, 0)),
                context: CodeActionContext {
                    diagnostics: diagnostics.diagnostics,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(action) => action,
            CodeActionOrCommand::Command(_) => panic!("unexpected command"),
        };
        assert_eq!(action.title, "add () to make this a call");
        let edit = action.edit.as_ref().unwrap();
        let changes = edit.changes.as_ref().unwrap();
        let edits = changes.values().next().unwrap();
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(1, 0), Position::new(1, 1))
        );
        assert_eq!(edits[0].new_text, "x()");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn requests_after_an_edit_see_it() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///edit-tokens-test.dol".parse().unwrap();
        harness.open(uri.clone(), "let a = 1\n", 1).await;

        // Asked before the edit's projection has been published
        harness
            .change(uri.clone(), "# moved\nlet a = 1\necho $a\n", 2)
            .await;
        let tokens = semantic_tokens(&mut harness, uri.clone()).await;
        assert_eq!(harness.next_publish().await.version, Some(2));
        assert_eq!(tokens, semantic_tokens(&mut harness, uri.clone()).await);
        token_at(&tokens, 2, 6);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn did_save_requests_semantic_token_refresh() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///save-refresh-test.dol".parse().unwrap();
        harness.open(uri.clone(), "def foo = 42\n", 1).await;

        let request = Request::build(notification::DidSaveTextDocument::METHOD)
            .params(json!(DidSaveTextDocumentParams {
                text: Some("def foo = 42\n".to_owned()),
                text_document: TextDocumentIdentifier { uri },
            }))
            .finish();
        let service = &mut harness.service;
        let socket = &mut harness.socket;

        let save = async move { service.ready().await.unwrap().call(request).await.unwrap() };
        let observe = async move {
            let published = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(published.method(), notification::PublishDiagnostics::METHOD);

            let refresh = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(refresh.method(), request::SemanticTokensRefresh::METHOD);

            let (_, id, _) = refresh.into_parts();
            socket
                .send(Response::from_ok(id.unwrap(), Value::Null))
                .await
                .unwrap();
        };

        let (save, ()) = tokio::join!(save, observe);
        assert_eq!(save, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn did_save_ignores_non_dolang_files() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let request = Request::build(notification::DidSaveTextDocument::METHOD)
            .params(json!(DidSaveTextDocumentParams {
                text: Some("# heading\n".to_owned()),
                text_document: TextDocumentIdentifier {
                    uri: "file:///note.md".parse().unwrap(),
                },
            }))
            .finish();

        let service = &mut harness.service;
        let socket = &mut harness.socket;
        let save = async move { service.ready().await.unwrap().call(request).await.unwrap() };
        let observe = async move {
            assert!(
                tokio::time::timeout(Duration::from_millis(250), socket.next())
                    .await
                    .is_err()
            );
        };

        let (save, ()) = tokio::join!(save, observe);
        assert_eq!(save, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_projection_is_discarded() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///stale-test.dol".parse().unwrap();
        let older = "# older\nlet older = 1\n";
        let newer = "let newer = 1\n";
        harness.open(uri.clone(), older, 1).await;
        let stale = harness.service.inner().document(&uri).await.unwrap();
        harness.change(uri.clone(), newer, 2).await;
        harness.next_publish().await;

        // A projection of the revision the edit replaced isn't stored
        let backend = harness.service.inner();
        let stored = backend.document(&uri).await.unwrap();
        let unit = stale.unit.clone();
        let document = project(
            &uri,
            stale.member.clone(),
            unit,
            &PositionEncodingKind::UTF16,
        );
        assert!(!backend.documents().project(&uri, &Arc::new(document)));
        let document = backend.document(&uri).await.unwrap();
        assert_eq!(document.unit.source(), newer);
        assert_eq!(document.tokens, stored.tokens);

        // Reopening starts over, whatever versions came before
        let reopened = harness.open(uri.clone(), older, 1).await;
        assert_eq!(reopened.version, Some(1));
        let document = harness.service.inner().document(&uri).await.unwrap();
        assert_eq!(document.unit.source(), older);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn out_of_order_changes_publish_the_newest() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///order-test.dol".parse().unwrap();
        harness.open(uri.clone(), "let a = 1\n", 1).await;

        harness.change(uri.clone(), "let c = 3\n", 3).await;
        harness.change(uri.clone(), "let b = 2\n", 2).await;
        let published = harness
            .next_client_notification::<notification::PublishDiagnostics>()
            .await;
        assert_eq!(published.version, Some(3));
        harness.quiet().await;
        let document = harness.service.inner().document(&uri).await.unwrap();
        assert_eq!(document.unit.source(), "let c = 3\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn did_close_forgets_the_document() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let closed: Uri = "file:///closed-test.dol".parse().unwrap();
        harness.open(closed.clone(), "let a = 1\n", 1).await;
        harness
            .send_notification::<notification::DidCloseTextDocument>(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: closed.clone(),
                },
            })
            .await;
        // Applied before the handler returns
        let tokens = harness
            .send_request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                text_document: TextDocumentIdentifier { uri: closed },
            })
            .await;
        assert!(tokens.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn check_publishes_after_compile_and_survives_save() {
        // Only `dodo lsp-test` builds with the bundled typelibs
        if typelib_index::lookup("math").is_none() {
            return;
        }
        let mut harness = Harness::checking(Duration::from_millis(50));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///check-test.dol".parse().unwrap();
        let text = "import math\nmath.sqrt \"a\"\n";
        let compiled = harness.open(uri.clone(), text, 1).await;
        assert_eq!(compiled.diagnostics, []);

        let checked = harness.next_publish().await;
        assert_eq!(checked.version, Some(1));
        let [diagnostic] = &checked.diagnostics[..] else {
            panic!("expected one diagnostic: {:#?}", checked.diagnostics);
        };
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(1, 10), Position::new(1, 13))
        );

        // Saving the checked text reuses the unit and keeps its check
        let request = Request::build(notification::DidSaveTextDocument::METHOD)
            .params(json!(DidSaveTextDocumentParams {
                text: Some(text.to_owned()),
                text_document: TextDocumentIdentifier { uri },
            }))
            .finish();
        let service = &mut harness.service;
        let socket = &mut harness.socket;
        let save = async move { service.ready().await.unwrap().call(request).await.unwrap() };
        let observe = async move {
            let published = socket.next().await.unwrap();
            let params: PublishDiagnosticsParams =
                serde_json::from_value(published.params().cloned().unwrap()).unwrap();
            let refresh = socket.next().await.unwrap();
            let (_, id, _) = refresh.into_parts();
            socket
                .send(Response::from_ok(id.unwrap(), Value::Null))
                .await
                .unwrap();
            params
        };
        let (save, saved) = tokio::join!(save, observe);
        assert_eq!(save, None);
        assert_eq!(saved.diagnostics, checked.diagnostics);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rapid_edits_check_only_the_last() {
        let mut harness = Harness::checking(Duration::from_millis(200));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///rapid-test.dol".parse().unwrap();
        harness.open(uri.clone(), "let a = 1\n", 1).await;
        harness.change(uri.clone(), "let b = 2\n", 2).await;
        harness.change(uri.clone(), "let c = 3\n", 3).await;

        // Until v3's check, after its projection
        let mut versions = Vec::new();
        while versions.iter().filter(|&&v| v == 3).count() < 2 {
            versions.push(harness.next_publish().await.version.unwrap());
        }
        harness.quiet().await;
        let count = |version| versions.iter().filter(|&&v| v == version).count();
        assert_eq!(count(1), 0, "{versions:?}");
        // Its projection, unless the worker coalesced it with the next
        assert!(count(2) <= 1, "{versions:?}");
        // Its projection and its check
        assert_eq!(count(3), 2, "{versions:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_check_is_discarded() {
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///stale-check-test.dol".parse().unwrap();
        harness.open(uri.clone(), "let a = 1\n", 2).await;

        let backend = harness.service.inner();
        let member = backend.document(&uri).await.unwrap().member.clone();
        assert!((backend.documents().checked(&uri, &member)).is_some());

        // Superseded by an edit
        harness.change(uri.clone(), "let b = 2\n", 3).await;
        harness.next_publish().await;
        let backend = harness.service.inner();
        assert!(backend.documents().checked(&uri, &member).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unbundled_import_is_reported() {
        let mut harness = Harness::checking(Duration::from_millis(50));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///unbundled-test.dol".parse().unwrap();
        let text = "import not_bundled\nnot_bundled.f 1\n";
        harness.open(uri, text, 1).await;

        // Once, at the import; the use recovers
        let checked = harness.next_publish().await;
        let reported: Vec<_> = (checked.diagnostics.iter())
            .map(|diag| (diag.range, diag.message.as_str()))
            .collect();
        let import = Range::new(Position::new(0, 7), Position::new(0, 18));
        assert_eq!(reported, [(import, "module `not_bundled` not found")]);
        harness.quiet().await;
    }

    /// The diagnostics of a document's check, as range and message.
    async fn checked(harness: &mut Harness, path: &Path, text: &str) -> Vec<(Range, String)> {
        let uri = Uri::from_file_path(path).unwrap();
        harness.open(uri, text, 1).await;
        let checked = harness.next_publish().await;
        (checked.diagnostics.into_iter())
            .map(|diag| (diag.range, diag.message))
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn imports_resolve_on_the_module_path() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        tree.write("lib/a.dol", "pub def f x@Int\n  x\n");
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let reported = checked(
            &mut harness,
            &tree.0.join("main.dol"),
            "import a\na.f \"s\"\n",
        )
        .await;
        let [(range, _)] = &reported[..] else {
            panic!("expected one diagnostic: {reported:#?}");
        };
        assert_eq!(*range, Range::new(Position::new(1, 4), Position::new(1, 7)));

        // Without the module path the module is unknown
        let other = Directory::new();
        other.write(CONFIG_FILE_NAME, "[prelude]\n");
        let reported = checked(&mut harness, &other.0.join("main.dol"), "import a\na.f 1\n").await;
        let import = Range::new(Position::new(0, 7), Position::new(0, 8));
        assert_eq!(reported, [(import, "module `a` not found".to_owned())]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn importers_check_against_open_buffers() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        tree.write("lib/a.dol", "import b\npub def g x@b.T\n  x\n");
        let b = Uri::from_file_path(tree.write("lib/b.dol", "pub @let T = Int\n")).unwrap();
        let main = Uri::from_file_path(tree.0.join("main.dol")).unwrap();
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(main.clone(), "import a\na.g \"s\"\n", 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics.len(), 1, "{:#?}", checked.diagnostics);

        // Through `a`, which is only on disk, to `b`'s buffer
        harness.open(b.clone(), "pub @let T = Str\n", 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);
        harness.change(b.clone(), "pub @let T = Int\n", 2).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics.len(), 1, "{:#?}", checked.diagnostics);
        harness.change(b.clone(), "pub @let T = Str\n", 3).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);

        // Back to the file
        harness.close(b).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics.len(), 1, "{:#?}", checked.diagnostics);
        harness.quiet().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn documents_sharing_modules_are_checked_together() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        tree.write("lib/a.dol", "pub def f x@Int\n  x\n");
        let one = Uri::from_file_path(tree.0.join("one.dol")).unwrap();
        let two = Uri::from_file_path(tree.0.join("two.dol")).unwrap();
        let mut harness = Harness::checking(Duration::from_millis(200));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(one.clone(), "import a\na.f \"s\"\n", 1).await;
        harness.open(two.clone(), "import a\na.f 1\n", 1).await;
        let mut checked = [harness.next_publish().await, harness.next_publish().await]
            .map(|published| (published.uri, published.diagnostics.len()));
        checked.sort_by_key(|(uri, _)| *uri == two);
        assert_eq!(checked, [(one.clone(), 1), (two.clone(), 0)]);

        let backend = harness.service.inner();
        let analyzed = async |uri| backend.document(uri).await.unwrap().analyzed().unwrap();
        let (one, two) = (analyzed(&one).await, analyzed(&two).await);
        assert!(Arc::ptr_eq(&one.analysis, &two.analysis));
        harness.quiet().await;
    }

    /// Where go to definition at a position goes
    async fn definition(
        harness: &mut Harness,
        uri: &Uri,
        line: u32,
        character: u32,
    ) -> Option<GotoDefinitionResponse> {
        harness
            .send_request::<request::GotoDefinition>(GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(line, character),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn definitions_and_docs_of_module_items() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        let a_text = "# A module\n\n# Doubles its argument\npub def f x @ Int -> Int\n  (x * 2)\n";
        let a = Uri::from_file_path(tree.write("lib/a.dol", a_text)).unwrap();
        tree.write("lib/re.dol", "pub import a:\n  - f\n");
        let main = Uri::from_file_path(tree.0.join("main.dol")).unwrap();
        let text = "\
import a
import re
import a:
  - f
a.f 1
re.f 1
f 1
";
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(main.clone(), text, 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);

        let at = |line| {
            let range = Range::new(Position::new(line, 8), Position::new(line, 9));
            Some(GotoDefinitionResponse::Scalar(Location::new(
                a.clone(),
                range,
            )))
        };
        // A path through the module, through a re-export, an imported item's
        // use, and the import itself
        for (line, character) in [(4, 2), (5, 3), (6, 0), (3, 4)] {
            let found = definition(&mut harness, &main, line, character).await;
            assert_eq!(found, at(3), "at {line}:{character}");
        }
        // The module's name is still its import
        let module = definition(&mut harness, &main, 4, 0).await;
        let import = Range::new(Position::new(0, 7), Position::new(0, 8));
        assert_eq!(
            module,
            Some(GotoDefinitionResponse::Scalar(Location::new(
                main.clone(),
                import
            )))
        );
        let hover = hover_text(&mut harness, main.clone(), 4, 2).await.unwrap();
        // A function's docs open with its signature, in place of its type
        assert!(
            hover.starts_with("```dolang\npub def f x @ Int -> Int\n```"),
            "{hover}"
        );
        assert!(hover.ends_with("Doubles its argument"), "{hover}");
        assert!(hover.contains("\n\n*in module `a`*\n\n"), "{hover}");
        // The module in a path, not the item the path names, has its source's
        // opening comment
        let module = hover_text(&mut harness, main.clone(), 4, 0).await;
        assert_eq!(module.unwrap(), "```dolang\nmodule a\n```\n\nA module");

        // The next check sees an edit move the item, checking both
        harness.open(a.clone(), &format!("\n{a_text}"), 1).await;
        let mut checked = [harness.next_publish().await, harness.next_publish().await]
            .map(|published| published.uri);
        checked.sort_by_key(|uri| *uri == main);
        assert_eq!(checked, [a.clone(), main.clone()]);
        let found = definition(&mut harness, &main, 4, 2).await;
        assert_eq!(found, at(4));
        harness.quiet().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn modules_have_the_configured_prelude() {
        // The prelude's modules are bundled only by `dodo lsp-test`
        if typelib_index::lookup("term").is_none() {
            return;
        }
        let tree = Directory::new();
        // No `[prelude]` table, so the shell's
        tree.write(CONFIG_FILE_NAME, "[modules]\npath = [\"lib\"]\n");
        tree.write("lib/a.dol", "pub def f x @ Int\n  echo $x\n");
        let main = Uri::from_file_path(tree.0.join("main.dol")).unwrap();
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(main.clone(), "import a\na.f 1\n", 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);
        harness.quiet().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn definitions_and_docs_of_members_and_overloads() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        let a_text = "\
# A module

pub class B
  # Its size
  pub field size = 0
  @def scale self x @ Int -> Int
  @def scale self x @ Str -> Str
  # Scales anything
  pub def scale self x
    x
pub class C: B
  pub def grow self
    nil
# Doubles a number
@def g x @ Int -> Int
# Doubles a string
@def g x @ Str -> Str
pub def g x
  (x + x)
import std:
  - getter
pub class D
  # Its area
  #[getter]
  pub def area _self -> Int
    1
";
        let a = Uri::from_file_path(tree.write("lib/a.dol", a_text)).unwrap();
        let main = Uri::from_file_path(tree.0.join("main.dol")).unwrap();
        let text = "\
import a
pub class Local
  pub def grow _self
    nil
# Twice a number
@def twice x @ Int -> Int
@def twice x @ Str -> Str
# Twice anything
def twice x
  (x + x)
pub def run l @ Local b @ a.B u @ (Local | a.C)
  l.grow()
  u.grow()
  b.scale(\"s\")
  twice(\"s\")
  a.g(1)
pub def size c @ a.C
  c.size
pub def area d @ a.D
  d.area
";
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(main.clone(), text, 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);

        let at = |uri: &Uri, line, start, end| {
            let range = Range::new(Position::new(line, start), Position::new(line, end));
            Location::new(uri.clone(), range)
        };
        let scalar = |location| Some(GotoDefinitionResponse::Scalar(location));
        // A member here, an inherited one in `a`, and a union receiver's in
        // each class
        let found = definition(&mut harness, &main, 11, 4).await;
        assert_eq!(found, scalar(at(&main, 2, 10, 14)));
        let found = definition(&mut harness, &main, 17, 4).await;
        assert_eq!(found, scalar(at(&a, 4, 12, 16)));
        let Some(GotoDefinitionResponse::Array(mut found)) =
            definition(&mut harness, &main, 12, 4).await
        else {
            panic!("expected a definition in each class");
        };
        found.sort_by_key(|location| location.uri == a);
        assert_eq!(found, [at(&main, 2, 10, 14), at(&a, 11, 10, 14)]);
        // The overload a call chose, of a method, a function here, and a
        // function in `a`
        let found = definition(&mut harness, &main, 13, 4).await;
        assert_eq!(found, scalar(at(&a, 6, 7, 12)));
        let found = definition(&mut harness, &main, 14, 2).await;
        assert_eq!(found, scalar(at(&main, 6, 5, 10)));
        let found = definition(&mut harness, &main, 15, 4).await;
        assert_eq!(found, scalar(at(&a, 14, 5, 6)));

        // An overload without docs has its implementation's
        for (line, character, doc) in [
            (17, 4, "Its size"),
            (13, 4, "Scales anything"),
            (14, 2, "Twice anything"),
            (15, 4, "Doubles a number"),
            (19, 4, "Its area"),
        ] {
            let hover = hover_text(&mut harness, main.clone(), line, character).await;
            let hover = hover.unwrap();
            assert!(hover.ends_with(doc), "at {line}:{character}: {hover}");
        }
        // Where each is declared: a member by its class, qualified by its
        // module when that isn't this one
        for (line, character, note) in [
            (13, 4, Some("in class `a.B`")),
            (15, 4, Some("in module `a`")),
            (11, 4, Some("in class `Local`")),
            (14, 2, None),
        ] {
            let hover = hover_text(&mut harness, main.clone(), line, character).await;
            let hover = hover.unwrap();
            let found = hover.lines().find_map(|line| line.strip_prefix("*in "));
            let found = found.map(|note| format!("in {}", note.trim_end_matches('*')));
            assert_eq!(found.as_deref(), note, "at {line}:{character}: {hover}");
        }
        // A getter is used as a field, which its declaration says
        let found = definition(&mut harness, &main, 19, 4).await;
        assert_eq!(found, scalar(at(&a, 24, 10, 14)));
        let hover = hover_text(&mut harness, main.clone(), 19, 4).await.unwrap();
        assert!(hover.contains("#[getter] pub def area"), "{hover}");
        harness.quiet().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disk_changes_recheck_importers() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        let a = tree.write("lib/a.dol", "pub def f x@Int\n  x\n");
        let main = Uri::from_file_path(tree.0.join("main.dol")).unwrap();
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(main.clone(), "import a\na.f \"s\"\n", 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics.len(), 1, "{:#?}", checked.diagnostics);

        tree.write("lib/a.dol", "pub def f x@Str\n  x\n");
        harness.watched(&[(&a, FileChangeType::CHANGED)]).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);

        fs::remove_file(&a).unwrap();
        harness.watched(&[(&a, FileChangeType::DELETED)]).await;
        let checked = harness.next_publish_for(&main).await;
        let messages: Vec<_> = (checked.diagnostics.iter())
            .map(|diagnostic| &diagnostic.message[..])
            .collect();
        assert_eq!(messages, ["module `a` not found"]);

        tree.write("lib/a.dol", "pub def f x@Str\n  x\n");
        harness.watched(&[(&a, FileChangeType::CREATED)]).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);

        // An open buffer of the module stands in for its file
        let buffer = Uri::from_file_path(&a).unwrap();
        harness
            .open(buffer.clone(), "# buffer\npub def f x@Str\n  x\n", 1)
            .await;
        // Its check, and its importer's against the buffer
        let mut checked = [harness.next_publish().await, harness.next_publish().await];
        checked.sort_by_key(|published| published.uri == main);
        assert_eq!(
            checked.map(|published| (published.uri, published.diagnostics)),
            [(buffer, vec![]), (main, vec![])]
        );
        tree.write("lib/a.dol", "pub def f x@Int\n  x\n");
        harness.watched(&[(&a, FileChangeType::CHANGED)]).await;
        harness.quiet().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn config_changes_resolve_documents_again() {
        // The modules import `std`, which only `dodo lsp-test` bundles
        if typelib_index::lookup("std").is_none() {
            return;
        }
        let tree = Directory::new();
        let config = tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"one\"]\n");
        tree.write("one/a.dol", "pub def f x@Str\n  x\n");
        tree.write("two/a.dol", "pub def f x@Int\n  x\n");
        let main = Uri::from_file_path(tree.0.join("main.dol")).unwrap();
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        harness.open(main.clone(), "import a\na.f \"s\"\n", 1).await;
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics, []);

        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"two\"]\n");
        harness.watched(&[(&config, FileChangeType::CHANGED)]).await;
        // Compiled again, then checked against the new module path
        let projected = harness.next_publish_for(&main).await;
        assert_eq!(projected.diagnostics, []);
        let checked = harness.next_publish_for(&main).await;
        assert_eq!(checked.diagnostics.len(), 1, "{:#?}", checked.diagnostics);
        harness.quiet().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn file_watchers_register_when_the_client_allows() {
        let mut harness = Harness::new();
        harness
            .send_request::<request::Initialize>(InitializeParams {
                capabilities: ClientCapabilities {
                    workspace: Some(WorkspaceClientCapabilities {
                        did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                            dynamic_registration: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await;
        let initialized = harness.service.ready().await.unwrap().call(
            Request::build(notification::Initialized::METHOD)
                .params(json!({}))
                .finish(),
        );
        let client = async {
            let request = tokio::time::timeout(Duration::from_secs(1), harness.socket.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request.method(), request::RegisterCapability::METHOD);
            let params: RegistrationParams =
                serde_json::from_value(request.params().cloned().unwrap()).unwrap();
            let [registration] = &params.registrations[..] else {
                panic!("expected one registration: {params:#?}");
            };
            assert_eq!(
                registration.method,
                notification::DidChangeWatchedFiles::METHOD
            );
            let options: DidChangeWatchedFilesRegistrationOptions =
                serde_json::from_value(registration.register_options.clone().unwrap()).unwrap();
            let patterns: Vec<_> = (options.watchers.iter())
                .map(|watcher| watcher.glob_pattern.clone())
                .collect();
            assert_eq!(
                patterns,
                ["**/*.dol", "**/.dolang-lsp.toml"]
                    .map(|pattern| GlobPattern::String(pattern.into()))
            );
            let id = request.id().cloned().unwrap();
            harness
                .socket
                .send(Response::from_ok(id, Value::Null))
                .await
                .unwrap();
        };
        let (response, ()) = tokio::join!(initialized, client);
        assert!(response.unwrap().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn document_on_the_module_path_is_its_module() {
        let tree = Directory::new();
        tree.write(CONFIG_FILE_NAME, "[prelude]\n[modules]\npath = [\"lib\"]\n");
        tree.write("lib/a.dol", "");
        let mut harness = Harness::new();
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        // A module can't return at top level; a script can
        let text = "return 1\n";
        let module = harness
            .open(
                Uri::from_file_path(tree.0.join("lib/a.dol")).unwrap(),
                text,
                1,
            )
            .await;
        assert_eq!(module.diagnostics.len(), 1, "{:#?}", module.diagnostics);
        let script = harness
            .open(
                Uri::from_file_path(tree.0.join("main.dol")).unwrap(),
                text,
                1,
            )
            .await;
        assert_eq!(script.diagnostics, []);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bundled_stub_checks_clean_with_the_repository_config() {
        let std = Path::new(env!("CARGO_MANIFEST_DIR")).join("../dolang/stub/std.dol");
        let std = std.canonicalize().unwrap();
        let text = fs::read_to_string(&std).unwrap();
        let mut harness = Harness::checking(Duration::from_millis(10));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        assert_eq!(checked(&mut harness, &std, &text).await, []);
    }
}
