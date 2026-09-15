//! Annotate the elaborated tree without changing its semantic resolutions.

use std::{cell::Cell, collections::HashMap};

use dolang_util::alias;

use super::{Id, Kind, Node, Table, comment::Blocks};
use crate::{
    PreludeImport,
    ast::{visit::Node as AstNode, *},
    doc,
    source::{File, Span},
};

pub(crate) fn index(
    root: &mut Root,
    prelude: &mut [PreludeImport],
    file: &File<'_>,
    comments: &[Span],
) -> Table {
    let blocks = Blocks::new(file, comments);
    let mut index = Index {
        file,
        blocks,
        table: Table::new(),
        type_decls: HashMap::new(),
    };
    let root_id = index.table.push(Node::new(
        None,
        Kind::Root,
        Some(Span {
            start: 0,
            end: u32::try_from(file.content().len()).expect("source file is too large"),
        }),
        index.blocks.root(),
    ));
    let scope = Scope {
        outer: None,
        vars: Some(Cell::from_mut(root.0.body.vars.as_mut_slice()).as_slice_of_cells()),
        parent: Some(root_id),
        loop_target: None,
        return_target: None,
    };
    for import in prelude {
        match import {
            PreludeImport::Items { module, items } => {
                for item in items {
                    index.prelude(
                        &scope,
                        &mut item.res,
                        Kind::PreludeItem {
                            module: module.as_str().into(),
                            item: item.item.as_str().into(),
                            name: item.bind.as_str().into(),
                        },
                    );
                }
            }
            PreludeImport::ModuleAsIs {
                module, bind, res, ..
            }
            | PreludeImport::ModuleRenamed {
                module, bind, res, ..
            } => {
                index.prelude(
                    &scope,
                    res,
                    Kind::PreludeModule {
                        module: module.as_str().into(),
                        name: bind.as_str().into(),
                    },
                );
            }
        }
    }
    index.block(&scope, &mut root.0.body.stmts);
    index.table
}

struct Index<'a> {
    file: &'a File<'a>,
    blocks: Blocks,
    table: Table,
    /// The nodes of binders and type-only imports, by where their names start. They
    /// have no variables, so the types naming them identify them this way.
    type_decls: HashMap<u32, Id>,
}

struct Scope<'s> {
    // Frames live on the recursive call stack. Cells borrow the AST's existing
    // variable storage, so annotating an outer binding needs no parallel table.
    outer: Option<&'s Scope<'s>>,
    // Class frames affect document parentage but do not count toward Res::depth.
    vars: Option<&'s [Cell<Var>]>,
    parent: Option<Id>,
    loop_target: Option<Id>,
    return_target: Option<Id>,
}

impl Scope<'_> {
    fn nested<'s>(&'s self, vars: &'s mut [Var], parent: Option<Id>) -> Scope<'s> {
        Scope {
            outer: Some(self),
            vars: Some(Cell::from_mut(vars).as_slice_of_cells()),
            parent: parent.or(self.parent),
            loop_target: self.loop_target,
            return_target: self.return_target,
        }
    }

    fn binding(&self, res: Res) -> Option<&Cell<Var>> {
        match self.vars {
            Some(vars) if res.depth == 0 => vars.get(res.index),
            Some(_) => self.outer?.binding(Res {
                depth: res.depth - 1,
                ..res
            }),
            None => self.outer?.binding(res),
        }
    }

    fn node(&self, res: Res) -> Option<Id> {
        self.binding(res)?.get().node
    }

    fn bind(&self, res: Res, node: Id) {
        if let Some(var) = self.binding(res) {
            var.update(|mut var| {
                var.node = Some(node);
                var
            });
        }
    }
}

impl Index<'_> {
    fn param_kind(param: &Param) -> (Kind, Span) {
        let (kind, key_span, ident, ty, default) = match param {
            Param::Pos { ident, ty, default } => (
                Kind::PositionalParam {
                    name: ident.span,
                    default: default.as_ref().map(|default| default.expr.span()),
                },
                None,
                Some(ident.span),
                ty,
                default,
            ),
            Param::Key {
                key_span,
                colon_span,
                ident,
                ty,
                default,
            } => (
                Kind::KeyParam {
                    key: *key_span,
                    name: ident.span,
                    default: default.as_ref().map(|default| default.expr.span()),
                },
                // The `:` is part of how the parameter is written, and in the
                // `:name` form it is where the parameter starts.
                Some(*key_span | *colon_span),
                Some(ident.span),
                ty,
                default,
            ),
            Param::ConstKey {
                key_expr,
                ident,
                ty,
                default,
                ..
            } => {
                let key = key_expr.span();
                (
                    Kind::KeyParam {
                        key,
                        name: ident.span,
                        default: default.as_ref().map(|default| default.expr.span()),
                    },
                    Some(key),
                    Some(ident.span),
                    ty,
                    default,
                )
            }
            Param::Rest {
                ellipsis_span,
                ident,
                ty,
            } => (
                Kind::RestParam {
                    name: ident.as_ref().map(|ident| ident.span),
                },
                Some(*ellipsis_span),
                ident.as_ref().map(|ident| ident.span),
                ty,
                &None,
            ),
        };
        let ty_span = ty.as_ref().map(|ty| ty.span());
        let default_span = default.as_ref().map(|default| default.expr.span());
        let span = [key_span, ident, ty_span, default_span]
            .into_iter()
            .flatten()
            .reduce(|acc, span| acc | span)
            .unwrap_or(Span::INVALID);
        (kind, span)
    }

    fn push(&mut self, scope: &Scope<'_>, kind: Kind, span: Span) -> Id {
        self.push_to(scope.parent, kind, span)
    }

    fn push_to(&mut self, parent: Option<Id>, kind: Kind, span: Span) -> Id {
        // Apart from the root, only a declaration can be documented, and the
        // block sits above the construct as a whole — decorators included,
        // since they are written between the comment and the keyword.
        let doc = kind
            .definition()
            .and_then(|_| self.blocks.attached(self.file, span.start));
        self.table.push(Node::new(parent, kind, Some(span), doc))
    }

    fn reference(&mut self, scope: &Scope<'_>, ident: &mut Ident) {
        if let Some(res) = &mut ident.res {
            res.node = scope.node(*res);
        }
    }

    fn declaration(&mut self, scope: &Scope<'_>, ident: &mut Ident, kind: Kind, span: Span) -> Id {
        if let Some(id) = ident.res.and_then(|res| res.node) {
            return id;
        }
        let id = self.push(scope, kind, span);
        if let Some(res) = &mut ident.res {
            res.node = Some(id);
            scope.bind(*res, id);
        }
        id
    }

    fn prelude(&mut self, scope: &Scope<'_>, res: &mut Option<Res>, kind: Kind) {
        let Some(res) = res else { return };
        // An import the code never reads is not emitted, but a type may still name it
        if !scope.binding(*res).is_some_and(|var| {
            let var = var.get();
            var.is_emitted() || var.type_used
        }) {
            return;
        }
        if let Some(id) = scope.node(*res) {
            res.node = Some(id);
            return;
        }
        // A prelude binding has no source text to span or document
        let id = self.table.push(Node::new(scope.parent, kind, None, None));
        res.node = Some(id);
        scope.bind(*res, id);
    }

    /// Resolve an annotation, and record it as describing `parent`.
    fn annot(&mut self, scope: &Scope<'_>, parent: Option<Id>, annot: &mut Option<Box<Annot>>) {
        if let Some(annot) = annot {
            self.ty(scope, &mut annot.ty);
            if let Some(parent) = parent {
                self.type_node(parent, &annot.ty);
            }
        }
    }

    /// Resolve the names within a type.
    fn ty(&mut self, scope: &Scope<'_>, ty: &mut TypeExpr) {
        ty.each_name(&mut |head, decl, _| match decl {
            Some(decl) => decl.node = self.type_decls.get(&decl.span.start).copied(),
            None => self.reference(scope, head),
        });
    }

    /// Record a resolved type as describing `parent`, unless it could not be read.
    fn type_node(&mut self, parent: Id, ty: &TypeExpr) {
        if let Some(expr) = self.type_expr(ty) {
            self.push_to(Some(parent), Kind::Type { expr }, ty.span());
        }
    }

    /// Record a resolved superclass as a type describing `class`, unless it could not
    /// be read.
    fn super_node(&mut self, class: Id, super_ref: &ClassSuper) {
        let head = super_ref.ident.span;
        let name = super_ref.fields.last().map_or(head, |field| head | *field);
        let mut expr = doc::TypeExpr {
            span: name,
            kind: doc::TypeKind::Name {
                head,
                target: super_ref.ident.res.and_then(|res| res.node),
            },
        };
        if let Some(bracket_span) = super_ref.bracket_span {
            let Some(args) = self.type_args(&super_ref.args) else {
                return;
            };
            expr = doc::TypeExpr {
                span: name | bracket_span,
                kind: doc::TypeKind::App {
                    base: alias::Box::new(expr),
                    args,
                },
            };
        }
        let span = expr.span;
        self.push_to(Some(class), Kind::Type { expr }, span);
    }

    fn type_expr(&self, ty: &TypeExpr) -> Option<doc::TypeExpr> {
        let kind = match ty {
            TypeExpr::Name { head, decl, .. } => doc::TypeKind::Name {
                head: head.span,
                target: match decl {
                    Some(decl) => decl.node,
                    None => head.res.and_then(|res| res.node),
                },
            },
            TypeExpr::Const { expr } => doc::TypeKind::Const(match expr.fold(self.file)? {
                Const::Sym(span) => doc::TypeConst::Sym(self.file.str(span).into()),
                Const::Str(value) => doc::TypeConst::Str(value.into()),
                Const::Int(value) => doc::TypeConst::Int(value),
                Const::Bool(value) => doc::TypeConst::Bool(value),
                Const::Nil => doc::TypeConst::Nil,
                Const::Bin(_) | Const::F64(_) | Const::Error => return None,
            }),
            TypeExpr::App { base, args, .. } => doc::TypeKind::App {
                base: alias::Box::new(self.type_expr(base)?),
                args: self.type_args(args)?,
            },
            TypeExpr::Schema { args, .. } => doc::TypeKind::Schema {
                args: self.type_args(args)?,
            },
            TypeExpr::Group { ty: inner, .. } => self.type_expr(inner)?.kind,
            TypeExpr::Union { members, .. } => doc::TypeKind::Union {
                members: members
                    .iter()
                    .map(|member| self.type_expr(member))
                    .collect::<Option<_>>()?,
            },
            TypeExpr::Func { params, ret, .. } => doc::TypeKind::Func {
                params: self.type_args(params)?,
                ret: alias::Box::new(self.type_expr(ret)?),
            },
            TypeExpr::Error => return None,
        };
        Some(doc::TypeExpr {
            span: ty.span(),
            kind,
        })
    }

    fn type_args(&self, args: &[TypeArg]) -> Option<alias::Box<[doc::TypeArg]>> {
        args.iter()
            .map(|arg| {
                let (kind, ty, start) = match &arg.kind {
                    TypeArgKind::Pos(ty) => (doc::TypeArgKind::Pos, Some(ty), None),
                    TypeArgKind::Key { key, ty, .. } => {
                        let key = match key {
                            TypeKey::Sym(span) => *span,
                            TypeKey::Str(expr) => expr.span(),
                        };
                        (doc::TypeArgKind::Key { key }, Some(ty), Some(key))
                    }
                    TypeArgKind::Rest { ellipsis_span, ty } => {
                        (doc::TypeArgKind::Rest, Some(ty), Some(*ellipsis_span))
                    }
                    TypeArgKind::OpenRest { ellipsis_span } => {
                        (doc::TypeArgKind::OpenRest, None, Some(*ellipsis_span))
                    }
                    TypeArgKind::KeyRest {
                        ellipsis_span,
                        key_ty,
                        ty,
                        ..
                    } => (
                        doc::TypeArgKind::KeyRest {
                            key_ty: self.type_expr(key_ty)?,
                        },
                        Some(ty),
                        Some(*ellipsis_span),
                    ),
                };
                let span = [arg.optional, start]
                    .into_iter()
                    .flatten()
                    .chain(ty.map(|ty| ty.span()))
                    .reduce(|acc, span| acc | span)
                    .expect("a type argument has a source span");
                Some(doc::TypeArg {
                    span,
                    optional: arg.optional.is_some(),
                    kind,
                    ty: match ty {
                        Some(ty) => Some(self.type_expr(ty)?),
                        None => None,
                    },
                })
            })
            .collect()
    }

    /// Index the binders of a function, method or class.
    fn binders(&mut self, parent: Option<Id>, binders: Option<&mut Binders>) {
        let (Some(parent), Some(binders)) = (parent, binders) else {
            return;
        };
        for binder in &mut binders.binders {
            let (kind, sigil) = match binder.kind {
                BinderKind::Pos => (crate::BinderKind::Pos, None),
                BinderKind::Key { colon_span } => (crate::BinderKind::Key, Some(colon_span)),
                BinderKind::Rest { ellipsis_span } => {
                    (crate::BinderKind::Rest, Some(ellipsis_span))
                }
            };
            let name = binder.ident.span;
            let span = [
                sigil,
                Some(name),
                binder.bound.as_ref().map(|bound| bound.span()),
                binder.default.as_ref().map(|default| default.ty.span()),
            ]
            .into_iter()
            .flatten()
            .reduce(|left, right| left | right)
            .unwrap();
            let bound = binder
                .bound
                .as_ref()
                .and_then(|bound| self.type_expr(&bound.ty));
            let default = binder
                .default
                .as_ref()
                .and_then(|default| self.type_expr(&default.ty));
            let id = self.push_to(
                Some(parent),
                Kind::Binder {
                    name,
                    kind,
                    bound,
                    default,
                },
                span,
            );
            binder.node = Some(id);
            self.type_decls.insert(name.start, id);
        }
    }

    fn block(&mut self, scope: &Scope<'_>, stmts: &mut [Stmt]) {
        for stmt in stmts.iter_mut() {
            self.predeclare(scope, stmt);
        }
        for stmt in stmts.iter_mut() {
            self.stmt(scope, stmt);
        }
    }

    fn predeclare(&mut self, scope: &Scope<'_>, stmt: &mut Stmt) {
        match stmt {
            Stmt::NlGuard(guard) => self.predeclare(scope, &mut guard.body),
            Stmt::Def(def) => {
                let name = def.ident.span;
                let span = def.span();
                self.declaration(
                    scope,
                    &mut def.ident,
                    Kind::Function {
                        name,
                        is_pub: def.pub_span.is_some(),
                    },
                    span,
                );
            }
            Stmt::Class(class) => {
                let name = class.ident.span;
                let span = class.span();
                self.declaration(
                    scope,
                    &mut class.ident,
                    Kind::Class {
                        name,
                        is_pub: class.pub_span.is_some(),
                    },
                    span,
                );
            }
            Stmt::Import(import) => self.import(scope, import),
            Stmt::TypeAlias(alias) => {
                let name = alias.ident.span;
                let id = self.push(
                    scope,
                    Kind::Alias {
                        name,
                        is_pub: alias.pub_span.is_some(),
                    },
                    alias.span(),
                );
                alias.node = Some(id);
                self.type_decls.insert(name.start, id);
                self.binders(Some(id), alias.binders.as_deref_mut());
                self.type_node(id, &alias.ty);
            }
            _ => {}
        }
    }

    fn import(&mut self, scope: &Scope<'_>, import: &mut Import) {
        let is_pub = import.pub_span.is_some();
        for element in &mut import.elements {
            let element_span = element.span();
            match element {
                ImportElement::ModuleAsIs {
                    module,
                    bind,
                    type_only,
                    ..
                } => {
                    let name = self.file.str(*module).split('.').next().unwrap();
                    let name = Span {
                        start: module.start,
                        end: module.start + name.len() as u32,
                    };
                    let kind = Kind::ImportModule {
                        module: *module,
                        name,
                        is_pub,
                        type_only: type_only.is_some(),
                    };
                    if let Some(type_only) = type_only {
                        let id = self.push(scope, kind, element_span);
                        type_only.node = Some(id);
                        self.type_decls.insert(bind.span.start, id);
                    } else {
                        self.declaration(scope, bind, kind, element_span);
                    }
                }
                ImportElement::ModuleRenamed { module, bind, .. } => {
                    self.declaration(
                        scope,
                        bind,
                        Kind::ImportModule {
                            module: *module,
                            name: bind.span,
                            is_pub,
                            type_only: false,
                        },
                        element_span,
                    );
                }
                ImportElement::Items { module, items } => {
                    for item in items {
                        let item_span = item.span();
                        let (span, bind, type_only) = match item {
                            ImportItem::AsIs {
                                bind, type_only, ..
                            } => (bind.span, bind, type_only),
                            ImportItem::Renamed {
                                item,
                                bind,
                                type_only,
                                ..
                            } => (*item, bind, type_only),
                        };
                        let kind = Kind::ImportItem {
                            module: *module,
                            item: span,
                            name: bind.span,
                            is_pub,
                            type_only: type_only.is_some(),
                        };
                        match type_only {
                            // Only types name the item, so there is no variable to
                            // carry its node
                            Some(type_only) => {
                                let id = self.push(scope, kind, item_span);
                                type_only.node = Some(id);
                                self.type_decls.insert(bind.span.start, id);
                            }
                            None => {
                                self.declaration(scope, bind, kind, item_span);
                            }
                        }
                    }
                }
            }
        }
    }

    fn decorators(&mut self, parent: Id, decorators: &mut [Decorator]) {
        for decorator in decorators {
            let target = match &decorator.expr {
                Expr::Ident(ident) => ident.res.and_then(|res| res.node),
                _ => None,
            };
            self.table.push(Node::new(
                Some(parent),
                Kind::Decorator { target },
                Some(decorator.open_span | decorator.close_span),
                None,
            ));
        }
    }

    fn function(
        &mut self,
        scope: &Scope<'_>,
        func: &mut Function,
        parent: Option<Id>,
        named: bool,
        method: bool,
    ) {
        let mut inner = scope.nested(&mut func.body.vars, parent);
        if named {
            inner.loop_target = None;
            inner.return_target = parent;
        }
        for (i, param) in func.params.iter_mut().enumerate() {
            self.param(&inner, param, true, false, method && i == 0, None);
        }
        if let Some(ret) = &mut func.ret {
            self.ty(&inner, &mut ret.ty);
            if let Some(parent) = parent {
                self.type_node(parent, &ret.ty);
            }
        }
        self.block(&inner, &mut func.body.stmts);
    }

    /// Index a parameter, or a name bound by a pattern written like one.
    ///
    /// `extent` is the declaring statement, for a pattern that is one: a name
    /// bound by `let` extends over the whole `let`, however many names share
    /// it, whereas a loop or handler variable extends over itself alone —
    /// there the enclosing construct is a node of its own.
    fn param(
        &mut self,
        scope: &Scope<'_>,
        param: &mut Param,
        signature: bool,
        is_pub: bool,
        is_self: bool,
        extent: Option<Span>,
    ) {
        let (kind, span) = Self::param_kind(param);
        let ident = match param {
            Param::Pos { ident, default, .. } | Param::Key { ident, default, .. } => {
                if let Some(default) = default {
                    self.expr(scope, &mut default.expr);
                }
                Some(ident)
            }
            Param::ConstKey {
                ident,
                key_expr,
                default,
                ..
            } => {
                self.expr(scope, key_expr);
                if let Some(default) = default {
                    self.expr(scope, &mut default.expr);
                }
                Some(ident)
            }
            Param::Rest { ident, .. } => ident.as_mut(),
        };
        let id = if let Some(ident) = ident {
            let kind = if is_self {
                Kind::SelfParam { name: ident.span }
            } else if signature {
                kind
            } else {
                Kind::Bind {
                    name: ident.span,
                    is_pub,
                }
            };
            let span = if signature {
                span
            } else {
                extent.unwrap_or(ident.span)
            };
            Some(self.declaration(scope, ident, kind, span))
        } else if signature {
            Some(self.push(scope, kind, span))
        } else {
            None
        };
        let (Param::Pos { ty, .. }
        | Param::Key { ty, .. }
        | Param::ConstKey { ty, .. }
        | Param::Rest { ty, .. }) = param;
        self.annot(scope, id, ty);
    }

    fn pattern(
        &mut self,
        scope: &Scope<'_>,
        pattern: &mut Pattern,
        is_pub: bool,
        extent: Option<Span>,
    ) {
        match pattern {
            Pattern::Ident(PatIdent { ident, ty }) => {
                let span = extent.unwrap_or_else(|| match ty {
                    Some(ty) => ident.span | ty.span(),
                    None => ident.span,
                });
                let id = self.declaration(
                    scope,
                    ident,
                    Kind::Bind {
                        name: ident.span,
                        is_pub,
                    },
                    span,
                );
                self.annot(scope, Some(id), ty);
            }
            Pattern::Unpack(params) => {
                for param in params {
                    self.param(scope, param, false, is_pub, false, extent);
                }
            }
        }
    }

    fn stmt(&mut self, scope: &Scope<'_>, stmt: &mut Stmt) {
        match stmt {
            Stmt::NlGuard(guard) => self.stmt(scope, &mut guard.body),
            Stmt::Prim(prim) => self.prim(scope, prim),
            Stmt::Let(node) => {
                let span = node.span();
                self.prim(scope, &mut node.rhs);
                self.pattern(scope, &mut node.bind, node.pub_span.is_some(), Some(span));
            }
            Stmt::Bind(node) => {
                let span = node.span();
                self.expr(scope, &mut node.expr);
                self.pattern(scope, &mut node.bind, false, Some(span));
            }
            Stmt::Assign(node) => {
                self.lvalue(scope, &mut node.lhs);
                self.prim(scope, &mut node.rhs);
            }
            Stmt::Import(_) => {}
            Stmt::TypeAlias(_) => {}
            Stmt::Def(def) => {
                let id = def.ident.res.and_then(|res| res.node);
                for decorator in &mut def.decorators {
                    self.expr(scope, &mut decorator.expr);
                }
                if let Some(id) = id {
                    self.decorators(id, &mut def.decorators);
                }
                self.binders(id, def.binders.as_deref_mut());
                self.function(scope, &mut def.func, id, true, false);
            }
            Stmt::Class(class) => self.class(scope, class),
            Stmt::Return(ret) => {
                let span = ret.span();
                self.push(
                    scope,
                    Kind::Return {
                        target: scope.return_target,
                    },
                    span,
                );
                if let Some(expr) = &mut ret.expr {
                    self.expr(scope, expr);
                }
            }
            Stmt::Break(span, _) => {
                self.push(
                    scope,
                    Kind::Break {
                        target: scope.loop_target,
                    },
                    *span,
                );
            }
            Stmt::Continue(span, _) => {
                self.push(
                    scope,
                    Kind::Continue {
                        target: scope.loop_target,
                    },
                    *span,
                );
            }
            Stmt::Throw(node) => self.expr(scope, &mut node.expr),
            Stmt::While(node) => {
                let span = node.span();
                self.expr(scope, &mut node.expr);
                self.branch(
                    scope,
                    &mut node.body,
                    node.bind.as_mut().map(|bind| &mut bind.pattern),
                    Kind::While,
                    span,
                    Self::block,
                );
            }
            Stmt::For(node) => {
                let span = node.span();
                if let Some(expr) = &mut node.expr {
                    self.expr(scope, expr);
                }
                self.branch(
                    scope,
                    &mut node.body,
                    Some(&mut node.bind),
                    Kind::For,
                    span,
                    Self::block,
                );
            }
        }
    }

    fn class(&mut self, scope: &Scope<'_>, class: &mut Class) {
        let id = class.ident.res.and_then(|res| res.node);
        for decorator in &mut class.decorators {
            self.expr(scope, &mut decorator.expr);
        }
        if let Some(id) = id {
            self.decorators(id, &mut class.decorators);
        }
        self.binders(id, class.binders.as_deref_mut());
        for super_ref in &mut class.super_refs {
            self.reference(scope, &mut super_ref.ident);
            for arg in &mut super_ref.args {
                if let Some(ty) = arg.ty_mut() {
                    self.ty(scope, ty);
                }
            }
            if let Some(id) = id {
                self.super_node(id, super_ref);
            }
        }
        let inner = Scope {
            outer: Some(scope),
            vars: None,
            parent: id.or(scope.parent),
            loop_target: scope.loop_target,
            return_target: scope.return_target,
        };
        let scope = &inner;
        for member in &mut class.body.members {
            match member {
                ClassMember::Method(method) => {
                    let span = method.span();
                    let kind = if method.special.is_some() {
                        // The parentheses are how a protocol name is written,
                        // so they are part of the name: the source says
                        // `(init)` and never `init`.  They sit directly
                        // around it, so the name reaches them by adjacency.
                        Kind::SpecialMethod {
                            name: method.name_span.before_left_char()
                                | method.name_span.after_right_char(),
                        }
                    } else {
                        Kind::Method {
                            name: method.name_span,
                            is_pub: method.pub_span.is_some(),
                        }
                    };
                    let id = self.push(scope, kind, span);
                    method.node = Some(id);
                    for decorator in &mut method.decorators {
                        self.expr(scope, &mut decorator.expr);
                    }
                    self.decorators(id, &mut method.decorators);
                    self.binders(Some(id), method.binders.as_deref_mut());
                    self.function(scope, &mut method.func, Some(id), true, true);
                }
                ClassMember::Field(field) => {
                    // Every name in a `field x y` shares the declaration, so
                    // they share its extent and its documentation too.
                    let span = field.span();
                    for decorator in &mut field.decorators {
                        self.expr(scope, &mut decorator.expr);
                    }
                    match &mut field.init {
                        FieldInit::None => {}
                        FieldInit::Expr(expr) | FieldInit::Const(expr, _) => self.expr(scope, expr),
                        FieldInit::Thunk(func) => self.function(scope, func, None, true, true),
                    }
                    if let Some(annot) = &mut field.ty {
                        self.ty(scope, &mut annot.ty);
                    }
                    for name in &mut field.fields {
                        let id = self.push(
                            scope,
                            Kind::Field {
                                name: name.ident.span,
                                is_pub: field.pub_span.is_some(),
                            },
                            span,
                        );
                        name.node = Some(id);
                        // The names share the annotation, but each is described by it
                        if let Some(annot) = &field.ty {
                            self.type_node(id, &annot.ty);
                        }
                        self.decorators(id, &mut field.decorators);
                    }
                }
            }
        }
    }

    fn prim(&mut self, scope: &Scope<'_>, prim: &mut PrimStmt) {
        match prim {
            PrimStmt::Expr(expr) => self.expr(scope, expr),
            PrimStmt::If(node) => self.if_body(scope, node, false, Self::block),
            PrimStmt::Try(node) => {
                // A handler is a node of its own, so the `try` covers its own
                // body alone: sibling constructs stay disjoint.
                let span = node.try_span | node.body.span();
                let id = self.push(scope, Kind::Try, span);
                self.function(scope, &mut node.body, Some(id), false, false);
                for handler in &mut node.handlers {
                    let span = handler.span();
                    if let Some(expr) = &mut handler.class_expr {
                        self.expr(scope, expr);
                    }
                    let id = self.push(scope, Kind::Catch, span);
                    self.function(scope, &mut handler.func, Some(id), false, false);
                }
                if let Some((func, span)) = &mut node.finally {
                    let span = *span | func.span();
                    let id = self.push(scope, Kind::Finally, span);
                    self.function(scope, func, Some(id), false, false);
                }
            }
        }
    }

    fn branch<T: Body>(
        &mut self,
        scope: &Scope<'_>,
        body: &mut T,
        pattern: Option<&mut Pattern>,
        kind: Kind,
        span: Span,
        visit: fn(&mut Self, &Scope<'_>, &mut [T::Element]),
    ) {
        let is_loop = matches!(kind, Kind::While | Kind::For | Kind::ForElem);
        let id = self.push(scope, kind, span);
        let (vars, elems) = body.parts();
        let mut inner = scope.nested(vars, Some(id));
        if is_loop {
            inner.loop_target = Some(id);
        }
        if let Some(pattern) = pattern {
            self.pattern(&inner, pattern, false, None);
        }
        visit(self, &inner, elems);
    }

    fn if_body<T: Body>(
        &mut self,
        scope: &Scope<'_>,
        node: &mut If<T>,
        elem: bool,
        visit: fn(&mut Self, &Scope<'_>, &mut [T::Element]),
    ) {
        for branch in
            std::iter::once(&mut node.tbranch).chain(node.elif_branches.iter_mut().map(|(b, _)| b))
        {
            let span = branch.span();
            self.expr(scope, &mut branch.expr);
            self.branch(
                scope,
                &mut branch.body,
                branch.bind.as_mut().map(|b| &mut b.pattern),
                if elem { Kind::IfElem } else { Kind::If },
                span,
                visit,
            );
        }
        if let Some((body, span)) = &mut node.else_branch {
            let span = *span | body.span();
            self.branch(scope, body, None, Kind::Else, span, visit);
        }
    }

    fn for_elem<T: Element>(&mut self, scope: &Scope<'_>, node: &mut For<ExprBody<T>>) {
        let span = node.span();
        if let Some(expr) = &mut node.expr {
            self.expr(scope, expr);
        }
        self.branch(
            scope,
            &mut node.body,
            Some(&mut node.bind),
            Kind::ForElem,
            span,
            Self::elements,
        );
    }

    fn elements<T: Element>(&mut self, scope: &Scope<'_>, elems: &mut [T]) {
        for elem in elems {
            elem.index(self, scope);
        }
    }

    fn lvalue(&mut self, scope: &Scope<'_>, value: &mut LValue) {
        match value {
            LValue::Ident(ident) => self.reference(scope, ident),
            LValue::Field { object, .. } | LValue::PrivateField { object, .. } => {
                self.expr(scope, object)
            }
            LValue::Index { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.expr(scope, expr);
                }
            }
        }
    }

    fn expr(&mut self, scope: &Scope<'_>, expr: &mut Expr) {
        match expr {
            Expr::Ident(ident) => self.reference(scope, ident),
            Expr::Group { expr, .. } | Expr::Unary { expr, .. } => self.expr(scope, expr),
            Expr::Binary { exprs, .. } | Expr::Index { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.expr(scope, expr);
                }
            }
            Expr::Range { exprs, .. } => {
                for expr in exprs.iter_mut().flatten() {
                    self.expr(scope, expr);
                }
            }
            Expr::Lambda { func, do_span, .. } => {
                let span = func.span();
                let span = do_span.map_or(span, |do_span| do_span | span);
                let id = self.push(scope, Kind::Lambda, span);
                self.function(scope, func, Some(id), false, false);
            }
            Expr::Call { arg0, args, .. } => {
                self.expr(scope, arg0);
                for arg in args {
                    arg.index(self, scope);
                }
            }
            Expr::Get { object, .. } => self.expr(scope, object),
            Expr::Array { elems, .. } => {
                for elem in elems {
                    elem.index(self, scope);
                }
            }
            Expr::Dict { elems, .. } => {
                for elem in elems {
                    elem.index(self, scope);
                }
            }
            Expr::Concat { exprs, .. }
            | Expr::FmtSeq { exprs, .. }
            | Expr::BinConcat { exprs, .. } => {
                for expr in exprs.iter_mut() {
                    self.expr(scope, expr);
                }
            }
            Expr::Fmt { value, spec, .. } => {
                self.expr(scope, value);
                for expr in [&mut spec.width, &mut spec.precision].into_iter().flatten() {
                    self.expr(scope, expr);
                }
            }
            Expr::FmtParam { spec, .. } => {
                for expr in [&mut spec.width, &mut spec.precision].into_iter().flatten() {
                    self.expr(scope, expr);
                }
            }
            Expr::Escape(..)
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

/// The body of a construct, whether statements or the elements of a literal.
///
/// The [`AstNode`] bound is what lets a construct's extent be taken from
/// the tree rather than guessed at from the keyword that introduces it.
trait Body: AstNode {
    type Element;
    fn parts(&mut self) -> (&mut [Var], &mut [Self::Element]);
}
impl Body for Block {
    type Element = Stmt;
    fn parts(&mut self) -> (&mut [Var], &mut [Stmt]) {
        (&mut self.vars, &mut self.stmts)
    }
}
impl<T: AstNode> Body for ExprBody<T> {
    type Element = T;
    fn parts(&mut self) -> (&mut [Var], &mut [T]) {
        (&mut self.vars, &mut self.elems)
    }
}

trait Element: AstNode {
    fn index(&mut self, index: &mut Index<'_>, scope: &Scope<'_>);
}
impl Element for Arg {
    fn index(&mut self, index: &mut Index<'_>, scope: &Scope<'_>) {
        match self {
            Self::Pos(node) => index.expr(scope, &mut node.expr),
            Self::Key(node) => index.expr(scope, &mut node.expr),
            Self::Expand(node) => index.expr(scope, &mut node.expr),
            Self::DynamicKey(node) => {
                index.expr(scope, &mut node.key);
                index.expr(scope, &mut node.value);
            }
            Self::For(node) => index.for_elem(scope, node),
            Self::If(node) => index.if_body(scope, node, true, Index::elements),
        }
    }
}
impl Element for ArrayElem {
    fn index(&mut self, index: &mut Index<'_>, scope: &Scope<'_>) {
        match self {
            Self::Single(node) => index.expr(scope, &mut node.expr),
            Self::Expand(node) => index.expr(scope, &mut node.expr),
            Self::For(node) => index.for_elem(scope, node),
            Self::If(node) => index.if_body(scope, node, true, Index::elements),
        }
    }
}
impl Element for DictElem {
    fn index(&mut self, index: &mut Index<'_>, scope: &Scope<'_>) {
        match self {
            Self::Single(node) => index.expr(scope, &mut node.expr),
            Self::Key(node) => index.expr(scope, &mut node.expr),
            Self::Pair(node) => {
                index.expr(scope, &mut node.key);
                index.expr(scope, &mut node.value);
            }
            Self::Expand(node) => index.expr(scope, &mut node.expr),
            Self::For(node) => index.for_elem(scope, node),
            Self::If(node) => index.if_body(scope, node, true, Index::elements),
        }
    }
}
