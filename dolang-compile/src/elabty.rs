//! Resolve the names within types, and warn about types that cannot mean what they say.
//!
//! This runs only when documenting, after elaboration. It must not change anything
//! lowering reads, so it records resolutions on type names alone and marks bindings
//! only as named by a type. Scopes are walked as the document index walks them, so a
//! resolution's depth means the same to both.

use std::{
    cell::Cell,
    fmt::{self, Write},
};

use dolang_util::intern::BinTable;

use crate::{
    Compiler,
    ast::{
        Annot, Arg, ArrayElem, Binders, Block, Class, ClassMember, DictElem, Expr, ExprBody,
        FieldInit, For, Function, Ident, If, ImportElement, LValue, Origin, Param, PatIdent,
        Pattern, PrimStmt, Res, Root, Stmt, TypeExpr, Var,
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

struct Frame<'s> {
    outer: Option<&'s Frame<'s>>,
    kind: FrameKind<'s>,
}

enum FrameKind<'s> {
    /// A lexical scope, holding the variables elaboration left in it
    Vars {
        vars: &'s [Cell<Var>],
        decls: Vec<Option<Decl>>,
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
    /// A type-only import rather than a binder
    import: bool,
    used: Cell<bool>,
}

enum Found {
    Var { res: Res, import: bool },
    Type { import: bool },
}

impl<'s> Frame<'s> {
    fn vars<T: Element>(outer: Option<&'s Frame<'s>>, vars: &'s mut [Var], elems: &[T]) -> Self {
        let mut decls = vec![None; vars.len()];
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
                import: false,
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

fn declare(decls: &mut [Option<Decl>], ident: &Ident, decl: Decl) {
    if let Some(Res {
        index, depth: 0, ..
    }) = ident.res
        && let Some(slot) = decls.get_mut(index)
    {
        *slot = Some(decl);
    }
}

impl Check<'_> {
    fn sym_name(&self, sym: sym::Id) -> Option<&str> {
        self.symtab
            .get_by_index(sym.index())
            .map(|id| &self.bintab[*id])
    }

    /// Find what `name`, written at `site`, refers to.
    fn lookup(&self, frame: &Frame<'_>, name: &str, site: u32) -> Option<Found> {
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
                        let decl = decls.get(index).copied().flatten();
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
                    if let Some(found) = names
                        .iter()
                        .rev()
                        .find(|found| self.file.str(found.span) == name)
                    {
                        found.used.set(true);
                        return Some(Found::Type {
                            import: found.import,
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
        ty.each_name(&mut |head, dotted| self.name(frame, head, dotted));
    }

    fn name(&self, frame: &Frame<'_>, head: &mut Ident, dotted: bool) {
        let name = self.file.str(head.span);
        match self.lookup(frame, name, head.span.start) {
            None => self.diags.push(UnboundType(head.span)),
            Some(Found::Var { res, import }) => {
                head.res = Some(res);
                if dotted && !import {
                    self.diags.push(DottedNonImport(head.span));
                }
            }
            Some(Found::Type { import }) => {
                if dotted && !import {
                    self.diags.push(DottedNonImport(head.span));
                }
            }
        }
    }

    fn unused_types(&self, frame: &Frame<'_>) {
        if let FrameKind::Types { names } = &frame.kind {
            for name in names {
                if name.used.get() || self.file.str(name.span).starts_with('_') {
                    continue;
                }
                if name.import {
                    self.diags.push(UnusedTypeImport(name.span));
                } else {
                    self.diags.push(UnusedBinder(name.span));
                }
            }
        }
    }

    fn function(&mut self, outer: Option<&Frame<'_>>, func: &mut Function) {
        let Function { params, ret, body } = func;
        let frame = Frame::vars(outer, &mut body.vars, &body.stmts);
        for param in params.iter_mut() {
            self.param(&frame, param);
        }
        if let Some(ret) = ret {
            self.ty(&frame, &mut ret.ty);
        }
        let inner = frame.body(&body.stmts);
        for stmt in body.stmts.iter_mut() {
            self.stmt(&inner, stmt);
        }
        self.unused_types(&inner);
    }

    /// Check a function declared with binders.
    fn def(&mut self, frame: &Frame<'_>, binders: Option<&Binders>, func: &mut Function) {
        let inner = Frame::binders(frame, binders);
        self.function(Some(&inner), func);
        self.unused_types(&inner);
    }

    fn param(&mut self, frame: &Frame<'_>, param: &mut Param) {
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
            Pattern::Ident(PatIdent { ty, .. }) => self.annot(frame, ty),
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
            Stmt::Import(_) | Stmt::Break(..) | Stmt::Continue(..) => {}
            Stmt::Def(def) => {
                for decorator in &mut def.decorators {
                    self.expr(frame, &mut decorator.expr);
                }
                self.def(frame, def.binders.as_deref(), &mut def.func);
            }
            Stmt::Class(class) => self.class(frame, class),
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
        for super_ref in &mut class.super_refs {
            for arg in &mut super_ref.args {
                self.ty(&inner, arg.ty_mut());
            }
        }
        for member in &mut class.body.members {
            match member {
                ClassMember::Method(method) => {
                    for decorator in &mut method.decorators {
                        self.expr(&inner, &mut decorator.expr);
                    }
                    self.def(&inner, method.binders.as_deref(), &mut method.func);
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
    fn declare(&self, _decls: &mut [Option<Decl>]) {}

    /// Collect the type-only imports the element declares in its enclosing block.
    fn type_imports(&self, _names: &mut Vec<TypeName>) {}
}

impl Element for Stmt {
    fn check(&mut self, check: &mut Check<'_>, frame: &Frame<'_>) {
        check.stmt(frame, self);
    }

    fn declare(&self, decls: &mut [Option<Decl>]) {
        match self {
            Stmt::NlGuard(guard) => guard.body.declare(decls),
            Stmt::Def(def) => declare(decls, &def.ident, Decl::Hoisted),
            Stmt::Class(class) => declare(decls, &class.ident, Decl::Hoisted),
            Stmt::Import(import) => {
                for element in &import.elements {
                    match element {
                        ImportElement::ModuleAsIs { bind, .. }
                        | ImportElement::ModuleRenamed { bind, .. } => {
                            declare(decls, bind, Decl::Import)
                        }
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
                    let ImportElement::Items { items, .. } = element else {
                        continue;
                    };
                    for item in items.iter().filter(|item| item.is_type_only()) {
                        names.push(TypeName {
                            span: item.bind().span,
                            import: true,
                            // An exported name may be used elsewhere
                            used: Cell::new(import.pub_span.is_some()),
                        });
                    }
                }
            }
            _ => {}
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
