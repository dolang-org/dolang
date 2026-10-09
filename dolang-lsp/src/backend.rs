use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs, iter, mem,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use tokio::sync::{
    Mutex as AsyncMutex,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    oneshot,
};
use toml::{Table, Value as TomlValue};
use tower_lsp_server::{
    Client, ClientSocket, LanguageServer, LspService, jsonrpc::Result, ls_types::*,
};

use dolang_compile::{
    Config as CompileConfig, Context, ErrorKind, Kind, NodeId, Token, Unit, UnitId, diag, typeck,
};

use crate::{doc_index, typelib_index};

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DEFAULT_LIBRARY,
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEFINITION,
    SemanticTokenModifier::STATIC,
];
const CONFIG_FILE_NAME: &str = ".dolang-lsp.toml";
/// How long a document's edits must settle before it is type checked
const CHECK_DELAY: Duration = Duration::from_millis(300);

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

fn span_text<'a>(content: &'a str, span: &diag::Span) -> &'a str {
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

fn parameter_label(content: &str, unit: &Unit<'_>, id: NodeId) -> Option<String> {
    let node = unit.node(id)?;
    if !matches!(
        node.kind(),
        Kind::PositionalParam { .. }
            | Kind::KeyParam { .. }
            | Kind::RestParam { .. }
            | Kind::SelfParam { .. }
    ) {
        return None;
    }
    node.span().map(|span| one_line_source(content, &span))
}

/// The source text of the type describing a node, if it has one.
fn type_label(
    content: &str,
    unit: &Unit<'_>,
    id: NodeId,
    children: &HashMap<NodeId, Vec<NodeId>>,
) -> Option<String> {
    children
        .get(&id)?
        .iter()
        .filter_map(|child| unit.node(*child))
        .find(|child| matches!(child.kind(), Kind::Type { .. }))
        .and_then(|child| child.span())
        .map(|span| one_line_source(content, &span))
}

/// A declaration's signature, including any type it is annotated with.
fn declaration_label(
    content: &str,
    unit: &Unit<'_>,
    id: NodeId,
    children: &HashMap<NodeId, Vec<NodeId>>,
) -> Option<String> {
    let node = unit.node(id)?;
    let kind = node.kind();
    let mut name = node
        .definition()
        .map(|span| span_text(content, &span).to_owned())?;
    let binders = children
        .get(&id)
        .into_iter()
        .flatten()
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
        type_label(content, unit, id, children)
    };
    let returns = || {
        ty.as_deref()
            .map(|ty| format!(" -> {ty}"))
            .unwrap_or_default()
    };
    let annot = || {
        ty.as_deref()
            .map(|ty| format!(" @{ty}"))
            .unwrap_or_default()
    };
    let params = || {
        children
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|child| parameter_label(content, unit, *child))
            .collect::<Vec<_>>()
    };
    let label = match kind {
        Kind::Class {
            is_pub, type_only, ..
        } => {
            let supers = children
                .get(&id)
                .into_iter()
                .flatten()
                .filter_map(|child| unit.node(*child))
                .filter(|child| matches!(child.kind(), Kind::Type { .. }))
                .filter_map(|child| child.span())
                .map(|span| span_text(content, &span))
                .collect::<Vec<_>>();
            format!(
                "{}{}class {name}{}",
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
            format!(
                "{}{}def {name}{}{}",
                if is_pub && !at { "pub " } else { "" },
                if at { "@" } else { "" },
                if params.is_empty() {
                    String::new()
                } else {
                    format!(" {}", params.join(" "))
                },
                returns()
            )
        }
        Kind::SpecialMethod { .. } => {
            let params = params();
            format!(
                "def {name}{}{}",
                if params.is_empty() {
                    String::new()
                } else {
                    format!(" {}", params.join(" "))
                },
                returns()
            )
        }
        Kind::Field { is_pub, .. } => {
            format!(
                "{}field {name}{}",
                if is_pub { "pub " } else { "" },
                annot()
            )
        }
        Kind::Bind { is_pub, .. } => {
            format!("{}let {name}{}", if is_pub { "pub " } else { "" }, annot())
        }
        // A parameter is labeled by its source, annotation included
        Kind::PositionalParam { .. }
        | Kind::KeyParam { .. }
        | Kind::RestParam { .. }
        | Kind::SelfParam { .. } => parameter_label(content, unit, id)?,
        _ => return None,
    };
    Some(label)
}

/// Hover text for a name that resolves outside this document -- an import or
/// a prelude binding -- built from the static doc index rather than a local
/// doc comment, since there is no declaration in this file to read one from.
fn external_hover(kind: Kind<'_>, content: &str) -> Option<String> {
    let (module, item) = match kind {
        Kind::ImportModule { module, .. } => (span_text(content, &module), ""),
        Kind::ImportItem { module, item, .. } => {
            (span_text(content, &module), span_text(content, &item))
        }
        Kind::PreludeModule { module, .. } => (module, ""),
        Kind::PreludeItem { module, item, .. } => (module, item),
        _ => return None,
    };
    let entries = doc_index::lookup(module, item);
    if entries.is_empty() {
        return None;
    }
    // Overloads share a name, and each is described in turn
    Some(
        entries
            .iter()
            .map(render_external_hover)
            .collect::<Vec<_>>()
            .join("\n\n---\n\n"),
    )
}

/// Assembles a hover markdown blob from a fenced signature and a doc comment,
/// for both a local declaration and an external one from the static doc index.
fn render_hover_markdown(label: &str, prose: &str) -> String {
    let fence = if label.contains("```") { "````" } else { "```" };
    let mut markdown = format!("{fence}dolang\n{label}\n{fence}");
    if !prose.is_empty() {
        markdown.push_str("\n\n");
        markdown.push_str(prose);
    }
    markdown
}

fn render_external_hover(entry: &doc_index::DocEntry) -> String {
    render_hover_markdown(&doc_index::signature(entry), entry.doc)
}

/// Pre-render local hover text while the compiler unit and its spans live.
fn build_hovers(unit: &Unit<'_>, content: &str) -> HashMap<NodeId, String> {
    let mut children: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (id, node) in unit.nodes() {
        if let Some(parent) = node.parent() {
            children.entry(parent).or_default().push(id);
        }
    }
    for ids in children.values_mut() {
        // A prelude binding, having no span, sorts first
        ids.sort_by_key(|id| {
            unit.node(*id)
                .expect("child node remains in its unit")
                .span()
                .map(|span| span.start().byte_offset())
        });
    }

    unit.nodes()
        .filter_map(|(id, node)| {
            if let Some(markdown) = external_hover(node.kind(), content) {
                return Some((id, markdown));
            }
            let label = declaration_label(content, unit, id, &children)?;
            let doc = node.doc().map(|span| normalize_doc(content, &span));
            Some((
                id,
                render_hover_markdown(&label, doc.as_deref().unwrap_or("")),
            ))
        })
        .collect()
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
    hover: Option<String>,
}

/// Orders the projections of one document.
///
/// Handlers run concurrently, so edits can reach the worker out of order; the
/// client's version says which is newer.  Each open starts a new epoch, so a
/// reopened document outranks whatever its previous session left in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    epoch: u64,
    version: Option<i32>,
}

impl Stamp {
    /// A document saved without being opened.
    const UNOPENED: Self = Self {
        epoch: 0,
        version: None,
    };
}

/// What the worker made of one version of a document.
///
/// A newer projection replaces the whole document, so handlers hold an `Arc`
/// of one rather than a lock.
struct Document {
    stamp: Stamp,
    /// The unit the projection came from, which holds the document's source
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    unit: Arc<Unit<'static>>,
    tokens: Vec<SemanticToken>,
    /// Token range to the declaration it names, sorted by range start
    refs: Vec<(Range, NodeId)>,
    decls: HashMap<NodeId, Decl>,
    symbols: Vec<DocumentSymbol>,
    patches: Vec<Patch>,
}

impl Document {
    fn reference_at(&self, pos: &Position) -> Option<(Range, &Decl)> {
        let end = self.refs.partition_point(|(range, _)| &range.start <= pos);
        let (range, id) = self.refs.get(end.checked_sub(1)?)?;
        if &range.end <= pos {
            return None;
        }
        Some((*range, self.decls.get(id)?))
    }

    /// The declaration named by the token under `pos`, if any.
    ///
    /// `refs` is sorted by where each token starts, so the only candidate is
    /// the last one starting at or before the position.
    fn decl_at(&self, pos: &Position) -> Option<&Decl> {
        self.reference_at(pos).map(|(_, decl)| decl)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Import {
    Module(String),
    ModuleAs(String, String),
    Item(String, String),
    ItemAs(String, String, String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) prelude: Vec<Import>,
}

#[derive(Debug, Default)]
struct Config {
    root: Option<PathBuf>,
    workspaces: Vec<PathBuf>,
    settings: HashMap<PathBuf, Arc<Settings>>,
}

type Documents = Arc<Mutex<HashMap<Uri, Arc<Document>>>>;

/// The epoch of each open document, and the last one handed out.
#[derive(Default)]
struct Epochs {
    last: u64,
    open: HashMap<Uri, u64>,
}

pub(crate) struct Backend {
    documents: Documents,
    epochs: Mutex<Epochs>,
    worker: mpsc::Sender<Request>,
    client: Client,
    config: AsyncMutex<Config>,
    position_encoding: RwLock<PositionEncodingKind>,
}

/// A document's text to compile, from an open, an edit or a save.
struct Job {
    uri: Uri,
    path: PathBuf,
    text: String,
    /// A save has none, and takes the document's latest
    stamp: Option<Stamp>,
    settings: Option<Arc<Settings>>,
    encoding: PositionEncodingKind,
    /// Told once the projection is stored and its diagnostics published
    done: Option<oneshot::Sender<()>>,
}

enum Request {
    Compile(Job),
    /// The document closed in the given epoch
    Close(Uri, u64),
}

enum Event {
    Projected {
        uri: Uri,
        document: Document,
        diagnostics: Vec<Diagnostic>,
        done: Option<oneshot::Sender<()>>,
    },
    /// A check of the document's projection with the given stamp, with its
    /// compile diagnostics
    Checked {
        uri: Uri,
        stamp: Stamp,
        diagnostics: Vec<Diagnostic>,
    },
    Closed(Uri, u64),
}

/// The unit the worker last compiled for a document.
struct Open {
    stamp: Stamp,
    settings: Option<Arc<Settings>>,
    unit: Arc<Unit<'static>>,
    encoding: PositionEncodingKind,
    /// The unit's compile diagnostics
    diagnostics: Vec<Diagnostic>,
    /// The checker's diagnostics for the unit, once it has been checked
    checked: Option<Vec<Diagnostic>>,
}

/// Compiles documents off the event loop, on a thread of its own, and type
/// checks them once their edits settle.
///
/// Projections and checks go to the applier task, which stores projections and
/// publishes diagnostics in the order the worker made them.
struct Worker {
    events: UnboundedSender<Event>,
    open: HashMap<Uri, Open>,
    /// The epoch each document last closed in; older work for it is dropped
    closed: HashMap<Uri, u64>,
    /// How long edits must settle before a check, or `None` not to check
    check_delay: Option<Duration>,
    /// When each document with an unchecked unit is due to be checked
    pending: HashMap<Uri, Instant>,
    typelibs: Typelibs,
}

impl Worker {
    fn run(mut self, requests: mpsc::Receiver<Request>) {
        loop {
            // Few documents are open, so a scan finds the next check soon enough
            let due = self
                .pending
                .iter()
                .min_by_key(|(_, deadline)| **deadline)
                .map(|(uri, deadline)| (uri.clone(), *deadline));
            let first = match due {
                None => match requests.recv() {
                    Ok(first) => first,
                    Err(mpsc::RecvError) => break,
                },
                Some((uri, deadline)) => {
                    match requests.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(first) => first,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            self.pending.remove(&uri);
                            self.check(uri);
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            };
            let batch: Vec<_> = iter::once(first).chain(requests.try_iter()).collect();
            // Only the newest edit of each document is worth compiling
            let superseded: Vec<bool> = (0..batch.len())
                .map(|i| match &batch[i] {
                    Request::Compile(Job {
                        uri,
                        stamp: Some(stamp),
                        ..
                    }) => batch[i + 1..].iter().any(|later| match later {
                        Request::Compile(later) => {
                            later.uri == *uri && later.stamp.is_some_and(|later| later >= *stamp)
                        }
                        Request::Close(later, epoch) => later == uri && stamp.epoch <= *epoch,
                    }),
                    _ => false,
                })
                .collect();
            for (request, superseded) in batch.into_iter().zip(superseded) {
                match request {
                    Request::Compile(_) if superseded => {}
                    Request::Compile(job) => self.compile(job),
                    Request::Close(uri, epoch) => self.close(uri, epoch),
                }
            }
        }
    }

    fn compile(&mut self, job: Job) {
        let Job {
            uri,
            path,
            text,
            stamp,
            settings,
            encoding,
            done,
        } = job;
        let open = self.open.get(&uri);
        let stamp = stamp.unwrap_or_else(|| open.map_or(Stamp::UNOPENED, |open| open.stamp));
        if self
            .closed
            .get(&uri)
            .is_some_and(|&epoch| stamp.epoch <= epoch)
            || open.is_some_and(|open| stamp < open.stamp)
        {
            return;
        }
        let cached = open
            .filter(|open| {
                open.settings == settings && open.encoding == encoding && open.unit.source() == text
            })
            .map(|open| (open.unit.clone(), open.checked.clone()));
        let (cached, checked) = cached.unzip();
        let checked = checked.flatten();
        let projected = panic::catch_unwind(AssertUnwindSafe(|| {
            let unit = cached
                .unwrap_or_else(|| Arc::new(compile_document(&path, text, settings.as_deref())));
            let (document, diagnostics) = project(&uri, stamp, unit.clone(), &encoding);
            (unit, document, diagnostics)
        }));
        // The panic hook has logged the panic itself
        let Ok((unit, document, diagnostics)) = projected else {
            log::error!("failed to compile {}", path.display());
            return;
        };
        self.closed.remove(&uri);
        // An unchanged unit keeps its check; a changed one waits for its edits
        // to settle, each edit pushing the check back
        let published = match (&checked, self.check_delay) {
            (Some(checked), _) => [&diagnostics[..], checked].concat(),
            (None, Some(delay)) => {
                self.pending.insert(uri.clone(), Instant::now() + delay);
                diagnostics.clone()
            }
            (None, None) => diagnostics.clone(),
        };
        self.open.insert(
            uri.clone(),
            Open {
                stamp,
                settings,
                unit,
                encoding,
                diagnostics,
                checked,
            },
        );
        _ = self.events.send(Event::Projected {
            uri,
            document,
            diagnostics: published,
            done,
        });
    }

    /// Checks a document whose check is due, if it is still open and unchecked.
    fn check(&mut self, uri: Uri) {
        let Some(open) = self.open.get_mut(&uri) else {
            return;
        };
        if open.checked.is_some() {
            return;
        }
        let start = Instant::now();
        let checked = panic::catch_unwind(AssertUnwindSafe(|| {
            check_document(&uri, &open.unit, &open.encoding, &mut self.typelibs)
        }));
        log::debug!("checked {} in {:?}", uri.as_str(), start.elapsed());
        // The compile diagnostics already published are all there is to say
        // about a unit the checker can't take
        let checked = checked.unwrap_or_else(|_| {
            log::error!("failed to check {}", uri.as_str());
            None
        });
        open.checked = Some(checked.clone().unwrap_or_default());
        if let Some(checked) = checked {
            _ = self.events.send(Event::Checked {
                uri,
                stamp: open.stamp,
                diagnostics: [&open.diagnostics[..], &checked].concat(),
            });
        }
    }

    fn close(&mut self, uri: Uri, epoch: u64) {
        if self
            .open
            .get(&uri)
            .is_some_and(|open| open.stamp.epoch <= epoch)
        {
            self.open.remove(&uri);
            self.pending.remove(&uri);
        }
        let closed = self.closed.entry(uri.clone()).or_default();
        *closed = (*closed).max(epoch);
        _ = self.events.send(Event::Closed(uri, epoch));
    }
}

/// Stores a projection unless the document already has a newer one.
fn apply(documents: &Documents, uri: Uri, document: Document) -> bool {
    let mut documents = documents.lock().expect("documents lock poisoned");
    if documents
        .get(&uri)
        .is_some_and(|stored| stored.stamp > document.stamp)
    {
        return false;
    }
    documents.insert(uri, Arc::new(document));
    true
}

/// Whether the stored projection of a document is the one with `stamp`.
fn current(documents: &Documents, uri: &Uri, stamp: Stamp) -> bool {
    let documents = documents.lock().expect("documents lock poisoned");
    documents
        .get(uri)
        .is_some_and(|document| document.stamp == stamp)
}

/// Applies the worker's events in order, publishing each stored projection's
/// diagnostics.
async fn publish(client: Client, documents: Documents, mut events: UnboundedReceiver<Event>) {
    while let Some(event) = events.recv().await {
        match event {
            Event::Projected {
                uri,
                document,
                diagnostics,
                done,
            } => {
                let version = document.stamp.version;
                if apply(&documents, uri.clone(), document) {
                    client.publish_diagnostics(uri, diagnostics, version).await;
                }
                if let Some(done) = done {
                    _ = done.send(());
                }
            }
            Event::Checked {
                uri,
                stamp,
                diagnostics,
            } => {
                // Unless a newer projection has replaced what was checked
                if current(&documents, &uri, stamp) {
                    client
                        .publish_diagnostics(uri, diagnostics, stamp.version)
                        .await;
                }
            }
            Event::Closed(uri, epoch) => {
                let mut documents = documents.lock().expect("documents lock poisoned");
                if documents
                    .get(&uri)
                    .is_some_and(|document| document.stamp.epoch <= epoch)
                {
                    documents.remove(&uri);
                }
            }
        }
    }
}

/// Compiles a document as the server projects it.
fn compile_document(path: &Path, text: String, settings: Option<&Settings>) -> Unit<'static> {
    let mut config = CompileConfig::new();
    config.recover(true).document(true).typecheck(true);
    if let Some(settings) = settings {
        let mut prelude = config.prelude();
        for import in settings.prelude.iter() {
            match import {
                Import::Module(module) => {
                    prelude = prelude.import_module(module.clone());
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
    }
    config.unit(path, text)
}

/// The bundled typelibs, decoded as checks first need them.
#[derive(Default)]
struct Typelibs(HashMap<String, Option<typeck::Typelib<'static>>>);

impl Typelibs {
    /// The typelibs of the bundled modules a unit imports, directly or through
    /// other typelibs, each once.
    fn imported_by(&mut self, unit: &Unit<'_>) -> Vec<&typeck::Typelib<'static>> {
        let mut queue: Vec<String> = unit.imports().into_iter().map(str::to_owned).collect();
        let mut seen = HashSet::new();
        while let Some(name) = queue.pop() {
            if seen.contains(&name) {
                continue;
            }
            let typelib = self.0.entry(name.clone()).or_insert_with(|| {
                let bytes = typelib_index::lookup(&name)?;
                typeck::Typelib::read(bytes)
                    .inspect_err(|err| log::error!("bundled typelib {name} is unreadable: {err}"))
                    .ok()
            });
            if let Some(typelib) = typelib {
                queue.extend(typelib.imports().into_iter().map(str::to_owned));
            }
            seen.insert(name);
        }
        let mut found: Vec<_> = seen
            .iter()
            .filter_map(|name| self.0[name].as_ref())
            .collect();
        found.sort_by_key(|typelib| typelib.module());
        found
    }
}

/// Type checks a document's unit against the bundled typelibs it imports.
///
/// Returns the checker's diagnostics located in the document, or `None` when the
/// unit failed to compile and can't be checked.  An import that isn't bundled is
/// unknown to the checker, which says nothing about it.
fn check_document(
    uri: &Uri,
    unit: &Unit<'static>,
    encoding: &PositionEncodingKind,
    typelibs: &mut Typelibs,
) -> Option<Vec<Diagnostic>> {
    let mut builder = typeck::Builder::new();
    // The shell's pipelines connect stages with `proc`'s pipes
    builder.pipes(("proc", "PipeSender"), ("proc", "PipeReceiver"));
    let id = match builder.unit(unit) {
        Ok(id) => id,
        Err(err) if matches!(err.kind(), ErrorKind::Fail) => return None,
        Err(err) => {
            log::error!("cannot check {}: {err}", uri.as_str());
            return None;
        }
    };
    for typelib in typelibs.imported_by(unit) {
        if let Err(err) = builder.typelib(typelib) {
            log::error!("cannot check against {}: {err}", typelib.module());
        }
    }
    let check = builder.check();
    let index = DocumentIndex::new(unit.source(), encoding.clone());
    let mut diagnostics = Vec::new();
    for diag in check.diagnostics() {
        if diag.span().unit() == Some(id) {
            let relate = |range, span: &diag::SourceSpan, message: String| {
                relate_checked(uri, &index, &check, id, range, span, message)
            };
            convert_diag(&index, diag, relate, &mut diagnostics);
        }
    }
    Some(diagnostics)
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
        let documents = Documents::default();
        let (worker, requests) = mpsc::channel();
        let (events, applied) = unbounded_channel();
        let worker_state = Worker {
            events,
            open: HashMap::new(),
            closed: HashMap::new(),
            check_delay,
            pending: HashMap::new(),
            typelibs: Typelibs::default(),
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
            epochs: Default::default(),
            worker,
            client,
            config: Default::default(),
            position_encoding: RwLock::new(PositionEncodingKind::UTF16),
        }
    }

    /// The latest projection of a document.
    fn document(&self, uri: &Uri) -> Option<Arc<Document>> {
        let documents = self.documents.lock().expect("documents lock poisoned");
        documents.get(uri).cloned()
    }

    /// The epoch a document was opened in, or 0 if it is not open.
    fn epoch(&self, uri: &Uri) -> u64 {
        let epochs = self.epochs.lock().expect("epochs lock poisoned");
        epochs.open.get(uri).copied().unwrap_or(0)
    }

    /// Sends a document's text to the worker.
    async fn submit(
        &self,
        uri: Uri,
        text: String,
        stamp: Option<Stamp>,
        done: Option<oneshot::Sender<()>>,
    ) {
        let Some(path) = uri_to_file_path(&uri).map(Cow::into_owned) else {
            return;
        };
        if !is_dolang_source(&path) {
            return;
        }
        let settings = self.find_settings(&path).await;
        let job = Job {
            uri,
            path,
            text,
            stamp,
            settings,
            encoding: self.position_encoding(),
            done,
        };
        if self.worker.send(Request::Compile(job)).is_err() {
            log::error!("the compile worker has exited");
        }
    }

    async fn find_settings(&self, path: &Path) -> Option<Arc<Settings>> {
        let guard = self.config.lock().await;
        let mut cur = path.parent();
        let mut config_file = None;

        while let Some(dir) = cur {
            cur = dir.parent();
            if let Some(settings) = guard.settings.get(dir) {
                return Some(settings.clone());
            }
            let candidate = dir.join(CONFIG_FILE_NAME);
            if candidate.is_file() {
                config_file = Some((dir.to_owned(), candidate));
                break;
            }
        }
        mem::drop(guard);

        if let Some((config_dir, config)) = config_file {
            let settings = match Self::read_settings(&config) {
                Ok(settings) => settings,
                Err(e) => {
                    log::error!("failed to load configuration {}: {}", config.display(), e);
                    return None;
                }
            };
            let settings = Arc::new(settings);
            let mut guard = self.config.lock().await;
            guard.settings.insert(config_dir, settings.clone());
            return Some(settings);
        }
        Some(Arc::new(Self::default_settings(path)))
    }

    fn read_settings(path: &Path) -> std::result::Result<Settings, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        let value = toml::from_str::<TomlValue>(&content)
            .map_err(|e| format!("invalid TOML in {}: {e}", path.display()))?;
        Self::parse_settings_toml(&value)
    }

    fn parse_settings_toml(value: &TomlValue) -> std::result::Result<Settings, String> {
        let table = value
            .as_table()
            .ok_or_else(|| "settings are not a table as expected".to_owned())?;
        let mut prelude = Vec::new();

        for (key, value) in table {
            match key.as_str() {
                "prelude" => Self::parse_prelude_table(value, &mut prelude)?,
                _ => return Err(format!("unexpected key in settings: {key}")),
            }
        }

        Ok(Settings { prelude })
    }

    fn parse_prelude_table(
        value: &TomlValue,
        prelude: &mut Vec<Import>,
    ) -> std::result::Result<(), String> {
        let table = value
            .as_table()
            .ok_or_else(|| "prelude is not a table as expected".to_owned())?;

        for (module, value) in table {
            Self::parse_prelude_entry(module, value, prelude)?;
        }

        Ok(())
    }

    fn parse_prelude_entry(
        module: &str,
        value: &TomlValue,
        prelude: &mut Vec<Import>,
    ) -> std::result::Result<(), String> {
        match value {
            TomlValue::Boolean(true) => prelude.push(Import::Module(module.to_owned())),
            TomlValue::Boolean(false) => {
                return Err(format!("prelude entry for {module} cannot be false"));
            }
            TomlValue::String(bind) => {
                prelude.push(Import::ModuleAs(module.to_owned(), bind.clone()));
            }
            TomlValue::Array(items) => {
                for item in items {
                    let item = item.as_str().ok_or_else(|| {
                        format!("prelude array for {module} must contain only strings")
                    })?;
                    prelude.push(Import::Item(module.to_owned(), item.to_owned()));
                }
            }
            TomlValue::Table(table) => Self::parse_prelude_descriptor(module, table, prelude)?,
            _ => {
                return Err(format!(
                    "prelude entry for {module} must be true, a string, an array, or a table"
                ));
            }
        }

        Ok(())
    }

    fn parse_prelude_descriptor(
        module: &str,
        table: &Table,
        prelude: &mut Vec<Import>,
    ) -> std::result::Result<(), String> {
        for (key, value) in table {
            if key == "mod" {
                match value {
                    TomlValue::Boolean(true) => prelude.push(Import::Module(module.to_owned())),
                    TomlValue::Boolean(false) => {
                        return Err(format!("descriptor entry {module}.mod cannot be false"));
                    }
                    TomlValue::String(bind) => {
                        prelude.push(Import::ModuleAs(module.to_owned(), bind.clone()));
                    }
                    _ => {
                        return Err(format!(
                            "descriptor entry {module}.mod must be true or a string"
                        ));
                    }
                }
                continue;
            }

            match value {
                TomlValue::Boolean(true) => {
                    prelude.push(Import::Item(module.to_owned(), key.clone()));
                }
                TomlValue::Boolean(false) => {
                    return Err(format!("descriptor entry {module}.{key} cannot be false"));
                }
                TomlValue::String(bind) => {
                    prelude.push(Import::ItemAs(module.to_owned(), key.clone(), bind.clone()));
                }
                _ => {
                    return Err(format!(
                        "descriptor entry {module}.{key} must be true or a string"
                    ));
                }
            }
        }

        Ok(())
    }

    fn default_settings(_path: &Path) -> Settings {
        let prelude = vec![
            Import::Module("shell".into()),
            Import::Item("proc".into(), "run".into()),
            Import::Item("proc".into(), "sub".into()),
            Import::Item("shell".into(), "cd".into()),
            Import::Item("shell".into(), "env".into()),
            Import::Item("shell".into(), "exit".into()),
            Import::Item("term".into(), "echo".into()),
            Import::Item("term".into(), "print".into()),
        ];
        Settings { prelude }
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
    stamp: Stamp,
    unit: Arc<Unit<'static>>,
    encoding: &PositionEncodingKind,
) -> (Document, Vec<Diagnostic>) {
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
                                    hover: hovers.get(&id).cloned(),
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
                                    hover: hovers.get(&id).cloned(),
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

    let document = Document {
        stamp,
        unit,
        tokens,
        refs,
        decls,
        symbols,
        patches,
    };
    (document, diags)
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let TextDocumentItem {
            uri, text, version, ..
        } = params.text_document;
        let epoch = {
            let mut epochs = self.epochs.lock().expect("epochs lock poisoned");
            epochs.last += 1;
            let epoch = epochs.last;
            epochs.open.insert(uri.clone(), epoch);
            epoch
        };
        let stamp = Stamp {
            epoch,
            version: Some(version),
        };
        self.submit(uri, text, Some(stamp), None).await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let stamp = Stamp {
            epoch: self.epoch(&uri),
            version: Some(params.text_document.version),
        };
        let text = params.content_changes.into_iter().next().unwrap().text;
        self.submit(uri, text, Some(stamp), None).await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Some(path) = uri_to_file_path(&params.text_document.uri) else {
            return;
        };
        if !is_dolang_source(&path) {
            return;
        }
        if let Some(text) = params.text {
            let (done, stored) = oneshot::channel();
            self.submit(params.text_document.uri, text, None, Some(done))
                .await;
            // Refresh once the saved text's tokens are stored, or the worker
            // has passed over it
            _ = stored.await;
            _ = self.client.semantic_tokens_refresh().await;
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(document) = self.document(&params.text_document.uri) else {
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
        let Some(document) = self.document(&params.text_document.uri) else {
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
        let uri = params.text_document.uri;
        let epoch = {
            let mut epochs = self.epochs.lock().expect("epochs lock poisoned");
            epochs.open.remove(&uri).unwrap_or(0)
        };
        // Through the worker, so the document is dropped after any projection
        // still on its way
        if self.worker.send(Request::Close(uri, epoch)).is_err() {
            log::error!("the compile worker has exited");
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        log::debug!("change config: {params:?}")
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        log::debug!("change workspace: {params:?}")
    }

    async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {}

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some(document) = self.document(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        let pos = &params.text_document_position_params.position;
        let Some(decl) = document.decl_at(pos) else {
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
        let Some(document) = self.document(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        let pos = &params.text_document_position_params.position;
        let Some((range, decl)) = document.reference_at(pos) else {
            return Ok(None);
        };
        let Some(markdown) = decl.hover.as_ref() else {
            return Ok(None);
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown.clone(),
            }),
            range: Some(range),
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(document) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        let symbols = document.symbols.clone();
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let Some(document) = self.document(uri) else {
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
        let Some(document) = self.document(uri) else {
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
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};
    use tower::{Service, ServiceExt};
    use tower_lsp_server::jsonrpc::{Request, Response};
    use tower_lsp_server::ls_types::{notification, request};
    use tower_lsp_server::ls_types::{notification::Notification as _, request::Request as _};

    use super::*;

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

    fn parse_toml(input: &str) -> TomlValue {
        toml::from_str(input).unwrap()
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

    #[test]
    fn parse_module_import() {
        let settings =
            Backend::parse_settings_toml(&parse_toml("[prelude]\nshell = true\n")).unwrap();

        assert_eq!(settings.prelude, vec![Import::Module("shell".to_owned())]);
    }

    #[test]
    fn parse_item_alias() {
        let settings =
            Backend::parse_settings_toml(&parse_toml("[prelude.proc]\nrun = true\n")).unwrap();

        assert_eq!(
            settings.prelude,
            vec![Import::Item("proc".to_owned(), "run".to_owned())]
        );
    }

    #[test]
    fn parse_item_array() {
        let settings = Backend::parse_settings_toml(&parse_toml(
            "[prelude]\nregression = [\"assert\", \"log\"]\n",
        ))
        .unwrap();

        assert_eq!(
            settings.prelude,
            vec![
                Import::Item("regression".to_owned(), "assert".to_owned()),
                Import::Item("regression".to_owned(), "log".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_descriptor_table_with_mod_and_items() {
        let settings =
            Backend::parse_settings_toml(&parse_toml("[prelude.proc]\nmod = true\nsub = true\n"))
                .unwrap();

        assert_eq!(
            settings.prelude,
            vec![
                Import::Module("proc".to_owned()),
                Import::Item("proc".to_owned(), "sub".to_owned()),
            ]
        );
    }

    #[test]
    fn reject_unknown_top_level_key() {
        let error = Backend::parse_settings_toml(&parse_toml("other = true\n")).unwrap_err();

        assert!(error.contains("unexpected key in settings"));
    }

    #[test]
    fn reject_false_module_value() {
        let error =
            Backend::parse_settings_toml(&parse_toml("[prelude]\nshell = false\n")).unwrap_err();

        assert!(error.contains("cannot be false"));
    }

    #[test]
    fn reject_non_string_array_item() {
        let error = Backend::parse_settings_toml(&parse_toml("[prelude]\nshell = [\"echo\", 1]\n"))
            .unwrap_err();

        assert!(error.contains("must contain only strings"));
    }

    #[test]
    fn reject_invalid_descriptor_value() {
        let error =
            Backend::parse_settings_toml(&parse_toml("[prelude.shell]\necho = 1\n")).unwrap_err();

        assert!(error.contains("must be true or a string"));
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

        let field_hover = hover_at(&mut harness, uri.clone(), 4, 13).await.unwrap();
        assert_eq!(
            field_hover.contents,
            markdown("```dolang\npub field count @Int\n```\n\nThe count.")
        );

        let method_hover = hover_at(&mut harness, uri.clone(), 7, 11).await.unwrap();
        assert_eq!(
            method_hover.contents,
            markdown("```dolang\npub def get[U] self -> Int\n```\n\nGets the count.")
        );

        let function_hover = hover_at(&mut harness, uri.clone(), 22, 8).await.unwrap();
        assert_eq!(
            function_hover.contents,
            markdown(concat!(
                "```dolang\n",
                "pub def build[T, :K, ...R] value @ Int :mode = fast ...rest -> Widget\n",
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
            markdown("```dolang\nlet local @Int\n```")
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
                    "Writes to standard output."
                )
                .to_owned(),
            })
        );
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
        let path = Path::new("/stale-test.dol");
        let newer = "let newer = 1\n";
        let older = "# older\nlet older = 1\n";
        harness.open(uri.clone(), newer, 2).await;

        let backend = harness.service.inner();
        let stored = backend.document(&uri).unwrap();
        let stale = Stamp {
            version: Some(1),
            ..stored.stamp
        };
        let unit = Arc::new(compile_document(path, older.to_owned(), None));
        let (document, _) = project(&uri, stale, unit, &PositionEncodingKind::UTF16);
        assert!(!apply(&backend.documents, uri.clone(), document));
        let document = backend.document(&uri).unwrap();
        assert_eq!(document.unit.source(), newer);
        assert_eq!(document.tokens, stored.tokens);

        // Reopening starts a new epoch, which outranks every earlier version
        let reopened = harness.open(uri.clone(), older, 1).await;
        assert_eq!(reopened.version, Some(1));
        let document = harness.service.inner().document(&uri).unwrap();
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
        let document = harness.service.inner().document(&uri).unwrap();
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
        // The worker handles requests in order, so once a later document is
        // published the close has been applied
        let other: Uri = "file:///open-test.dol".parse().unwrap();
        harness.open(other, "let b = 2\n", 1).await;

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
        let stored = backend.document(&uri).unwrap().stamp;
        let older = Stamp {
            version: Some(1),
            ..stored
        };
        assert!(current(&backend.documents, &uri, stored));
        assert!(!current(&backend.documents, &uri, older));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unbundled_import_reports_nothing() {
        let mut harness = Harness::checking(Duration::from_millis(50));
        harness.initialize(vec![PositionEncodingKind::UTF16]).await;
        let uri: Uri = "file:///unbundled-test.dol".parse().unwrap();
        let text = "import not_bundled\nnot_bundled.f 1\n";
        harness.open(uri, text, 1).await;

        let checked = harness.next_publish().await;
        assert_eq!(checked.diagnostics, []);
        harness.quiet().await;
    }
}
