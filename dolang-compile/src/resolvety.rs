//! Resolve the names within types, and warn about types that cannot mean what they say.
//!
//! This runs only when documenting, after elaboration. It must not change anything
//! lowering reads, so it records resolutions on type names alone and marks bindings
//! only as named by a type. Scopes are walked as the document index walks them, so a
//! resolution's depth means the same to both.

use std::{
    cell::Cell,
    collections::HashSet,
    fmt::{self, Write},
    iter,
};

use dolang_util::intern::BinTable;

use crate::{
    Compiler,
    ast::{
        AliasBody, Annot, Arg, ArrayElem, Binders, Block, Class, ClassMember, Def, DictElem, Expr,
        ExprBody, FieldInit, For, Function, Ident, If, ImportElement, LValue, Origin, Param,
        PatIdent, Pattern, PrimStmt, Res, Root, Stmt, TypeArgKind, TypeDecl, TypeExpr, Var,
        visit::Node,
    },
    diag::Severity,
    source::{Diagnose, Diags, File, Span},
    sym,
};

struct UnboundType(Span);

impl Diagnose for UnboundType {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "unbound type name")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct DottedNonImport(Span);

impl Diagnose for DottedNonImport {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "a dotted type name must begin with an import")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct UnusedBinder(Span);

impl Diagnose for UnusedBinder {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "unused binder")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct UnusedTypeImport(Span);

impl Diagnose for UnusedTypeImport {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "unused type import")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct OverloadWithoutImpl(Span);

impl Diagnose for OverloadWithoutImpl {
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "type-only def has no implementation")
    }

    fn span(&self) -> Span {
        self.0
    }
}

struct TypeShadowsValue(Span);

impl Diagnose for TypeShadowsValue {
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "type name shadows a value of the same name")
    }
    fn span(&self) -> Span {
        self.0
    }
}

struct AliasShadowsEarly(Span);

impl Diagnose for AliasShadowsEarly {
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(
            w,
            "type name refers to its block's alias, not the outer type it shadows"
        )
    }
    fn span(&self) -> Span {
        self.0
    }
}

struct ValueShadowsType(Span);

impl Diagnose for ValueShadowsType {
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn message(&self, _compiler: &Compiler<'_>, w: &mut dyn Write) -> fmt::Result {
        write!(w, "value name shadows a type of the same name")
    }
    fn span(&self) -> Span {
        self.0
    }
}

pub(crate) fn check(
    root: &mut Root,
    file: &File<'_>,
    symtab: &sym::Table,
    bintab: &BinTable,
    diags: &Diags,
) {
    let mut check = Check {
        file,
        symtab,
        bintab,
        diags,
    };
    check.function(None, &mut root.0);
}

struct Check<'a> {
    file: &'a File<'a>,
    symtab: &'a sym::Table,
    bintab: &'a BinTable,
    diags: &'a Diags,
}

/// How a block's own statements declare one of its variables
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decl {
    /// A `def` or `class`, which is visible throughout the block
    Hoisted,
    /// An import, which is also visible throughout the block
    Import,
}

/// How a block's own statements declare its variables
#[derive(Default)]
struct Decls {
    decls: Vec<Option<Decl>>,
    /// The modules a block's imports bind, by the index of the variable each binds.
    /// Modules are not nested, so `security.unix` and `security.nfs4` bind the one
    /// variable `security` and only their whole paths say what it holds.
    modules: Vec<(usize, ModulePath)>,
}

/// The path of a module bound to a variable, relative to the variable's own name
#[derive(Clone, Copy)]
enum ModulePath {
    /// The variable is the head of a dotted path, which this spans in full
    Dotted(Span),
    /// The variable is the whole path, as for `import math: m`
    Bind,
}

impl Decls {
    fn decl(&self, index: usize) -> Option<Decl> {
        self.decls.get(index).copied().flatten()
    }

    /// Whether an import bound a module to the variable at `index`, which constrains
    /// what a dotted type name reaching that variable may say.
    fn imports_module(&self, index: usize) -> bool {
        self.modules.iter().any(|(bound, _)| *bound == index)
    }

    /// Whether `path` names a type in a module bound to the variable at `index`.
    fn names_module(&self, file: &File<'_>, index: usize, path: &[&str]) -> bool {
        self.modules
            .iter()
            .filter(|(bound, _)| *bound == index)
            .any(|(_, module)| match module {
                ModulePath::Dotted(span) => names_type_in(file.str(*span), path),
                ModulePath::Bind => path.len() == 2,
            })
    }
}

/// Whether the dotted module path `module` is what holds the type `path` names. Modules
/// are not nested, so the path must be the module's own components and then one name.
fn names_type_in(module: &str, path: &[&str]) -> bool {
    let components = module.split('.');
    components.clone().count() + 1 == path.len()
        && components.eq(path[..path.len() - 1].iter().copied())
}

struct Frame<'s> {
    outer: Option<&'s Frame<'s>>,
    kind: FrameKind<'s>,
}

enum FrameKind<'s> {
    /// A lexical scope, holding the variables elaboration left in it
    Vars {
        vars: &'s [Cell<Var>],
        decls: Decls,
        /// Whether names are being resolved within the block's statements, where its
        /// declarations are visible before they appear
        in_body: Cell<bool>,
    },
    /// Names that exist only in types: the binders of a declaration, or the type-only
    /// imports of a block
    Types { names: Vec<TypeName> },
}

/// A name declared for types alone
struct TypeName {
    span: Span,
    /// The whole dotted path of a type-only module import, of which `span` is only the
    /// head. Modules are not nested, so `security.unix` and `security.nfs4` are separate
    /// imports that happen to share the head they bind, and only the whole path says
    /// which module a dotted type name comes from. For a renamed import, this is
    /// the local name instead.
    module: Option<Span>,
    /// A type-only import rather than a binder
    import: bool,
    /// The declaration of an alias, which is visible throughout its block but shadows
    /// an outer name even before it appears
    alias: Option<Span>,
    /// Whether an unused name should be diagnosed.
    warn_unused: bool,
    used: Cell<bool>,
}

impl TypeName {
    /// The name this binds, which for a dotted module import is only its head.
    fn bound_name<'a>(&self, file: &'a File<'_>) -> &'a str {
        file.str(self.span)
    }

    /// Whether this is what `path` names. A module import holds the type the path ends
    /// with; every other declaration names a type itself, so it matches its own name.
    fn matches(&self, file: &File<'_>, path: &[&str]) -> bool {
        match self.module {
            Some(module) => names_type_in(file.str(module), path),
            None => path[0] == self.bound_name(file),
        }
    }

    /// Where an unused name is reported, which for a module import is its whole path.
    fn report_span(&self) -> Span {
        self.module.unwrap_or(self.span)
    }
}

enum Found {
    Var { res: Res, import: bool },
    Type { import: bool, span: Span },
}

impl<'s> Frame<'s> {
    fn vars<T: Element>(outer: Option<&'s Frame<'s>>, vars: &'s mut [Var], elems: &[T]) -> Self {
        let mut decls = Decls {
            decls: vec![None; vars.len()],
            modules: Vec::new(),
        };
        for elem in elems {
            elem.declare(&mut decls);
        }
        Frame {
            outer,
            kind: FrameKind::Vars {
                vars: Cell::from_mut(vars).as_slice_of_cells(),
                decls,
                in_body: Cell::new(false),
            },
        }
    }

    fn binders(outer: &'s Frame<'s>, binders: Option<&Binders>) -> Self {
        let names = binders
            .into_iter()
            .flat_map(|binders| &binders.binders)
            .map(|binder| TypeName {
                span: binder.ident.span,
                module: None,
                import: false,
                alias: None,
                warn_unused: true,
                used: Cell::new(false),
            })
            .collect();
        Frame {
            outer: Some(outer),
            kind: FrameKind::Types { names },
        }
    }

    /// Enter the statements of the block whose variables this frame holds, where the
    /// block's declarations are visible before they appear. The returned frame holds the
    /// block's type-only imports, which are found before its variables.
    fn body<T: Element>(&'s self, elems: &[T]) -> Frame<'s> {
        if let FrameKind::Vars { in_body, .. } = &self.kind {
            in_body.set(true);
        }
        let mut names = Vec::new();
        for elem in elems {
            elem.type_imports(&mut names);
        }
        Frame {
            outer: Some(self),
            kind: FrameKind::Types { names },
        }
    }
}

fn declare(decls: &mut Decls, ident: &Ident, decl: Decl) -> Option<usize> {
    let Some(Res {
        index, depth: 0, ..
    }) = ident.res
    else {
        return None;
    };
    let slot = decls.decls.get_mut(index)?;
    *slot = Some(decl);
    Some(index)
}

/// Record a module import, which binds only the head of the path it names.
fn declare_module(decls: &mut Decls, ident: &Ident, module: ModulePath) {
    if let Some(index) = declare(decls, ident, Decl::Import) {
        decls.modules.push((index, module));
    }
}

impl Check<'_> {
    fn sym_name(&self, sym: sym::Id) -> Option<&str> {
        self.symtab
            .get_by_index(sym.index())
            .map(|id| &self.bintab[*id])
    }

    fn has_value(&self, frame: &Frame<'_>, name: &str, site: u32) -> bool {
        let mut frame = Some(frame);
        while let Some(current) = frame {
            if let FrameKind::Vars {
                vars,
                decls,
                in_body,
            } = &current.kind
                && vars.iter().enumerate().rev().any(|(index, cell)| {
                    let var = cell.get();
                    if self.sym_name(var.sym) != Some(name) {
                        return false;
                    }
                    let decl = decls.decl(index);
                    match var.origin {
                        Origin::Source(span) | Origin::SelfParam(span) => {
                            span.start < site || (decl.is_some() && in_body.get())
                        }
                        Origin::PreludeModule | Origin::PreludeItem { .. } | Origin::Repl => true,
                        Origin::Synthetic => false,
                    }
                })
            {
                return true;
            }
            frame = current.outer;
        }
        false
    }

    /// Whether a type named `name` is visible at `site`. An alias counts only once
    /// declared, so a value is diagnosed where the later of the two appears.
    fn has_type(&self, frame: &Frame<'_>, name: &str, site: u32) -> bool {
        let mut frame = Some(frame);
        while let Some(current) = frame {
            if let FrameKind::Types { names } = &current.kind
                && names.iter().rev().any(|found| {
                    found.bound_name(self.file) == name
                        && found.alias.is_none_or(|alias| site > alias.end)
                })
            {
                return true;
            }
            frame = current.outer;
        }
        false
    }

    fn warn_value_name(&self, frame: &Frame<'_>, ident: &Ident) {
        if self.has_type(frame, self.file.str(ident.span), ident.span.start) {
            self.diags.push(ValueShadowsType(ident.span));
        }
    }

    /// Find what the dotted `path`, written at `site`, refers to.
    fn lookup(&self, frame: &Frame<'_>, path: &[&str], site: u32) -> Option<Found> {
        let name = path[0];
        let mut depth = 0;
        let mut frame = Some(frame);
        while let Some(current) = frame {
            match &current.kind {
                FrameKind::Vars {
                    vars,
                    decls,
                    in_body,
                } => {
                    // A later binding of a name shadows an earlier one
                    for (index, cell) in vars.iter().enumerate().rev() {
                        let var = cell.get();
                        if self.sym_name(var.sym) != Some(name) {
                            continue;
                        }
                        let decl = decls.decl(index);
                        let visible = match var.origin {
                            Origin::Source(span) | Origin::SelfParam(span) => {
                                span.start < site || (decl.is_some() && in_body.get())
                            }
                            Origin::PreludeModule | Origin::PreludeItem { .. } | Origin::Repl => {
                                true
                            }
                            Origin::Synthetic => false,
                        };
                        if !visible {
                            continue;
                        }
                        // An import binds only the head of the module path it names, so
                        // the whole path must be one of the modules actually imported.
                        if decls.imports_module(index)
                            && !decls.names_module(self.file, index, path)
                        {
                            continue;
                        }
                        cell.update(|mut var| {
                            var.type_used = true;
                            var
                        });
                        return Some(Found::Var {
                            res: Res {
                                index,
                                depth,
                                node: None,
                            },
                            import: decl == Some(Decl::Import) || var.is_prelude(),
                        });
                    }
                    depth += 1;
                }
                FrameKind::Types { names } => {
                    // A later binding of a name shadows an earlier one
                    if let Some(found) = names
                        .iter()
                        .rev()
                        .find(|found| found.matches(self.file, path))
                    {
                        found.used.set(true);
                        if let Some(alias) = found.alias
                            && site <= alias.end
                            // Past the block's own variables, whose conflicts with its
                            // aliases are diagnosed at the alias
                            && let Some(outer) = current.outer.and_then(|block| block.outer)
                            && (self.has_type(outer, name, site) || self.has_value(outer, name, site))
                        {
                            self.diags.push(AliasShadowsEarly(Span {
                                start: site,
                                end: site + name.len() as u32,
                            }));
                        }
                        return Some(Found::Type {
                            import: found.import,
                            span: found.span,
                        });
                    }
                }
            }
            frame = current.outer;
        }
        None
    }

    fn annot(&mut self, frame: &Frame<'_>, annot: &mut Option<Box<Annot>>) {
        if let Some(annot) = annot {
            self.ty(frame, &mut annot.ty);
        }
    }

    fn ty(&mut self, frame: &Frame<'_>, ty: &mut TypeExpr) {
        ty.each_name(&mut |head, decl, fields| self.name(frame, head, decl, fields));
    }

    fn name(
        &self,
        frame: &Frame<'_>,
        head: &mut Ident,
        decl: &mut Option<TypeDecl>,
        fields: &[Span],
    ) {
        let path: Vec<_> = iter::once(head.span)
            .chain(fields.iter().copied())
            .map(|span| self.file.str(span))
            .collect();
        let dotted = !fields.is_empty();
        match self.lookup(frame, &path, head.span.start) {
            // What is unbound is everything the last component is looked up in, which for
            // a dotted name is the module it says holds the type.
            None => self.diags.push(UnboundType(Span {
                start: head.span.start,
                end: match fields.split_last() {
                    Some((_, [.., module])) => module.end,
                    _ => head.span.end,
                },
            })),
            Some(Found::Var { res, import }) => {
                head.res = Some(res);
                if dotted && !import {
                    self.diags.push(DottedNonImport(head.span));
                }
            }
            Some(Found::Type { import, span }) => {
                *decl = Some(TypeDecl { span, node: None });
                if dotted && !import {
                    self.diags.push(DottedNonImport(head.span));
                }
            }
        }
    }

    fn unused_types(&self, frame: &Frame<'_>) {
        if let FrameKind::Types { names } = &frame.kind {
            for name in names {
                if !name.warn_unused
                    || name.used.get()
                    || name.bound_name(self.file).starts_with('_')
                {
                    continue;
                }
                if name.import {
                    self.diags.push(UnusedTypeImport(name.report_span()));
                } else {
                    self.diags.push(UnusedBinder(name.report_span()));
                }
            }
        }
    }

    fn function(&mut self, outer: Option<&Frame<'_>>, func: &mut Function) {
        let Function {
            params, ret, body, ..
        } = func;
        let frame = Frame::vars(outer, &mut body.vars, &body.stmts);
        for param in params.iter_mut() {
            self.param(&frame, param);
        }
        if let Some(ret) = ret {
            self.ty(&frame, &mut ret.ty);
        }
        let inner = frame.body(&body.stmts);
        self.overloads(&body.stmts);
        for stmt in body.stmts.iter_mut() {
            self.stmt(&inner, stmt);
        }
        self.unused_types(&inner);
    }

    /// Warn about a type-only def that no def of the same name among its block's
    /// statements implements.
    fn overloads<T: Element>(&self, elems: &[T]) {
        let impls: HashSet<&str> = elems
            .iter()
            .filter_map(Element::def)
            .filter(|def| !def.is_type_only())
            .map(|def| self.file.str(def.ident.span))
            .collect();
        for def in elems.iter().filter_map(Element::def) {
            if def.is_type_only() && !impls.contains(self.file.str(def.ident.span)) {
                self.diags.push(OverloadWithoutImpl(def.ident.span));
            }
        }
    }

    /// Check a function declared with binders.
    fn def(&mut self, frame: &Frame<'_>, binders: Option<&mut Binders>, func: &mut Function) {
        let inner = Frame::binders(frame, binders.as_deref());
        self.binder_types(&inner, binders);
        self.function(Some(&inner), func);
        self.unused_types(&inner);
    }

    fn binder_types(&mut self, frame: &Frame<'_>, binders: Option<&mut Binders>) {
        for binder in binders.into_iter().flat_map(|binders| &mut binders.binders) {
            if let Some(bound) = &mut binder.bound {
                self.ty(frame, &mut bound.ty);
            }
            if let Some(default) = &mut binder.default {
                self.ty(frame, &mut default.ty);
            }
        }
    }

    fn param(&mut self, frame: &Frame<'_>, param: &mut Param) {
        match param {
            Param::Pos { ident, .. } | Param::Key { ident, .. } | Param::ConstKey { ident, .. } => {
                self.warn_value_name(frame, ident)
            }
            Param::Rest {
                ident: Some(ident), ..
            } => self.warn_value_name(frame, ident),
            Param::Rest { ident: None, .. } => {}
        }
        match param {
            Param::Pos { ty, default, .. } | Param::Key { ty, default, .. } => {
                if let Some(default) = default {
                    self.expr(frame, &mut default.expr);
                }
                self.annot(frame, ty);
            }
            Param::ConstKey {
                key_expr,
                ty,
                default,
                ..
            } => {
                self.expr(frame, key_expr);
                if let Some(default) = default {
                    self.expr(frame, &mut default.expr);
                }
                self.annot(frame, ty);
            }
            Param::Rest { ty, .. } => self.annot(frame, ty),
        }
    }

    fn pattern(&mut self, frame: &Frame<'_>, pattern: &mut Pattern) {
        match pattern {
            Pattern::Ident(PatIdent { ident, ty }) => {
                self.warn_value_name(frame, ident);
                self.annot(frame, ty);
            }
            Pattern::Unpack(params) => {
                for param in params {
                    self.param(frame, param);
                }
            }
        }
    }

    fn stmt(&mut self, frame: &Frame<'_>, stmt: &mut Stmt) {
        match stmt {
            Stmt::NlGuard(guard) => self.stmt(frame, &mut guard.body),
            Stmt::Prim(prim) => self.prim(frame, prim),
            Stmt::Let(node) => {
                self.prim(frame, &mut node.rhs);
                self.pattern(frame, &mut node.bind);
            }
            Stmt::Bind(node) => {
                self.expr(frame, &mut node.expr);
                self.pattern(frame, &mut node.bind);
            }
            Stmt::Assign(node) => {
                self.lvalue(frame, &mut node.lhs);
                self.prim(frame, &mut node.rhs);
            }
            Stmt::Import(import) => {
                for element in &import.elements {
                    match element {
                        ImportElement::ModuleAsIs {
                            bind,
                            type_only: Some(_),
                            ..
                        }
                        | ImportElement::ModuleRenamed {
                            bind,
                            type_only: Some(_),
                            ..
                        } => {
                            if self.has_value(frame, self.file.str(bind.span), bind.span.start) {
                                self.diags.push(TypeShadowsValue(bind.span));
                            }
                        }
                        ImportElement::Items { items, .. } => {
                            for item in items.iter().filter(|item| item.is_type_only()) {
                                let bind = item.bind();
                                if self.has_value(frame, self.file.str(bind.span), bind.span.start)
                                {
                                    self.diags.push(TypeShadowsValue(bind.span));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Stmt::Break(..) | Stmt::Continue(..) => {}
            Stmt::TypeAlias(alias) => {
                if self.has_value(
                    frame,
                    self.file.str(alias.ident.span),
                    alias.ident.span.start,
                ) {
                    self.diags.push(TypeShadowsValue(alias.ident.span));
                }
                let inner = Frame::binders(frame, alias.binders.as_deref());
                self.binder_types(&inner, alias.binders.as_deref_mut());
                // An opaque alias has no body to use its binders in
                if let AliasBody::Type(ty) = &mut alias.body {
                    self.ty(&inner, ty);
                    self.unused_types(&inner);
                }
            }
            Stmt::Def(def) => {
                // An overload names the value its implementation binds
                if !def.is_type_only() {
                    self.warn_value_name(frame, &def.ident);
                }
                for decorator in &mut def.decorators {
                    self.expr(frame, &mut decorator.expr);
                }
                self.def(frame, def.binders.as_deref_mut(), &mut def.func);
            }
            Stmt::Class(class) => {
                if class.is_protocol() {
                    // A protocol is visible throughout its block, so a value of the
                    // block is diagnosed where the value is bound
                    let mut enclosing = Some(frame);
                    while let Some(current) = enclosing {
                        enclosing = current.outer;
                        if matches!(current.kind, FrameKind::Vars { .. }) {
                            break;
                        }
                    }
                    let name = self.file.str(class.ident.span);
                    if let Some(enclosing) = enclosing
                        && self.has_value(enclosing, name, class.ident.span.start)
                    {
                        self.diags.push(TypeShadowsValue(class.ident.span));
                    }
                } else {
                    self.warn_value_name(frame, &class.ident);
                }
                self.class(frame, class)
            }
            Stmt::Return(ret) => {
                if let Some(expr) = &mut ret.expr {
                    self.expr(frame, expr);
                }
            }
            Stmt::Throw(node) => self.expr(frame, &mut node.expr),
            Stmt::While(node) => {
                self.expr(frame, &mut node.expr);
                self.branch(
                    frame,
                    &mut node.body,
                    node.bind.as_mut().map(|bind| &mut bind.pattern),
                );
            }
            Stmt::For(node) => {
                if let Some(expr) = &mut node.expr {
                    self.expr(frame, expr);
                }
                self.branch(frame, &mut node.body, Some(&mut node.bind));
            }
        }
    }

    fn class(&mut self, frame: &Frame<'_>, class: &mut Class) {
        for decorator in &mut class.decorators {
            self.expr(frame, &mut decorator.expr);
        }
        let inner = Frame::binders(frame, class.binders.as_deref());
        self.binder_types(&inner, class.binders.as_deref_mut());
        for super_ref in &mut class.super_refs {
            // Elaboration resolves the head of a supertype that exists at runtime
            if super_ref.type_only {
                self.name(
                    &inner,
                    &mut super_ref.ident,
                    &mut super_ref.decl,
                    &super_ref.fields,
                );
            }
            for arg in &mut super_ref.args {
                match &mut arg.kind {
                    TypeArgKind::KeyRest { key_ty, ty, .. } => {
                        self.ty(&inner, key_ty);
                        self.ty(&inner, ty);
                    }
                    _ => {
                        if let Some(ty) = arg.ty_mut() {
                            self.ty(&inner, ty);
                        }
                    }
                }
            }
        }
        let impls: HashSet<(bool, &str)> = class
            .body
            .members
            .iter()
            .filter_map(|member| match member {
                ClassMember::Method(method) if !method.type_only => {
                    Some((method.special.is_some(), self.file.str(method.name_span)))
                }
                _ => None,
            })
            .collect();
        for member in &mut class.body.members {
            match member {
                ClassMember::Method(method) => {
                    // Protocol members have no implementation to find
                    if method.at_span.is_some()
                        && !impls
                            .contains(&(method.special.is_some(), self.file.str(method.name_span)))
                    {
                        self.diags.push(OverloadWithoutImpl(method.name_span));
                    }
                    for decorator in &mut method.decorators {
                        self.expr(&inner, &mut decorator.expr);
                    }
                    self.def(&inner, method.binders.as_deref_mut(), &mut method.func);
                }
                ClassMember::Field(field) => {
                    for decorator in &mut field.decorators {
                        self.expr(&inner, &mut decorator.expr);
                    }
                    match &mut field.init {
                        FieldInit::None => {}
                        FieldInit::Expr(expr) | FieldInit::Const(expr, _) => {
                            self.expr(&inner, expr)
                        }
                        FieldInit::Thunk(func) => self.function(Some(&inner), func),
                    }
                    self.annot(&inner, &mut field.ty);
                }
            }
        }
        self.unused_types(&inner);
    }

    fn prim(&mut self, frame: &Frame<'_>, prim: &mut PrimStmt) {
        match prim {
            PrimStmt::Expr(expr) => self.expr(frame, expr),
            PrimStmt::If(node) => self.if_body(frame, node),
            PrimStmt::Try(node) => {
                self.function(Some(frame), &mut node.body);
                for handler in &mut node.handlers {
                    if let Some(expr) = &mut handler.class_expr {
                        self.expr(frame, expr);
                    }
                    self.function(Some(frame), &mut handler.func);
                }
                if let Some((func, _)) = &mut node.finally {
                    self.function(Some(frame), func);
                }
            }
        }
    }

    fn branch<T: Body>(&mut self, frame: &Frame<'_>, body: &mut T, pattern: Option<&mut Pattern>) {
        let (vars, elems) = body.parts();
        let inner = Frame::vars(Some(frame), vars, elems);
        if let Some(pattern) = pattern {
            self.pattern(&inner, pattern);
        }
        let body = inner.body(elems);
        self.overloads(elems);
        for elem in elems.iter_mut() {
            elem.check(self, &body);
        }
        self.unused_types(&body);
    }

    fn if_body<T: Body>(&mut self, frame: &Frame<'_>, node: &mut If<T>) {
        for branch in
            std::iter::once(&mut node.tbranch).chain(node.elif_branches.iter_mut().map(|(b, _)| b))
        {
            self.expr(frame, &mut branch.expr);
            self.branch(
                frame,
                &mut branch.body,
                branch.bind.as_mut().map(|b| &mut b.pattern),
            );
        }
        if let Some((body, _)) = &mut node.else_branch {
            self.branch(frame, body, None);
        }
    }

    fn for_elem<T: Element>(&mut self, frame: &Frame<'_>, node: &mut For<ExprBody<T>>) {
        if let Some(expr) = &mut node.expr {
            self.expr(frame, expr);
        }
        self.branch(frame, &mut node.body, Some(&mut node.bind));
    }

    fn lvalue(&mut self, frame: &Frame<'_>, value: &mut LValue) {
        match value {
            LValue::Ident(_) => {}
            LValue::Field { object, .. } | LValue::PrivateField { object, .. } => {
                self.expr(frame, object)
            }
            LValue::Index { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.expr(frame, expr);
                }
            }
        }
    }

    fn expr(&mut self, frame: &Frame<'_>, expr: &mut Expr) {
        match expr {
            Expr::Group { expr, .. } | Expr::Unary { expr, .. } => self.expr(frame, expr),
            Expr::Binary { exprs, .. } | Expr::Index { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.expr(frame, expr);
                }
            }
            Expr::Range { exprs, .. } => {
                for expr in exprs.iter_mut().flatten() {
                    self.expr(frame, expr);
                }
            }
            Expr::Lambda { func, .. } => self.function(Some(frame), func),
            Expr::Call { arg0, args, .. } => {
                self.expr(frame, arg0);
                for arg in args {
                    arg.check(self, frame);
                }
            }
            Expr::Get { object, .. } => self.expr(frame, object),
            Expr::Array { elems, .. } => {
                for elem in elems {
                    elem.check(self, frame);
                }
            }
            Expr::Dict { elems, .. } => {
                for elem in elems {
                    elem.check(self, frame);
                }
            }
            Expr::Concat { exprs, .. }
            | Expr::FmtSeq { exprs, .. }
            | Expr::BinConcat { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.expr(frame, expr);
                }
            }
            Expr::Fmt { value, spec, .. } => {
                self.expr(frame, value);
                for expr in [&mut spec.width, &mut spec.precision].into_iter().flatten() {
                    self.expr(frame, expr);
                }
            }
            Expr::FmtParam { spec, .. } => {
                for expr in [&mut spec.width, &mut spec.precision].into_iter().flatten() {
                    self.expr(frame, expr);
                }
            }
            Expr::Ident(_)
            | Expr::Escape(..)
            | Expr::EscapeByte(..)
            | Expr::Literal(_)
            | Expr::Int(..)
            | Expr::VerbatimInt(..)
            | Expr::F64(..)
            | Expr::VerbatimF64(..)
            | Expr::Bool(..)
            | Expr::Nil(_)
            | Expr::Sym(_)
            | Expr::Error => {}
        }
    }
}

/// The body of a construct that opens a scope, whether statements or the elements of a
/// literal
trait Body {
    type Element: Element;
    fn parts(&mut self) -> (&mut [Var], &mut [Self::Element]);
}

impl Body for Block {
    type Element = Stmt;
    fn parts(&mut self) -> (&mut [Var], &mut [Stmt]) {
        (&mut self.vars, &mut self.stmts)
    }
}

impl<T: Element> Body for ExprBody<T> {
    type Element = T;
    fn parts(&mut self) -> (&mut [Var], &mut [T]) {
        (&mut self.vars, &mut self.elems)
    }
}

trait Element {
    fn check(&mut self, check: &mut Check<'_>, frame: &Frame<'_>);

    /// Record how the element declares variables of its enclosing block.
    fn declare(&self, _decls: &mut Decls) {}

    /// Collect the type-only imports the element declares in its enclosing block.
    fn type_imports(&self, _names: &mut Vec<TypeName>) {}

    /// The `def` the element is, if it is one.
    fn def(&self) -> Option<&Def> {
        None
    }
}

impl Element for Stmt {
    fn check(&mut self, check: &mut Check<'_>, frame: &Frame<'_>) {
        check.stmt(frame, self);
    }

    fn declare(&self, decls: &mut Decls) {
        match self {
            Stmt::NlGuard(guard) => guard.body.declare(decls),
            Stmt::Def(def) => {
                declare(decls, &def.ident, Decl::Hoisted);
            }
            Stmt::Class(class) => {
                declare(decls, &class.ident, Decl::Hoisted);
            }
            Stmt::Import(import) => {
                for element in &import.elements {
                    match element {
                        ImportElement::ModuleAsIs {
                            module,
                            bind,
                            type_only: None,
                            ..
                        } => declare_module(decls, bind, ModulePath::Dotted(*module)),
                        ImportElement::ModuleRenamed {
                            bind,
                            type_only: None,
                            ..
                        } => declare_module(decls, bind, ModulePath::Bind),
                        ImportElement::ModuleAsIs {
                            type_only: Some(_), ..
                        }
                        | ImportElement::ModuleRenamed {
                            type_only: Some(_), ..
                        } => {}
                        ImportElement::Items { items, .. } => {
                            for item in items {
                                declare(decls, item.bind(), Decl::Import);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn type_imports(&self, names: &mut Vec<TypeName>) {
        match self {
            Stmt::NlGuard(guard) => guard.body.type_imports(names),
            Stmt::Import(import) => {
                for element in &import.elements {
                    match element {
                        ImportElement::ModuleAsIs {
                            module,
                            bind,
                            type_only: Some(_),
                            ..
                        } => names.push(TypeName {
                            span: bind.span,
                            module: Some(*module),
                            import: true,
                            alias: None,
                            warn_unused: true,
                            used: Cell::new(import.pub_span.is_some()),
                        }),
                        ImportElement::ModuleRenamed {
                            bind,
                            type_only: Some(_),
                            ..
                        } => names.push(TypeName {
                            span: bind.span,
                            module: Some(bind.span),
                            import: true,
                            alias: None,
                            warn_unused: true,
                            used: Cell::new(import.pub_span.is_some()),
                        }),
                        ImportElement::Items { items, .. } => {
                            for item in items.iter().filter(|item| item.is_type_only()) {
                                names.push(TypeName {
                                    span: item.bind().span,
                                    module: None,
                                    import: true,
                                    alias: None,
                                    warn_unused: true,
                                    // An exported name may be used elsewhere
                                    used: Cell::new(import.pub_span.is_some()),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Stmt::TypeAlias(alias) => names.push(TypeName {
                span: alias.ident.span,
                module: None,
                import: false,
                alias: Some(alias.span()),
                warn_unused: false,
                used: Cell::new(false),
            }),
            // A protocol, like a class, is visible throughout its block
            Stmt::Class(class) if class.is_protocol() => names.push(TypeName {
                span: class.ident.span,
                module: None,
                import: false,
                alias: None,
                warn_unused: false,
                used: Cell::new(false),
            }),
            _ => {}
        }
    }

    fn def(&self) -> Option<&Def> {
        match self {
            Stmt::Def(def) => Some(def),
            _ => None,
        }
    }
}

impl Element for Arg {
    fn check(&mut self, check: &mut Check<'_>, frame: &Frame<'_>) {
        match self {
            Self::Pos(node) => check.expr(frame, &mut node.expr),
            Self::Key(node) => check.expr(frame, &mut node.expr),
            Self::Expand(node) => check.expr(frame, &mut node.expr),
            Self::DynamicKey(node) => {
                check.expr(frame, &mut node.key);
                check.expr(frame, &mut node.value);
            }
            Self::For(node) => check.for_elem(frame, node),
            Self::If(node) => check.if_body(frame, node),
        }
    }
}

impl Element for ArrayElem {
    fn check(&mut self, check: &mut Check<'_>, frame: &Frame<'_>) {
        match self {
            Self::Single(node) => check.expr(frame, &mut node.expr),
            Self::Expand(node) => check.expr(frame, &mut node.expr),
            Self::For(node) => check.for_elem(frame, node),
            Self::If(node) => check.if_body(frame, node),
        }
    }
}

impl Element for DictElem {
    fn check(&mut self, check: &mut Check<'_>, frame: &Frame<'_>) {
        match self {
            Self::Single(node) => check.expr(frame, &mut node.expr),
            Self::Key(node) => check.expr(frame, &mut node.expr),
            Self::Pair(node) => {
                check.expr(frame, &mut node.key);
                check.expr(frame, &mut node.value);
            }
            Self::Expand(node) => check.expr(frame, &mut node.expr),
            Self::For(node) => check.for_elem(frame, node),
            Self::If(node) => check.if_body(frame, node),
        }
    }
}
