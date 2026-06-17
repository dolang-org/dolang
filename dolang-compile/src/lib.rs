#![deny(warnings)]

pub(crate) mod ast;
pub(crate) mod cfg;
pub(crate) mod constant;
pub mod diag;
pub(crate) mod elab;
pub(crate) mod emit;
pub(crate) mod flow;
pub(crate) mod lex;
pub(crate) mod lower;
pub(crate) mod origin;
pub(crate) mod parse;
pub(crate) mod sig;
pub mod source;
pub(crate) mod sym;

use std::{
    error,
    fmt::{self, Display},
    io::{self, Write},
    mem,
    ops::ControlFlow,
    path::Path,
};

use crate::{ast::visit, lex::Comment};

use self::{
    ast::visit::{Node, NodeKind},
    elab::Elaborater,
    emit::Emitter,
    lex::Lexer,
    lower::Lowerer,
    parse::Parser,
    source::{Diags, File},
};

pub use ast::{Context, visit::Token};

use dolang_util::intern::{self, BinTable};

use ast::Res;

const STD_PRELUDE: &[&str] = &[
    "array", "bin", "bool", "dbg", "dict", "float", "func", "int", "module", "range", "record",
    "property", "set", "str", "sym", "tuple", "type",
];

#[derive(Debug)]
enum ErrorInfo<B> {
    Fail,
    Io(io::Error),
    Break(B),
}

/// Kind of compilation error.
#[derive(Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Compilation failed; consult diagnostics.
    Fail,
    /// I/O error emitting bytecode.
    Io,
    /// Diagnostic or token visitor stopped execution.
    Break,
}

/// Compile error
#[derive(Debug)]
pub struct Error<B>(ErrorInfo<B>);

impl<B> Error<B> {
    /// Get kind of error
    pub fn kind(&self) -> ErrorKind {
        match &self.0 {
            ErrorInfo::Fail => ErrorKind::Fail,
            ErrorInfo::Io(_) => ErrorKind::Io,
            ErrorInfo::Break(_) => ErrorKind::Break,
        }
    }

    /// Get underlying [`io::Error`], if applicable
    pub fn as_io(&self) -> Option<&io::Error> {
        match &self.0 {
            ErrorInfo::Io(error) => Some(error),
            _ => None,
        }
    }

    /// Get underlying `B`, if applicable
    pub fn as_break(&self) -> Option<&B> {
        match &self.0 {
            ErrorInfo::Break(b) => Some(b),
            _ => None,
        }
    }
}

impl<B> From<io::Error> for Error<B> {
    fn from(value: io::Error) -> Self {
        Error(ErrorInfo::Io(value))
    }
}

impl<B: Display> Display for Error<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ErrorInfo::Fail => "compilation failed".fmt(f),
            ErrorInfo::Io(e) => e.fmt(f),
            ErrorInfo::Break(b) => b.fmt(f),
        }
    }
}

impl<B: error::Error> error::Error for Error<B> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.0 {
            ErrorInfo::Fail => None,
            ErrorInfo::Io(error) => Some(error),
            ErrorInfo::Break(b) => b.source(),
        }
    }
}

impl<B> From<parse::Error> for Error<B> {
    fn from(_: parse::Error) -> Self {
        Self(ErrorInfo::Fail)
    }
}

impl<B> From<elab::Error> for Error<B> {
    fn from(_: elab::Error) -> Self {
        Self(ErrorInfo::Fail)
    }
}

impl<B> From<lower::Error> for Error<B> {
    fn from(_: lower::Error) -> Self {
        Self(ErrorInfo::Fail)
    }
}

/// Diagnostic emitter
pub trait EmitDiag {
    /// Information supplied when stopping compilation
    type Break;

    /// Emit diagnostic
    fn emit(&mut self, diag: diag::Diag) -> ControlFlow<Self::Break>;
}

/// Callback function as diagnostic emitter
impl<F, B> EmitDiag for F
where
    F: FnMut(diag::Diag) -> ControlFlow<B>,
{
    type Break = B;

    fn emit(&mut self, diag: diag::Diag) -> ControlFlow<Self::Break> {
        self(diag)
    }
}

/// Origin of a resolved identifier
pub use diag::Origin;

/// Token emitter
pub trait EmitToken {
    /// Information supplied when stopping analysis
    type Break;

    /// Emit token
    fn emit(
        &mut self,
        token: Token,
        span: diag::Span,
        origin: Option<diag::Origin>,
        context: Context,
    ) -> ControlFlow<Self::Break>;
}

/// Callback function as token emitter
impl<F, B> EmitToken for F
where
    F: FnMut(Token, diag::Span, Option<diag::Origin>, Context) -> ControlFlow<B>,
{
    type Break = B;

    fn emit(
        &mut self,
        token: Token,
        span: diag::Span,
        origin: Option<diag::Origin>,
        context: Context,
    ) -> ControlFlow<Self::Break> {
        self(token, span, origin, context)
    }
}

struct VisitAdapter<'a, 'e, B> {
    file: &'a File<'a>,
    origintab: &'a origin::Table,
    emit: &'e mut dyn EmitToken<Break = B>,
}

struct CallAdapter<'a, 'b, 'e, B> {
    parent: &'b mut VisitAdapter<'a, 'e, B>,
    seen_arg0: bool,
}

struct CallIdentAdapter<'a, 'b, 'e, B>(&'b mut VisitAdapter<'a, 'e, B>);

impl<'a, 'b, 'e, B> visit::Visit for CallIdentAdapter<'a, 'b, 'e, B> {
    type Break = B;

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        self.0.node(node)
    }

    fn token(
        &mut self,
        _token: Token,
        span: source::Span,
        origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break> {
        self.0
            .emit_token(Token::Variable, span, origin, Context::Call)
    }
}

struct MethodAdapter<'a, 'b, 'e, B>(&'b mut VisitAdapter<'a, 'e, B>);

impl<'a, 'b, 'e, B> visit::Visit for MethodAdapter<'a, 'b, 'e, B> {
    type Break = B;

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
        self.0.node(node)
    }

    fn token(
        &mut self,
        token: Token,
        span: source::Span,
        origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break> {
        let context = match token {
            Token::Field => Context::Call,
            _ => Context::None,
        };
        self.0.emit_token(token, span, origin, context)
    }
}

impl<'a, 'b, 'e, B> visit::Visit for CallAdapter<'a, 'b, 'e, B> {
    type Break = B;

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
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
        origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break> {
        self.parent.emit_token(token, span, origin, Context::None)
    }
}

fn convert_span(file: &File, span: source::Span) -> diag::Span {
    let coords = file.coord_span(span);
    diag::Span::new(
        diag::Pos::new(span.start as usize, coords.start.line, coords.start.column),
        diag::Pos::new(span.end as usize, coords.end.line, coords.end.column),
    )
}

fn convert_origin(file: &File, internal: &origin::Origin) -> Option<diag::Origin> {
    match internal {
        origin::Origin::ImportItem { module, item, name } => Some(diag::Origin::ImportItem {
            module: convert_span(file, *module),
            item: convert_span(file, *item),
            name: convert_span(file, *name),
        }),
        origin::Origin::ImportModule { module, name } => Some(diag::Origin::ImportModule {
            module: convert_span(file, *module),
            name: convert_span(file, *name),
        }),
        origin::Origin::PreludeModule { module, name } => Some(diag::Origin::PreludeModule {
            module: module.clone(),
            name: name.clone(),
        }),
        origin::Origin::PreludeItem { module, item, name } => Some(diag::Origin::PreludeItem {
            module: module.clone(),
            item: item.clone(),
            name: name.clone(),
        }),
        origin::Origin::Class { span } => Some(diag::Origin::Class {
            span: convert_span(file, *span),
        }),
        origin::Origin::Def { span, class } => Some(diag::Origin::Def {
            span: convert_span(file, *span),
            class: class.map(|s| convert_span(file, s)),
        }),
        origin::Origin::Bind { span, class } => Some(diag::Origin::Bind {
            span: convert_span(file, *span),
            class: class.map(|s| convert_span(file, s)),
        }),
        origin::Origin::Param { span } => Some(diag::Origin::Param {
            span: convert_span(file, *span),
        }),
        origin::Origin::SelfParam { span } => Some(diag::Origin::SelfParam {
            span: convert_span(file, *span),
        }),
        origin::Origin::Synthetic | origin::Origin::Repl => None,
    }
}

impl<'a, 'e, B> VisitAdapter<'a, 'e, B> {
    fn emit_token(
        &mut self,
        token: Token,
        span: source::Span,
        origin: Option<origin::Id>,
        context: Context,
    ) -> ControlFlow<B> {
        let diag_span = convert_span(self.file, span);
        let diag_origin = origin.and_then(|id| convert_origin(self.file, &self.origintab[id]));
        self.emit.emit(token, diag_span, diag_origin, context)
    }
}

impl<'a, 'e, B> visit::Visit for VisitAdapter<'a, 'e, B> {
    type Break = B;

    fn node<T: Node + ?Sized>(&mut self, node: &T) -> ControlFlow<Self::Break> {
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
        origin: Option<origin::Id>,
    ) -> ControlFlow<Self::Break> {
        self.emit_token(token, span, origin, Context::None)
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

#[derive(Debug)]
pub(crate) struct PreludeItem {
    item: String,
    bind: String,
    res: Option<Res>,
}

#[derive(Debug)]
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
    compiler: &'b mut Compiler<'a>,
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
        self.compiler.prelude.clear();
        self
    }

    /// Imports the named module, as by:
    /// ```do
    /// import module
    /// ```
    pub fn import_module(self, module: impl Into<String>) -> Self {
        let module = module.into();
        let bind = Self::module_name_first(&module).to_owned();

        self.compiler.prelude.push(PreludeImport::ModuleAsIs {
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
        self.compiler.prelude.push(PreludeImport::ModuleRenamed {
            module: module.into(),
            bind: name.into(),
            res: None,
        });
        self
    }

    /// Import items from a module.  Returns a builder object to configure individual items.
    pub fn import_items(self, module: impl Into<String>) -> Items<'a, 'b> {
        self.compiler.prelude.push(PreludeImport::Items {
            module: module.into(),
            items: Vec::new(),
        });
        Items {
            compiler: self.compiler,
        }
    }
}

/// Item import builder.
#[must_use]
pub struct Items<'a, 'b> {
    compiler: &'b mut Compiler<'a>,
}

impl<'a, 'b> Items<'a, 'b> {
    /// Imports the given item, as by:
    /// ```do
    /// import module:
    ///   - item
    /// ```
    pub fn item(self, item: impl Into<String>) -> Self {
        let item = item.into();
        match self.compiler.prelude.last_mut().unwrap() {
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
        match self.compiler.prelude.last_mut().unwrap() {
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
            compiler: self.compiler,
        }
    }
}

/// Compiles Do source to bytecode.
pub struct Compiler<'a> {
    file: File<'a>,
    origintab: origin::Table,
    symtab: sym::Table,
    bintab: BinTable,
    consttab: constant::Table,
    packtab: sig::PackTable,
    unpacktab: sig::UnpackTable,
    mode: Mode<'a>,
    prelude: Vec<PreludeImport>,
}

impl<'a> Compiler<'a> {
    /// Create new compiler instance
    ///
    /// # Arguments
    /// - `path`: The path of the source file; used in backtraces
    /// - `content`: The source as a byte slice
    pub fn new(path: &'a Path, content: &'a [u8]) -> Self {
        let mut this = Self {
            file: File::new(path, content),
            origintab: Default::default(),
            symtab: sym::Table::new(),
            bintab: BinTable::new(),
            consttab: constant::Table::new(),
            packtab: sig::PackTable::new(),
            unpacktab: sig::UnpackTable::new(),
            mode: Mode::Script,
            prelude: Default::default(),
        };
        this.prelude()
            .import_module("std")
            .import_module("strand")
            .import_items("std")
            .items(STD_PRELUDE.iter().copied())
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

    /// Configure a prelude, a collection of standard imports which are injected into the code.
    ///
    /// Note that prelude imports which are not referenced by the code are omitted from compilation, even
    /// if importing them would have side effects.
    pub fn prelude(&mut self) -> Prelude<'a, '_> {
        Prelude { compiler: self }
    }

    fn feed_comments<B>(
        &self,
        tokens: &mut dyn EmitToken<Break = B>,
        comments: impl IntoIterator<Item = source::Span>,
    ) -> Result<(), Error<B>> {
        for comment in comments.into_iter() {
            let content = self.file.str(comment);
            let slice = content.trim_end();
            if let ControlFlow::Break(b) = tokens.emit(
                Token::Comment,
                convert_span(
                    &self.file,
                    source::Span {
                        start: comment.start,
                        end: comment.start + slice.len() as u32,
                    },
                ),
                None,
                ast::Context::None,
            ) {
                return Err(Error(ErrorInfo::Break(b)));
            }
        }
        Ok(())
    }

    fn run<D: EmitDiag>(
        mut self,
        diags: &mut D,
        tokens: Option<&mut dyn EmitToken<Break = D::Break>>,
        write: Option<&mut dyn Write>,
        ignore_errors: bool,
    ) -> Result<(), Error<D::Break>> {
        let mut prelude = mem::take(&mut self.prelude);
        let mut ds = Diags::new();
        let mut comments = vec![];
        let comment = if tokens.is_some() {
            Some((&mut |span| comments.push(span)) as &mut dyn Comment)
        } else {
            None
        };
        let mut parser = self.parser(&ds, comment);
        let ast = parser.parse(ignore_errors);
        self.drain_diags(&mut ds, diags)?;
        let mut ast = ast?;
        #[cfg(feature = "debug")]
        if let Err(e) = self.export_ast_dot(&ast, false) {
            eprintln!("AST DOT export failed: {e}")
        }
        let mut elab = self.elaborater(&ds);
        let res = elab.elaborate(&mut ast, &mut prelude, ignore_errors);
        #[cfg(feature = "debug")]
        if let Err(e) = self.export_ast_dot(&ast, true) {
            eprintln!("Resolved AST DOT export failed: {e}")
        }
        self.drain_diags(&mut ds, diags)?;
        res?;
        self.prelude = prelude;
        if let Some(tokens) = tokens {
            if let ControlFlow::Break(e) = ast.accept(&mut VisitAdapter {
                file: &self.file,
                origintab: &self.origintab,
                emit: tokens,
            }) {
                return Err(Error(ErrorInfo::Break(e)));
            }
            self.feed_comments(tokens, comments)?;
        }
        if let Some(write) = write {
            let mut lowerer = self.lowerer();
            let graph = lowerer.run(&ast)?;
            #[cfg(feature = "debug")]
            {
                // Export DOT file if environment variable is set
                if let Ok(output) = std::env::var("DO_EXPORT_DOT")
                    && let Err(e) = self.export_cfg_dot(&graph, output)
                {
                    eprintln!("dot export failed: {e}");
                }
            }
            let mut emitter = self.emitter(&graph);
            Ok(emitter.emit(write)?)
        } else {
            Ok(())
        }
    }

    /// Analyze source code, generating diagnostics and semantic tokens.
    /// Ignores errors to the greatest extent possible, other than a request
    /// by either emitter to stop analysis.  The two emitters must share a common
    /// `Break` type.
    ///
    /// The precise order of emitted tokens is not specified, but will be approximately in
    /// textual order.  The precise interleaving of diagnostics and tokens is not specified.
    ///
    /// # Arguments
    /// - `diags`: Where to send generated diagnostics.
    /// - `tokens`: Where to send semantic tokens.
    ///
    /// # Errors
    /// - [`ErrorKind::Fail`]: Compilation failed due to at least one fatal error.  This should
    ///   nearly never occur unless the source code is too severely malformed to process.
    /// - [`ErrorKind::Break`]: Compilation was stopped by the diagnostic emitter.
    pub fn analyze<D: EmitDiag, T: EmitToken<Break = D::Break>>(
        self,
        diags: &mut D,
        tokens: &mut T,
    ) -> Result<(), Error<D::Break>> {
        self.run(diags, Some(tokens), None, true)
    }

    /// Compile the source
    ///
    /// # Arguments
    /// - `write`: Where to write bytecode.
    /// - `diags`: Where to send generated diagnostics.
    ///
    /// # Errors
    /// - [`ErrorKind::Fail`]: Compilation failed due to at least one fatal error.
    /// - [`ErrorKind::Break`]: Compilation was stopped by the diagnostic emitter.
    /// - [`ErrorKind::Io`]: Writing bytecode failed with an [`io::Error`].
    pub fn compile<E: EmitDiag>(
        self,
        write: &mut impl Write,
        diags: &mut E,
    ) -> Result<(), Error<E::Break>> {
        self.run(diags, None, Some(write), false)
    }

    fn drain_diags<E: EmitDiag>(
        &self,
        diags: &mut Diags,
        emit: &mut E,
    ) -> Result<(), Error<E::Break>> {
        for diag in diags.drain() {
            if let ControlFlow::Break(b) = emit.emit(diag.resolve(self)) {
                return Err(Error(ErrorInfo::Break(b)));
            }
        }
        Ok(())
    }

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
            &mut self.origintab,
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
            origintab: &self.origintab,
            prelude: &self.prelude,
            sentinel_const: None,
        }
    }

    fn emitter<'b>(&'a self, graph: &'b cfg::Graph) -> Emitter<'b>
    where
        'a: 'b,
    {
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
    fn ast_to_dot<N: Node + ?Sized>(&self, ast: &N, writer: &mut impl Write) -> io::Result<()> {
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

    /// Export AST to a DOT file based on the DO_EXPORT_DOT environment variable
    /// Similar to the CFG export functionality
    fn export_ast_dot<N: Node + ?Sized>(&self, ast: &N, res: bool) -> io::Result<()> {
        use std::{
            fs,
            path::{self, Component},
        };

        if let Ok(output) = std::env::var("DO_EXPORT_DOT") {
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
            eprintln!("AST DOT exported to: {}", out.display());
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
