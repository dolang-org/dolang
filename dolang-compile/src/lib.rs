#![deny(warnings)]

pub(crate) mod ast;
pub(crate) mod cfg;
pub(crate) mod constant;
pub mod diag;
pub(crate) mod doc;
pub(crate) mod elab;
pub(crate) mod emit;
pub(crate) mod flow;
pub(crate) mod lex;
pub(crate) mod lower;
pub(crate) mod parse;
pub(crate) mod sig;
pub mod source;
pub(crate) mod sym;

use std::{
    convert::Infallible,
    error,
    fmt::{self, Display},
    io::{self, Write},
    marker::PhantomData,
    mem,
    num::NonZero,
    ops::ControlFlow,
    path::Path,
    slice,
};

use crate::{ast::visit, lex::Comment};

use self::{
    ast::visit::{Node as AstNode, NodeKind},
    elab::Elaborater,
    emit::Emitter,
    lex::Lexer,
    lower::Lowerer,
    parse::Parser,
    source::{Diags, File},
};

pub use ast::{Context, visit::Token};

#[cfg(feature = "debug")]
use dolang_util::debug_eprintln;
use dolang_util::intern::{self, BinTable};

use ast::Res;

const STD_PRELUDE: &[&str] = &[
    "Array", "Bin", "BinBuf", "Bool", "Dict", "Float", "Func", "Int", "Range", "Record", "Set",
    "Str", "StrBuf", "Sym", "Tuple", "Type", "array", "bool", "class", "dbg", "dict", "float",
    "getter", "int", "record", "setter", "static", "str", "sym", "tuple", "type",
];

#[derive(Debug)]
enum ErrorInfo {
    Fail,
    Io(io::Error),
}

/// Kind of compilation error.
#[derive(Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Compilation failed; consult diagnostics.
    Fail,
    /// I/O error emitting bytecode.
    Io,
}

/// Compile error
#[derive(Debug)]
pub struct Error(ErrorInfo);

impl Error {
    /// Get kind of error
    pub fn kind(&self) -> ErrorKind {
        match &self.0 {
            ErrorInfo::Fail => ErrorKind::Fail,
            ErrorInfo::Io(_) => ErrorKind::Io,
        }
    }

    /// Get underlying [`io::Error`], if applicable
    pub fn as_io(&self) -> Option<&io::Error> {
        match &self.0 {
            ErrorInfo::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Error(ErrorInfo::Io(value))
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ErrorInfo::Fail => "compilation failed".fmt(f),
            ErrorInfo::Io(e) => e.fmt(f),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.0 {
            ErrorInfo::Fail => None,
            ErrorInfo::Io(error) => Some(error),
        }
    }
}

impl From<lower::Error> for Error {
    fn from(_: lower::Error) -> Self {
        Self(ErrorInfo::Fail)
    }
}

/// Identity of a document node.
///
/// Compare and hash these to relate tokens to the declarations they refer to;
/// the representation is deliberately opaque.  Obtain nodes from
/// [`Unit::nodes`] and [`Unit::node`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(NonZero<u32>);

/// Token emitter
pub trait EmitToken {
    /// Emit token
    ///
    /// `node` identifies the declaration the token refers to, if any.  Resolve
    /// it with [`Unit::node`].
    fn emit(&mut self, token: Token, span: diag::Span, node: Option<NodeId>, context: Context);
}

/// Callback function as token emitter
impl<F> EmitToken for F
where
    F: FnMut(Token, diag::Span, Option<NodeId>, Context),
{
    fn emit(&mut self, token: Token, span: diag::Span, node: Option<NodeId>, context: Context) {
        self(token, span, node, context)
    }
}

struct VisitAdapter<'a, 'e> {
    file: &'a File<'a>,
    emit: &'e mut dyn EmitToken,
}

struct CallAdapter<'a, 'b, 'e> {
    parent: &'b mut VisitAdapter<'a, 'e>,
    seen_arg0: bool,
}

struct CallIdentAdapter<'a, 'b, 'e>(&'b mut VisitAdapter<'a, 'e>);

impl visit::Visit for CallIdentAdapter<'_, '_, '_> {
    type Break = Infallible;

    fn node<T: AstNode + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        self.0.node(node)
    }

    fn token(
        &mut self,
        _token: Token,
        span: source::Span,
        node: Option<doc::Id>,
    ) -> ControlFlow<Self::Break> {
        self.0
            .emit_token(Token::Variable, span, node, Context::Call)
    }
}

struct MethodAdapter<'a, 'b, 'e>(&'b mut VisitAdapter<'a, 'e>);

impl visit::Visit for MethodAdapter<'_, '_, '_> {
    type Break = Infallible;

    fn node<T: AstNode + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        self.0.node(node)
    }

    fn token(
        &mut self,
        token: Token,
        span: source::Span,
        node: Option<doc::Id>,
    ) -> ControlFlow<Self::Break> {
        let context = match token {
            Token::Field => Context::Call,
            _ => Context::None,
        };
        self.0.emit_token(token, span, node, context)
    }
}

impl visit::Visit for CallAdapter<'_, '_, '_> {
    type Break = Infallible;

    fn node<T: AstNode + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        if self.seen_arg0 {
            node.accept(self.parent)
        } else {
            self.seen_arg0 = true;
            match node.kind() {
                NodeKind::Index | NodeKind::Call => node.accept(&mut CallAdapter {
                    parent: self.parent,
                    seen_arg0: false,
                }),
                NodeKind::Ident => node.accept(&mut CallIdentAdapter(self.parent)),
                NodeKind::Field => node.accept(&mut MethodAdapter(self.parent)),
                _ => node.accept(self.parent),
            }
        }
    }

    fn token(
        &mut self,
        token: Token,
        span: source::Span,
        node: Option<doc::Id>,
    ) -> ControlFlow<Self::Break> {
        self.parent.emit_token(token, span, node, Context::None)
    }
}

fn convert_span(file: &File, span: source::Span) -> diag::Span {
    let coords = file.coord_span(span);
    diag::Span::new(
        diag::Pos::new(span.start as usize, coords.start.line, coords.start.column),
        diag::Pos::new(span.end as usize, coords.end.line, coords.end.column),
    )
}

fn public_node_id(id: doc::Id) -> NodeId {
    NodeId(id.get())
}

fn internal_node_id(id: NodeId) -> doc::Id {
    doc::Id::new(id.0)
}

/// A document node: a declaration or construct recorded by document indexing.
///
/// This is a view onto the unit that produced it rather than a copy, so it is
/// cheap to pass around and its spans are resolved only when asked for.
#[derive(Copy, Clone)]
pub struct Node<'a> {
    file: &'a File<'a>,
    node: &'a doc::Node,
}

impl<'a> Node<'a> {
    /// The node this one is lexically inside, if any.
    ///
    /// Parentage is a containment relation: a method's parent is its class, a
    /// parameter's is its function, a `let`'s is whatever construct encloses it.
    pub fn parent(&self) -> Option<NodeId> {
        self.node.parent.map(public_node_id)
    }

    /// The extent of the whole construct.
    ///
    /// Order siblings by this; nothing depends on the order nodes are yielded.
    pub fn span(&self) -> diag::Span {
        convert_span(self.file, self.node.span)
    }

    /// What this node is, with whatever else varies by that.
    pub fn kind(&self) -> Kind<'a> {
        let span = |span: &source::Span| convert_span(self.file, *span);
        match &self.node.kind {
            doc::Kind::Class {
                name,
                is_pub,
                supers,
            } => Kind::Class {
                name: span(name),
                is_pub: *is_pub,
                supers: Supers {
                    file: self.file,
                    supers: supers.iter(),
                },
            },
            doc::Kind::Function { name, is_pub } => Kind::Function {
                name: span(name),
                is_pub: *is_pub,
            },
            doc::Kind::Method { name, is_pub } => Kind::Method {
                name: span(name),
                is_pub: *is_pub,
            },
            doc::Kind::SpecialMethod { name } => Kind::SpecialMethod { name: span(name) },
            doc::Kind::Field { name, is_pub } => Kind::Field {
                name: span(name),
                is_pub: *is_pub,
            },
            doc::Kind::Bind { name, is_pub } => Kind::Bind {
                name: span(name),
                is_pub: *is_pub,
            },
            doc::Kind::PositionalParam { name, default } => Kind::PositionalParam {
                name: span(name),
                default: default.as_ref().map(span),
            },
            doc::Kind::KeyParam { key, name, default } => Kind::KeyParam {
                key: span(key),
                name: span(name),
                default: default.as_ref().map(span),
            },
            doc::Kind::RestParam { name } => Kind::RestParam {
                name: name.as_ref().map(span),
            },
            doc::Kind::SelfParam { name } => Kind::SelfParam { name: span(name) },
            doc::Kind::ImportModule { module, name } => Kind::ImportModule {
                module: span(module),
                name: span(name),
            },
            doc::Kind::ImportItem { module, item, name } => Kind::ImportItem {
                module: span(module),
                item: span(item),
                name: span(name),
            },
            doc::Kind::PreludeModule { module, name } => Kind::PreludeModule { module, name },
            doc::Kind::PreludeItem { module, item, name } => {
                Kind::PreludeItem { module, item, name }
            }
            doc::Kind::Lambda => Kind::Lambda,
            doc::Kind::If => Kind::If,
            doc::Kind::Else => Kind::Else,
            doc::Kind::While => Kind::While,
            doc::Kind::For => Kind::For,
            doc::Kind::Try => Kind::Try,
            doc::Kind::Catch => Kind::Catch,
            doc::Kind::Finally => Kind::Finally,
            doc::Kind::ForElem => Kind::ForElem,
            doc::Kind::IfElem => Kind::IfElem,
            doc::Kind::Decorator { target } => Kind::Decorator {
                target: target.map(public_node_id),
            },
            doc::Kind::Break { target } => Kind::Break {
                target: target.map(public_node_id),
            },
            doc::Kind::Continue { target } => Kind::Continue {
                target: target.map(public_node_id),
            },
            doc::Kind::Return { target } => Kind::Return {
                target: target.map(public_node_id),
            },
        }
    }
}

/// What a [`Node`] is, together with everything that varies by that.
///
/// This is deliberately at the granularity of the language rather than the
/// parser: `:foo` and `foo: local` parameters are both [`Kind::KeyParam`],
/// for instance, leaving the parser free to re-split them.
#[non_exhaustive]
pub enum Kind<'a> {
    /// A class declaration
    Class {
        /// The class name
        name: diag::Span,
        /// Declared `pub`
        is_pub: bool,
        /// Superclass references, in the order written
        supers: Supers<'a>,
    },
    /// A `def` at statement level
    Function {
        /// The function name
        name: diag::Span,
        /// Declared `pub`
        is_pub: bool,
    },
    /// A `def` in a class body.  Its class is its parent.
    Method {
        /// The method name
        name: diag::Span,
        /// Declared `pub`
        is_pub: bool,
    },
    /// A method implementing a protocol.  Its class is its parent.
    ///
    /// A special method is part of the type's interface however it was
    /// declared, so there is no visibility to report.
    SpecialMethod {
        /// The protocol name, excluding the parentheses it is written in
        name: diag::Span,
    },
    /// A field declaration in a class body.  Its class is its parent.
    ///
    /// A declaration naming several fields yields one node per name.
    Field {
        /// The field name
        name: diag::Span,
        /// Declared `pub`
        is_pub: bool,
    },
    /// A `let` or other binding
    Bind {
        /// The bound name
        name: diag::Span,
        /// Declared `pub`
        is_pub: bool,
    },
    /// A positional parameter. Its function is its parent.
    PositionalParam {
        /// The bound name
        name: diag::Span,
        /// The default value expression, if any.  Slice the source for its text.
        default: Option<diag::Span>,
    },
    /// A keyword parameter. Its function is its parent.
    KeyParam {
        /// The key as written, without the `:` sigil
        key: diag::Span,
        /// The bound name
        name: diag::Span,
        /// The default value expression, if any
        default: Option<diag::Span>,
    },
    /// A rest parameter. Its function is its parent.
    RestParam {
        /// The bound name; absent for an anonymous rest parameter
        name: Option<diag::Span>,
    },
    /// The `self` parameter of a method
    SelfParam {
        /// The bound name
        name: diag::Span,
    },
    /// `import foo` or `import foo: bar`
    ImportModule {
        /// The module path as written
        module: diag::Span,
        /// The bound name
        name: diag::Span,
    },
    /// An item imported from a module
    ImportItem {
        /// The module path as written
        module: diag::Span,
        /// The item name within the module
        item: diag::Span,
        /// The bound name
        name: diag::Span,
    },
    /// A module bound by the prelude, which has no source text
    PreludeModule {
        /// The module path
        module: &'a str,
        /// The bound name
        name: &'a str,
    },
    /// An item bound by the prelude, which has no source text
    PreludeItem {
        /// The module path
        module: &'a str,
        /// The item name within the module
        item: &'a str,
        /// The bound name
        name: &'a str,
    },
    /// A `do` block
    Lambda,
    /// An `if` or `else if` body
    If,
    /// An `else` body
    Else,
    /// A `while` body
    While,
    /// A `for` body
    For,
    /// A `try` body
    Try,
    /// A `catch` handler
    Catch,
    /// A `finally` body
    Finally,
    /// A comprehension `for`
    ForElem,
    /// A comprehension `if`
    IfElem,
    /// A decorator.  What it decorates is its parent.
    Decorator {
        /// The node the decorator expression names, when it is an identifier
        target: Option<NodeId>,
    },
    /// A `break`
    Break {
        /// The loop broken out of
        target: Option<NodeId>,
    },
    /// A `continue`
    Continue {
        /// The loop continued
        target: Option<NodeId>,
    },
    /// A `return`
    Return {
        /// The function returned from
        target: Option<NodeId>,
    },
}

/// A superclass reference
pub struct Super<'a> {
    /// The reference as written
    pub span: diag::Span,
    /// The node it names, when it is an identifier
    pub target: Option<NodeId>,
    phantom: PhantomData<&'a ()>,
}

/// Iterator over a class's superclass references
#[derive(Clone)]
pub struct Supers<'a> {
    file: &'a File<'a>,
    supers: slice::Iter<'a, doc::Super>,
}

impl<'a> Iterator for Supers<'a> {
    type Item = Super<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.supers.next().map(|super_ref| Super {
            span: convert_span(self.file, super_ref.span),
            target: super_ref.target.map(public_node_id),
            phantom: PhantomData,
        })
    }
}

impl VisitAdapter<'_, '_> {
    fn emit_token(
        &mut self,
        token: Token,
        span: source::Span,
        node: Option<doc::Id>,
        context: Context,
    ) -> ControlFlow<Infallible> {
        let diag_span = convert_span(self.file, span);
        let node = node.map(public_node_id);
        self.emit.emit(token, diag_span, node, context);
        ControlFlow::Continue(())
    }
}

impl visit::Visit for VisitAdapter<'_, '_> {
    type Break = Infallible;

    fn node<T: AstNode + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        if matches!(node.kind(), NodeKind::Call) {
            node.accept(&mut CallAdapter {
                parent: self,
                seen_arg0: false,
            })
        } else {
            node.accept(self)
        }
    }

    fn token(
        &mut self,
        token: Token,
        span: source::Span,
        node: Option<doc::Id>,
    ) -> ControlFlow<Self::Break> {
        self.emit_token(token, span, node, Context::None)
    }
}

/// Compilation mode
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq)]
pub enum Mode<'a> {
    /// Compile as script: return value is value of final statement or early return
    Script,
    /// Compile as module: return value is a module of top-level bindings, or that of early return
    Module { name: &'a str },
    /// Compile in REPL mode:
    /// - Return value is a module of top-level bindings, including private bindings (e.g. imports)
    /// - Early return is disallowed at top level
    /// - Value of final statement is bound to `_`
    Repl,
}

#[derive(Debug, Clone)]
pub(crate) struct PreludeItem {
    item: String,
    bind: String,
    res: Option<Res>,
}

#[derive(Debug, Clone)]
pub(crate) enum PreludeImport {
    Items {
        module: String,
        items: Vec<PreludeItem>,
    },
    ModuleAsIs {
        module: String,
        bind: String,
        res: Option<Res>,
        insert: bool,
    },
    ModuleRenamed {
        module: String,
        bind: String,
        res: Option<Res>,
    },
}

/// Prelude configurer
pub struct Prelude<'a, 'b> {
    config: &'b mut Config<'a>,
}

impl<'a, 'b> Prelude<'a, 'b> {
    fn module_name_first(module: &str) -> &str {
        if let Some((first, _)) = module.split_once(".") {
            first
        } else {
            module
        }
    }

    /// Clear the prelude, including any default imports
    pub fn clear(self) -> Self {
        self.config.prelude.clear();
        self
    }

    /// Imports the named module, as by:
    /// ```do
    /// import module
    /// ```
    pub fn import_module(self, module: impl Into<String>) -> Self {
        let module = module.into();
        let bind = Self::module_name_first(&module).to_owned();

        self.config.prelude.push(PreludeImport::ModuleAsIs {
            module,
            bind,
            res: None,
            insert: false,
        });
        self
    }

    /// Imports the named module under a different name, as by:
    /// ```do
    /// import module: name
    /// ```
    pub fn import_module_with_name(
        self,
        module: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        self.config.prelude.push(PreludeImport::ModuleRenamed {
            module: module.into(),
            bind: name.into(),
            res: None,
        });
        self
    }

    /// Import items from a module.  Returns a builder object to configure individual items.
    pub fn import_items(self, module: impl Into<String>) -> Items<'a, 'b> {
        self.config.prelude.push(PreludeImport::Items {
            module: module.into(),
            items: Vec::new(),
        });
        Items {
            config: self.config,
        }
    }
}

/// Item import builder.
#[must_use]
pub struct Items<'a, 'b> {
    config: &'b mut Config<'a>,
}

impl<'a, 'b> Items<'a, 'b> {
    /// Imports the given item, as by:
    /// ```do
    /// import module:
    ///   - item
    /// ```
    pub fn item(self, item: impl Into<String>) -> Self {
        let item = item.into();
        match self.config.prelude.last_mut().unwrap() {
            PreludeImport::Items { items, .. } => items.push(PreludeItem {
                item: item.clone(),
                bind: item,
                res: None,
            }),
            _ => unreachable!(),
        };
        self
    }

    /// Imports the given items, as by:
    /// ```do
    /// import module:
    ///   - item
    ///   - ...
    /// ```
    pub fn items(mut self, items: impl IntoIterator<Item = impl Into<String>>) -> Self {
        for item in items.into_iter() {
            self = self.item(item);
        }
        self
    }

    /// Imports the given item under a different name, as by:
    /// ```do
    /// import module:
    ///   item: name
    /// ```
    pub fn item_with_name(self, item: impl Into<String>, name: impl Into<String>) -> Self {
        match self.config.prelude.last_mut().unwrap() {
            PreludeImport::Items { items, .. } => items.push(PreludeItem {
                item: item.into(),
                bind: name.into(),
                res: None,
            }),
            _ => unreachable!(),
        };
        self
    }

    /// Finish item imports.
    ///
    /// Calling this method may be necessary to ensure changes take effect.
    pub fn commit(self) -> Prelude<'a, 'b> {
        Prelude {
            config: self.config,
        }
    }
}

/// Compiler configuration.
///
/// A configuration carries no source, so it may be reused to build any number of
/// [`Unit`]s.
pub struct Config<'a> {
    mode: Mode<'a>,
    prelude: Vec<PreludeImport>,
    recover: bool,
    document: bool,
}

impl Default for Config<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Config<'a> {
    /// Create a new configuration with the default prelude
    pub fn new() -> Self {
        let mut this = Self {
            mode: Mode::Script,
            prelude: Default::default(),
            recover: false,
            document: false,
        };
        this.prelude()
            .import_module("std")
            .import_module("strand")
            .import_items("std")
            .items(STD_PRELUDE.iter().copied())
            .commit()
            .import_items("strand")
            .items(["fork", "pipeline", "stream", "put", "spawn"])
            .commit();
        this
    }

    /// Change compilation mode
    ///
    /// Default: [`Mode::Script`]
    pub fn mode(&mut self, mode: Mode<'a>) -> &mut Self {
        self.mode = mode;
        self
    }

    /// Recover from errors to the greatest extent possible, retaining a partial tree so
    /// that diagnostics and tokens remain available for malformed source.
    ///
    /// A [`Unit`] which recovered from an error cannot be emitted; [`Unit::emit`] will
    /// fail with [`ErrorKind::Fail`].
    ///
    /// Default: `false`
    pub fn recover(&mut self, recover: bool) -> &mut Self {
        self.recover = recover;
        self
    }

    /// Build document structure and annotate semantic tokens with node identities.
    ///
    /// Default: false. When disabled, node queries are empty and tokens carry no identities.
    pub fn document(&mut self, document: bool) -> &mut Self {
        self.document = document;
        self
    }

    /// Configure a prelude, a collection of standard imports which are injected into the code.
    ///
    /// Note that prelude imports which are not referenced by the code are omitted from compilation, even
    /// if importing them would have side effects.
    pub fn prelude(&mut self) -> Prelude<'a, '_> {
        Prelude { config: self }
    }

    /// Parse and elaborate source into a [`Unit`].
    ///
    /// This operation is infallible: errors are recorded in the unit and surfaced through
    /// [`Unit::diagnostics`] and [`Unit::emit`].
    ///
    /// # Arguments
    /// - `path`: The path of the source file; used in backtraces
    /// - `content`: The source as a byte slice
    pub fn unit<'b>(&self, path: &'b Path, content: &'b [u8]) -> Unit<'b>
    where
        'a: 'b,
    {
        let mut compiler = Compiler {
            file: File::new(path, content),
            symtab: sym::Table::new(),
            bintab: BinTable::new(),
            consttab: constant::Table::new(),
            packtab: sig::PackTable::new(),
            unpacktab: sig::UnpackTable::new(),
            mode: self.mode.clone(),
            prelude: self.prelude.clone(),
        };
        let mut prelude = mem::take(&mut compiler.prelude);
        let diags = Diags::new();
        let mut comments = vec![];

        let (mut ast, mut failed) = {
            let mut collect = |span| comments.push(span);
            let mut parser = compiler.parser(&diags, Some(&mut collect as &mut dyn Comment));
            let ast = parser.parse(self.recover);
            let failed = parser.failed();
            (ast, failed)
        };
        #[cfg(feature = "debug")]
        if let Err(e) = compiler.export_ast_dot(&ast, false) {
            debug_eprintln!("AST DOT export failed: {e}")
        }

        {
            let mut elab = compiler.elaborater(&diags);
            elab.elaborate(&mut ast, &mut prelude);
            failed |= elab.failed();
        }
        #[cfg(feature = "debug")]
        if let Err(e) = compiler.export_ast_dot(&ast, true) {
            debug_eprintln!("Resolved AST DOT export failed: {e}")
        }

        compiler.prelude = prelude;
        let document = self
            .document
            .then(|| doc::index(&mut ast, &mut compiler.prelude, &compiler.file));
        Unit {
            document,
            compiler,
            ast,
            comments,
            diags,
            failed,
        }
    }
}

/// A parsed and elaborated compilation unit.
///
/// A unit retains the compiler state which produced it, so diagnostics are resolved
/// lazily as they are iterated.
pub struct Unit<'a> {
    document: Option<doc::Table>,
    compiler: Compiler<'a>,
    ast: ast::Root,
    comments: Vec<source::Span>,
    diags: Diags,
    failed: bool,
}

impl Unit<'_> {
    /// Iterate diagnostics generated while building the unit.
    ///
    /// Diagnostics are yielded in the order they were generated.
    pub fn diagnostics(&self) -> impl Iterator<Item = diag::Diag> + '_ {
        self.diags.iter().map(|diag| diag.resolve(&self.compiler))
    }

    /// Iterate the document nodes of the unit: its declarations and constructs.
    ///
    /// Requires [`Config::document`]. This reads the index and walks no syntax tree, so
    /// structure can be had without emitting tokens at all.  Each node names its
    /// parent, so the order nodes are yielded carries no meaning; order siblings
    /// by [`Node::span`].
    pub fn nodes(&self) -> impl Iterator<Item = (NodeId, Node<'_>)> {
        std::iter::successors(self.next_id(None), |id| self.next_id(Some(*id))).map(|id| {
            (
                id,
                self.node(id).expect("next_id only returns surfaced nodes"),
            )
        })
    }

    /// Return the next surfaced node identity after `id`, or the first when absent.
    #[doc(hidden)]
    pub fn next_id(&self, id: Option<NodeId>) -> Option<NodeId> {
        let table = self.document.as_ref()?;
        let next = id.map_or(0, |id| internal_node_id(id).index() + 1);
        (next < table.len()).then(|| public_node_id(doc::Id::from_index(next)))
    }

    /// Look up a single document node, as named by a token or by another node.
    ///
    /// Returns `None` for an identity this unit did not produce.
    pub fn node(&self, id: NodeId) -> Option<Node<'_>> {
        let table = self.document.as_ref()?;
        let id = internal_node_id(id);
        if id.index() >= table.len() {
            return None;
        }
        let node = &table[id];
        Some(Node {
            file: &self.compiler.file,
            node,
        })
    }

    /// Emit semantic tokens for the unit.
    ///
    /// The precise order of emitted tokens is not specified, but will be approximately in
    /// textual order.
    ///
    /// # Arguments
    /// - `tokens`: Where to send semantic tokens.
    pub fn tokens(&self, tokens: &mut impl EmitToken) {
        let ControlFlow::Continue(()) = self.ast.accept(&mut VisitAdapter {
            file: &self.compiler.file,
            emit: tokens,
        });
        for comment in self.comments.iter() {
            let content = self.compiler.file.str(*comment);
            let slice = content.trim_end();
            tokens.emit(
                Token::Comment,
                convert_span(
                    &self.compiler.file,
                    source::Span {
                        start: comment.start,
                        end: comment.start + slice.len() as u32,
                    },
                ),
                None,
                ast::Context::None,
            );
        }
    }

    /// Compile the unit, writing bytecode.
    ///
    /// # Arguments
    /// - `write`: Where to write bytecode.
    ///
    /// # Errors
    /// - [`ErrorKind::Fail`]: The unit contains at least one error; consult
    ///   [`Unit::diagnostics`].
    /// - [`ErrorKind::Io`]: Writing bytecode failed with an [`io::Error`].
    pub fn emit(mut self, write: &mut impl Write) -> Result<(), Error> {
        if self.failed {
            return Err(Error(ErrorInfo::Fail));
        }
        let mut lowerer = self.compiler.lowerer();
        let graph = lowerer.run(&self.ast)?;
        #[cfg(feature = "debug")]
        {
            // Export DOT file if environment variable is set
            if let Ok(output) = std::env::var("DOLANG_EXPORT_DOT")
                && let Err(e) = self.compiler.export_cfg_dot(&graph, output)
            {
                debug_eprintln!("DOT export failed: {e}");
            }
        }
        let mut emitter = self.compiler.emitter(&graph);
        Ok(emitter.emit(write)?)
    }
}

/// Compiler state backing a [`Unit`].
pub(crate) struct Compiler<'a> {
    file: File<'a>,
    symtab: sym::Table,
    bintab: BinTable,
    consttab: constant::Table,
    packtab: sig::PackTable,
    unpacktab: sig::UnpackTable,
    mode: Mode<'a>,
    prelude: Vec<PreludeImport>,
}

impl Compiler<'_> {
    fn parser<'b>(
        &'b mut self,
        diags: &'b Diags,
        comment: Option<&'b mut dyn Comment>,
    ) -> Parser<'b> {
        Parser::new(Lexer::new(&self.file, diags, comment), &self.file, diags)
    }

    fn elaborater<'b>(&'b mut self, diags: &'b Diags) -> Elaborater<'b> {
        Elaborater::new(
            self.mode.clone(),
            &self.file,
            &mut self.bintab,
            &mut self.symtab,
            diags,
        )
    }

    fn lowerer(&mut self) -> Lowerer<'_> {
        Lowerer {
            mode: self.mode.clone(),
            file: &self.file,
            symtab: &mut self.symtab,
            bintab: &mut self.bintab,
            consttab: &mut self.consttab,
            packtab: &mut self.packtab,
            unpacktab: &mut self.unpacktab,
            prelude: &self.prelude,
            sentinel_const: None,
        }
    }

    fn emitter<'b>(&'b self, graph: &'b cfg::Graph) -> Emitter<'b> {
        Emitter {
            file: &self.file,
            graph,
            bintab: &self.bintab,
            symtab: &self.symtab,
            consttab: &self.consttab,
            packtab: &self.packtab,
            unpacktab: &self.unpacktab,
            debugbintab: Default::default(),
            mode: self.mode.clone(),
        }
    }
}

#[cfg(feature = "debug")]
impl Compiler<'_> {
    /// Generate a graphviz DOT representation of an AST
    fn ast_to_dot<N: AstNode + ?Sized>(&self, ast: &N, writer: &mut impl Write) -> io::Result<()> {
        use crate::ast::{dot::DotVisitor, visit::Visit};
        use dot_writer::DotWriter;

        let mut writer = DotWriter::from(writer);
        writer.set_pretty_print(true);
        let mut digraph = writer.digraph();
        let mut visitor = DotVisitor::new(&mut digraph, self);
        match visitor.node(ast) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(e) => Err(e),
        }
    }

    /// Export AST to a DOT file based on the DOLANG_EXPORT_DOT environment variable
    /// Similar to the CFG export functionality
    fn export_ast_dot<N: AstNode + ?Sized>(&self, ast: &N, res: bool) -> io::Result<()> {
        use std::{
            fs,
            path::{self, Component},
        };

        if let Ok(output) = std::env::var("DOLANG_EXPORT_DOT") {
            let src = path::absolute(self.file.path())?;
            let cwd = std::env::current_dir()?;
            let rel = src.strip_prefix(&cwd).unwrap_or(&src);
            let comps: Vec<_> = rel
                .components()
                .filter_map(|c| {
                    if let Component::Normal(c) = c {
                        Some(c.to_string_lossy())
                    } else {
                        None
                    }
                })
                .collect();
            let name = comps.join("_");
            fs::create_dir_all(&output)?;
            let out = std::path::Path::new(&output)
                .join(&name)
                .with_extension(if res { "res.dot" } else { "ast.dot" });
            let mut file = fs::File::create(&out)?;

            self.ast_to_dot(ast, &mut file)?;
            debug_eprintln!("AST DOT exported to: {}", out.display());
        }

        Ok(())
    }

    fn export_cfg_dot(&mut self, graph: &cfg::Graph, output: String) -> Result<(), io::Error> {
        use std::{
            fs,
            path::{self, Component},
        };
        let src = path::absolute(self.file.path())?;
        let cwd = std::env::current_dir()?;
        let rel = src.strip_prefix(cwd).unwrap_or(&src);
        let comps: Vec<_> = rel
            .components()
            .filter_map(|c| {
                if let Component::Normal(c) = c {
                    Some(c.to_string_lossy())
                } else {
                    None
                }
            })
            .collect();
        let name = comps.join("_");
        fs::create_dir_all(&output)?;
        let out = Path::new(&output).join(&name).with_extension("cfg.dot");
        let mut file = fs::File::create(&out)?;
        graph.dot(self, &mut file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic_snapshot(unit: &Unit<'_>) -> Vec<String> {
        unit.diagnostics()
            .map(|diag| {
                let mut text =
                    format!("{:?} {:?} {}", diag.severity(), diag.span(), diag.message());
                for annotation in diag.annotations() {
                    text.push_str(&format!(
                        "|{} {:?} {}",
                        match annotation.kind() {
                            diag::AnnotationKind::Primary => "primary",
                            diag::AnnotationKind::Context => "context",
                        },
                        annotation.span(),
                        annotation.message()
                    ));
                }
                for note in diag.notes() {
                    text.push_str(&format!(
                        "|{} {}",
                        match note.kind() {
                            diag::NoteKind::Info => "info",
                            diag::NoteKind::Help => "help",
                        },
                        note.message()
                    ));
                }
                for patch in diag.patches() {
                    text.push_str(&format!(
                        "|{:?} {} {}",
                        patch.span(),
                        patch.message(),
                        patch.sub()
                    ));
                }
                text
            })
            .collect()
    }

    fn token_snapshot(unit: &Unit<'_>) -> Vec<String> {
        let mut tokens = Vec::new();
        unit.tokens(&mut |token, span, node, context| {
            if let Some(id) = node {
                assert!(unit.node(id).is_some());
            }
            tokens.push(format!("{token:?} {span:?} {context:?}"));
        });
        tokens
    }

    #[test]
    fn document_indexing_preserves_compilation_and_diagnostics() {
        let sources: &[&[u8]] = &[
            include_bytes!("../../dodo.dol"),
            include_bytes!("../../dolang-shell-modules/lib/transfer.dol"),
            include_bytes!("../../dolang-shell-modules/lib/test.dol"),
            b"def f unused\n  let x = 1\n  do x\nf 2\n",
            b"class C\n  field private = 1\n  pub def get self\n    self.private\n  pub def set self\n    self.private = 2\n",
            b"class C\n  #[static]\n  pub field x = str(1)\n  #[class]\n  pub field y = str(2)\nC\n",
            b"let static = 1\nclass C\n  #[static]\n  pub field x = str(1)\nC\n",
            b"import std\nimport std: s\nlet x = 1\ns.str(x)\n",
            b"def f()\n  while true\n    (do break)\n  return 1\nf()\n",
            b"let a = [1, 2]\nlet b = [for x = a do x]\nb\n",
            b"def f x\n  return x\nlet =\n",
        ];
        for mode in [Mode::Script, Mode::Module { name: "parity" }, Mode::Repl] {
            for source in sources {
                let mut config = Config::new();
                config.mode(mode.clone()).recover(true);
                let plain = config.unit(Path::new("parity.dol"), source);
                let indexed = config.document(true).unit(Path::new("parity.dol"), source);
                assert!(plain.document.is_none());
                assert!(plain.nodes().next().is_none());
                assert!(plain.next_id(None).is_none());
                plain.tokens(&mut |_, _, id: Option<NodeId>, _| assert!(id.is_none()));
                if let Some((id, _)) = indexed.nodes().next() {
                    assert!(plain.node(id).is_none());
                }
                assert_eq!(diagnostic_snapshot(&plain), diagnostic_snapshot(&indexed));
                assert_eq!(token_snapshot(&plain), token_snapshot(&indexed));
                assert_eq!(plain.failed, indexed.failed);
                let (mut left, mut right) = (Vec::new(), Vec::new());
                assert_eq!(
                    plain.emit(&mut left).is_ok(),
                    indexed.emit(&mut right).is_ok()
                );
                assert_eq!(left, right);
            }
        }
    }

    #[test]
    fn references_follow_lexical_scopes_and_forward_declarations() {
        let source = b"def first x\n  second $x\ndef second x\n  let saved = x\n  let closure = do |x| saved\n  closure $x\nfirst 1\n";
        let unit = config().unit(Path::new("refs.dol"), source);
        assert!(!unit.failed);
        let mut refs = Vec::new();
        unit.tokens(&mut |_, span: diag::Span, id, _| {
            if let Some(id) = id {
                let node = unit.node(id).unwrap();
                refs.push((span.start().byte_offset(), declared_name(&unit, &node), id));
            }
        });
        let named: Vec<_> = refs
            .iter()
            .filter(|(_, name, _)| name == "second")
            .collect();
        assert_eq!(named.len(), 2);
        assert_eq!(named[0].2, named[1].2);
        let saved: Vec<_> = refs.iter().filter(|(_, name, _)| name == "saved").collect();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].2, saved[1].2);
        let xs: std::collections::HashSet<_> = refs
            .iter()
            .filter(|(_, name, _)| name == "x")
            .map(|(_, _, id)| id)
            .collect();
        assert_eq!(xs.len(), 3);
    }

    #[test]
    fn shared_import_bindings_keep_distinct_declarations() {
        let unit = config().unit(Path::new("imports.dol"), b"import std\nimport std\nstd\n");
        assert!(!unit.failed);
        let imports: Vec<_> = unit
            .nodes()
            .filter(|(_, node)| matches!(node.kind(), Kind::ImportModule { .. }))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(imports.len(), 2);
        assert_ne!(imports[0], imports[1]);
        let mut ids = Vec::new();
        unit.tokens(&mut |_, _, id, _| {
            if let Some(id) = id {
                ids.push(id);
            }
        });
        assert_eq!(ids, vec![imports[0], imports[1], imports[1]]);
    }

    #[test]
    fn anonymous_parameters_and_nonlocal_jumps_are_indexed() {
        let source = b"def invoke block\n  block()\ndef f ...\n  while true\n    invoke do continue\n    invoke do break\n  invoke do return 1\n";
        let unit = config().unit(Path::new("jumps.dol"), source);
        assert!(!unit.failed, "{:?}", diagnostic_snapshot(&unit));
        assert!(
            unit.nodes()
                .any(|(_, node)| matches!(node.kind(), Kind::RestParam { name: None }))
        );
        let mut jumps = Vec::new();
        for (_, node) in unit.nodes() {
            let (kind, target) = match node.kind() {
                Kind::Break { target } => ("break", target),
                Kind::Continue { target } => ("continue", target),
                Kind::Return { target } => ("return", target),
                _ => continue,
            };
            assert!(matches!(
                unit.node(node.parent().unwrap()).unwrap().kind(),
                Kind::Lambda
            ));
            let target = unit.node(target.unwrap()).unwrap();
            jumps.push((kind, kind_name(&target.kind()).to_owned()));
        }
        assert_eq!(
            jumps,
            vec![
                ("continue", "while".into()),
                ("break", "while".into()),
                ("return", "function".into())
            ]
        );
    }

    #[test]
    fn static_initializers_resolve_in_the_enclosing_scope() {
        let mut config = Config::new();
        config.document(true);
        let unit = config.unit(
            Path::new("static.dol"),
            b"let x = 1\nclass C\n  #[static]\n  pub field y = str(x)\nC\n",
        );
        assert!(!unit.failed);
        assert!(!unit.nodes().any(|(_, node)| matches!(
            node.kind(),
            Kind::Function { .. } | Kind::Lambda | Kind::SelfParam { .. }
        )));
        let mut xs = Vec::new();
        unit.tokens(&mut |_, _, id, _| {
            if let Some(id) = id
                && declared_name(&unit, &unit.node(id).unwrap()) == "x"
            {
                xs.push(id);
            }
        });
        assert_eq!(xs.len(), 2);
        assert_eq!(xs[0], xs[1]);
    }

    fn config<'a>() -> Config<'a> {
        let mut config = Config::new();
        config.document(true);
        // The default prelude imports modules which are not available here
        config.prelude().clear();
        config
    }

    #[test]
    fn valid_source_emits_bytecode() {
        let unit = config().unit(Path::new("<test>"), b"let x = 1\nx\n");
        assert!(
            !unit
                .diagnostics()
                .any(|d| d.severity() == diag::Severity::Error)
        );
        let mut tokens = 0;
        unit.tokens(&mut |_, _, _, _| tokens += 1);
        assert!(tokens != 0);
        let mut out = Vec::new();
        unit.emit(&mut out).unwrap();
        assert!(!out.is_empty());
    }

    #[test]
    fn recovered_unit_reports_diagnostics_and_refuses_to_emit() {
        let source = b"let x = 1\nlet = 2\n";
        let unit = config().recover(true).unit(Path::new("<test>"), source);

        let diags: Vec<_> = unit.diagnostics().collect();
        assert!(diags.iter().any(|d| d.severity() == diag::Severity::Error));

        // Tokens remain available despite the error
        let mut tokens = 0;
        unit.tokens(&mut |_, _, _, _| tokens += 1);
        assert!(tokens != 0);

        let mut out = Vec::new();
        assert!(matches!(
            unit.emit(&mut out).unwrap_err().kind(),
            ErrorKind::Fail
        ));
    }

    #[test]
    fn unrecovered_unit_reports_diagnostics() {
        let source = b"let x = 1\nlet = 2\n";
        let unit = config().unit(Path::new("<test>"), source);

        assert!(
            unit.diagnostics()
                .any(|d| d.severity() == diag::Severity::Error)
        );
        let mut out = Vec::new();
        assert!(matches!(
            unit.emit(&mut out).unwrap_err().kind(),
            ErrorKind::Fail
        ));
    }

    /// Collect `(kind name, declared name, parent's declared name)` for every
    /// node, so a test can state the tree without depending on node identities.
    fn tree(unit: &Unit<'_>) -> Vec<(&'static str, String, Option<String>)> {
        let named = |id: NodeId| -> Option<String> {
            let node = unit.node(id)?;
            Some(node_name(unit, &node.kind()))
        };
        let mut rows: Vec<_> = unit
            .nodes()
            .map(|(_, node)| {
                (
                    kind_name(&node.kind()),
                    node_name(unit, &node.kind()),
                    node.parent().and_then(named),
                )
            })
            .collect();
        rows.sort();
        rows
    }

    fn kind_name(kind: &Kind<'_>) -> &'static str {
        match kind {
            Kind::Class { .. } => "class",
            Kind::Function { .. } => "function",
            Kind::Method { .. } => "method",
            Kind::SpecialMethod { .. } => "special_method",
            Kind::Field { .. } => "field",
            Kind::Bind { .. } => "bind",
            Kind::PositionalParam { .. } => "positional_param",
            Kind::KeyParam { .. } => "key_param",
            Kind::RestParam { .. } => "rest_param",
            Kind::SelfParam { .. } => "self",
            Kind::ImportModule { .. } => "import_module",
            Kind::ImportItem { .. } => "import_item",
            Kind::PreludeModule { .. } => "prelude_module",
            Kind::PreludeItem { .. } => "prelude_item",
            Kind::Lambda => "lambda",
            Kind::If => "if",
            Kind::Else => "else",
            Kind::While => "while",
            Kind::For => "for",
            Kind::Try => "try",
            Kind::Catch => "catch",
            Kind::Finally => "finally",
            Kind::ForElem => "for_elem",
            Kind::IfElem => "if_elem",
            Kind::Decorator { .. } => "decorator",
            Kind::Break { .. } => "break",
            Kind::Continue { .. } => "continue",
            Kind::Return { .. } => "return",
        }
    }

    /// The source text of whatever a node declares, or its kind if it declares
    /// no name.
    fn node_name(unit: &Unit<'_>, kind: &Kind<'_>) -> String {
        let text = |span: &diag::Span| {
            unit.compiler.file.str(source::Span {
                start: span.start().byte_offset() as source::Offset,
                end: span.end().byte_offset() as source::Offset,
            })
        };
        match kind {
            Kind::Class { name, .. }
            | Kind::Function { name, .. }
            | Kind::Method { name, .. }
            | Kind::Field { name, .. }
            | Kind::Bind { name, .. }
            | Kind::SelfParam { name }
            | Kind::ImportModule { name, .. }
            | Kind::ImportItem { name, .. } => text(name).to_owned(),
            // The span names the identifier; the kind is what says it is a
            // protocol name and so is written in parentheses.
            Kind::SpecialMethod { name } => format!("({})", text(name)),
            Kind::PositionalParam { name, .. } | Kind::KeyParam { name, .. } => {
                text(name).to_owned()
            }
            Kind::RestParam { name } => name.as_ref().map_or("...", text).to_owned(),
            Kind::PreludeModule { name, .. } | Kind::PreludeItem { name, .. } => (*name).to_owned(),
            other => kind_name(other).to_owned(),
        }
    }

    /// Nonzero identities keep optional node links pointer-free.
    #[test]
    fn an_optional_node_identity_is_no_wider_than_a_node_identity() {
        assert_eq!(
            mem::size_of::<Option<NodeId>>(),
            mem::size_of::<NodeId>(),
            "NodeId should be niche-optimized"
        );
        assert_eq!(
            mem::size_of::<Option<doc::Id>>(),
            mem::size_of::<doc::Id>(),
            "doc::Id should be niche-optimized"
        );
    }

    /// The source text of the name a node declares.
    fn declared_name(unit: &Unit<'_>, node: &Node<'_>) -> String {
        node_name(unit, &node.kind())
    }

    #[test]
    fn declarations_parent_to_the_construct_that_encloses_them() {
        let unit = config().unit(
            Path::new("<test>"),
            b"class Point\n  pub field x = 0\n\n  pub def (init) self x\n    self.x = x\n\n  def hidden self\n    let local = 1\n",
        );
        let tree = tree(&unit);

        // The class is the parent of its members; the members are the parents of
        // their own parameters and bindings.
        assert!(tree.contains(&("class", "Point".into(), None)));
        assert!(tree.contains(&("field", "x".into(), Some("Point".into()))));
        assert!(tree.contains(&("special_method", "(init)".into(), Some("Point".into()))));
        assert!(tree.contains(&("method", "hidden".into(), Some("Point".into()))));
        assert!(tree.contains(&("self", "self".into(), Some("(init)".into()))));
        assert!(tree.contains(&("positional_param", "x".into(), Some("(init)".into()))));
        assert!(tree.contains(&("bind", "local".into(), Some("hidden".into()))));
    }

    /// A method reports its visibility; a special method has none to report.
    ///
    /// A special method is part of the type's interface however it was
    /// declared, so it is a kind of its own rather than a `Method` carrying an
    /// option that says to ignore its visibility.
    #[test]
    fn methods_record_visibility_and_special_methods_name_their_protocol() {
        let unit = config().unit(
            Path::new("<test>"),
            b"class Point\n  pub def (init) self\n    self\n\n  def (str) self\n    self\n\n  pub def visible self\n    self\n\n  def hidden self\n    self\n",
        );
        let mut methods = Vec::new();
        let mut special = Vec::new();
        for (_, node) in unit.nodes() {
            let kind = node.kind();
            match kind {
                Kind::Method { is_pub, .. } => methods.push((node_name(&unit, &kind), is_pub)),
                Kind::SpecialMethod { .. } => special.push(node_name(&unit, &kind)),
                _ => {}
            }
        }
        methods.sort();
        special.sort();
        assert_eq!(
            methods,
            vec![("hidden".to_string(), false), ("visible".to_string(), true)]
        );
        // `(str)` is declared without `pub`, but it is interface all the same.
        assert_eq!(special, vec!["(init)".to_string(), "(str)".to_string()]);
    }

    #[test]
    fn parameters_record_form_and_whether_they_have_a_default() {
        let unit = config().unit(
            Path::new("<test>"),
            b"def connect host :port = 8080 ...rest\n  nil\n\ndef ignore first ...\n  first\n",
        );
        let mut params = Vec::new();
        for (_, node) in unit.nodes() {
            let (name, form, has_default) = match node.kind() {
                Kind::PositionalParam { name, default } => {
                    (Some(name), "positional", default.is_some())
                }
                Kind::KeyParam { name, default, .. } => (Some(name), "key", default.is_some()),
                Kind::RestParam { name } => (name, "rest", false),
                _ => continue,
            };
            let name = name.as_ref().map(|name| {
                unit.compiler
                    .file
                    .str(source::Span {
                        start: name.start().byte_offset() as source::Offset,
                        end: name.end().byte_offset() as source::Offset,
                    })
                    .to_owned()
            });
            params.push((name, form, has_default));
        }
        // An anonymous `...` binds nothing, so it has no name — but it is part
        // of the signature and so must still be a node.
        assert_eq!(
            params,
            vec![
                (Some("host".to_string()), "positional", false),
                (Some("port".to_string()), "key", true),
                (Some("rest".to_string()), "rest", false),
                (Some("first".to_string()), "positional", false),
                (None, "rest", false),
            ]
        );
    }

    #[test]
    fn jumps_name_the_construct_they_target() {
        let unit = config().unit(
            Path::new("<test>"),
            b"def loop_forever count\n  while (count > 0)\n    break\n  return 1\n",
        );
        let target_kind = |id: Option<NodeId>| {
            id.and_then(|id| unit.node(id))
                .map(|node| kind_name(&node.kind()))
        };
        let mut jumps = Vec::new();
        for (_, node) in unit.nodes() {
            match node.kind() {
                // A local `break` records nothing in the elaborator's non-local
                // jump bookkeeping, so this would be empty if the node stream
                // only reported the non-local case.
                Kind::Break { target } => jumps.push(("break", target_kind(target))),
                Kind::Return { target } => jumps.push(("return", target_kind(target))),
                _ => {}
            }
        }
        jumps.sort();
        assert_eq!(
            jumps,
            vec![("break", Some("while")), ("return", Some("function"))]
        );
    }

    #[test]
    fn sibling_constructs_that_bind_the_same_name_stay_distinct() {
        // Both handlers bind `err`.  Without a node per handler they would share
        // a parent, and anything keyed on (parent, name) would silently collide.
        let unit = config().unit(
            Path::new("<test>"),
            b"import std\ndef risky x\n  try\n    x\n  catch std.TypeError: err\n    err\n  catch err\n    err\n",
        );
        let catches: Vec<_> = unit
            .nodes()
            .filter(|(_, node)| matches!(node.kind(), Kind::Catch))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(catches.len(), 2);

        let binds: Vec<_> = unit
            .nodes()
            .filter_map(|(_, node)| match node.kind() {
                Kind::PositionalParam { .. } | Kind::KeyParam { .. } | Kind::RestParam { .. } => {
                    node.parent()
                }
                _ => None,
            })
            .filter(|parent| catches.contains(parent))
            .collect();
        assert_eq!(binds.len(), 2);
        assert_ne!(binds[0], binds[1]);
    }

    #[test]
    fn decorators_are_nodes_naming_what_they_resolve_to() {
        let mut config = Config::new();
        config.document(true);
        config
            .prelude()
            .clear()
            .import_items("std")
            .items(["getter"])
            .commit();
        let unit = config.unit(
            Path::new("<test>"),
            b"class Point\n  #[getter]\n  pub def x self\n    0\n",
        );

        let getter = unit
            .nodes()
            .find(|(_, node)| matches!(node.kind(), Kind::PreludeItem { item: "getter", .. }))
            .map(|(id, _)| id)
            .expect("prelude `getter` should be a node");

        let decorator = unit
            .nodes()
            .find_map(|(_, node)| match node.kind() {
                Kind::Decorator { target } => Some((node.parent(), target)),
                _ => None,
            })
            .expect("decorator should be a node");
        assert_eq!(decorator.1, Some(getter));

        // The decorator hangs off the method it decorates, so a consumer needs
        // no source text to know which declaration carries it.
        let method = unit
            .nodes()
            .find(|(_, node)| matches!(node.kind(), Kind::Method { .. }))
            .map(|(id, _)| id);
        assert_eq!(decorator.0, method);
    }

    #[test]
    fn unused_prelude_imports_are_not_surfaced() {
        let mut config = Config::new();
        config.document(true);
        config
            .prelude()
            .clear()
            .import_items("std")
            .items(["getter", "static"])
            .commit();
        let unit = config.unit(
            Path::new("<test>"),
            b"class Point\n  #[getter]\n  pub def x self\n    0\n",
        );

        let items: Vec<_> = unit
            .nodes()
            .filter_map(|(_, node)| match node.kind() {
                Kind::PreludeItem { item, .. } => Some(item),
                _ => None,
            })
            .collect();
        assert_eq!(items, vec!["getter"]);
    }
    #[test]
    fn superclasses_name_the_import_they_resolve_to() {
        let unit = config().unit(
            Path::new("<test>"),
            b"import std\nimport std:\n  - Record\n\nclass Named: Record\n  pub field name = nil\n\nclass Tagged: std.Record\n  pub field tag = nil\n",
        );
        let item = unit
            .nodes()
            .find(|(_, node)| matches!(node.kind(), Kind::ImportItem { .. }))
            .map(|(id, _)| id)
            .expect("`Record` should be an import item");

        let supers: Vec<_> = unit
            .nodes()
            .filter_map(|(_, node)| match node.kind() {
                Kind::Class { supers, .. } => Some((
                    declared_name(&unit, &node),
                    supers.map(|s| s.target).collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect();

        // A bare identifier resolves to the declaration it names; a dotted path
        // is an expression, so there is nothing to point at and the consumer is
        // left with the span.
        assert_eq!(
            supers,
            vec![
                ("Named".to_string(), vec![Some(item)]),
                ("Tagged".to_string(), vec![None]),
            ]
        );
    }

    #[test]
    fn field_decorators_resolve_to_the_prelude_item_that_names_the_scope() {
        let mut config = Config::new();
        config.document(true);
        config
            .prelude()
            .clear()
            .import_items("std")
            .items(["class", "static"])
            .commit();
        let unit = config.unit(
            Path::new("<test>"),
            b"class Counter\n  #[class]\n  pub field total = 0\n\n  pub field n = 0\n",
        );

        let prelude: Vec<_> = unit
            .nodes()
            .filter_map(|(id, node)| match node.kind() {
                Kind::PreludeItem { item, .. } => Some((item, id)),
                _ => None,
            })
            .collect();
        let class = prelude
            .iter()
            .find(|(item, _)| *item == "class")
            .map(|(_, id)| *id)
            .expect("prelude `class` should be a node");

        // `#[class]` is a decorator node parented to the field, targeting the
        // prelude import — the same test the elaborator applies, so a consumer
        // decides field scope by identity rather than by matching source text.
        let decorated: Vec<_> = unit
            .nodes()
            .filter_map(|(_, node)| match node.kind() {
                Kind::Decorator { target } => Some((node.parent(), target)),
                _ => None,
            })
            .collect();
        assert_eq!(decorated.len(), 1);
        assert_eq!(decorated[0].1, Some(class));

        let total = unit
            .nodes()
            .find(|(_, node)| {
                matches!(node.kind(), Kind::Field { .. }) && declared_name(&unit, node) == "total"
            })
            .map(|(id, _)| id);
        assert_eq!(decorated[0].0, total);
    }
}
