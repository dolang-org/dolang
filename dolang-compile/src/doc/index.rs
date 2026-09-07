//! Annotate the elaborated tree without changing its semantic resolutions.

use std::cell::Cell;

use super::{Id, Kind, Node, Super, Table};
use crate::{
    PreludeImport,
    ast::{visit::Node as _, *},
    source::{File, Span},
};

pub(crate) fn index(root: &mut Root, prelude: &mut [PreludeImport], file: &File<'_>) -> Table {
    let mut index = Index {
        file,
        table: Table::new(),
    };
    let scope = Scope {
        outer: None,
        vars: Some(Cell::from_mut(root.0.body.vars.as_mut_slice()).as_slice_of_cells()),
        parent: None,
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
            | PreludeImport::ModuleRenamed { module, bind, res } => {
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
    table: Table,
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
        let (kind, key_span, ident, default) = match param {
            Param::Pos { ident, default } => (
                Kind::PositionalParam {
                    name: ident.span,
                    default: default.as_ref().map(|default| default.expr.span()),
                },
                None,
                Some(ident.span),
                default,
            ),
            Param::Key {
                key_span,
                ident,
                default,
            } => (
                Kind::KeyParam {
                    key: *key_span,
                    name: ident.span,
                    default: default.as_ref().map(|default| default.expr.span()),
                },
                Some(*key_span),
                Some(ident.span),
                default,
            ),
            Param::ConstKey {
                key_expr,
                ident,
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
                    default,
                )
            }
            Param::Rest {
                ellipsis_span,
                ident,
            } => (
                Kind::RestParam {
                    name: ident.as_ref().map(|ident| ident.span),
                },
                Some(*ellipsis_span),
                ident.as_ref().map(|ident| ident.span),
                &None,
            ),
        };
        let default_span = default.as_ref().map(|default| default.expr.span());
        let span = [key_span, ident, default_span]
            .into_iter()
            .flatten()
            .reduce(|acc, span| acc | span)
            .unwrap_or(Span::INVALID);
        (kind, span)
    }

    fn push(&mut self, scope: &Scope<'_>, kind: Kind, span: Span) -> Id {
        self.table.push(Node::new(scope.parent, kind, span))
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
        if !scope
            .binding(*res)
            .is_some_and(|var| var.get().is_emitted())
        {
            return;
        }
        if let Some(id) = scope.node(*res) {
            res.node = Some(id);
            return;
        }
        let id = self.push(scope, kind, Span::INVALID);
        res.node = Some(id);
        scope.bind(*res, id);
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
                self.declaration(
                    scope,
                    &mut def.ident,
                    Kind::Function {
                        name,
                        is_pub: def.pub_span.is_some(),
                    },
                    def.def_span | name,
                );
            }
            Stmt::Class(class) => {
                let name = class.ident.span;
                self.declaration(
                    scope,
                    &mut class.ident,
                    Kind::Class {
                        name,
                        is_pub: class.pub_span.is_some(),
                        supers: Default::default(),
                    },
                    class.class_span | name,
                );
            }
            Stmt::Import(import) => self.import(scope, import),
            _ => {}
        }
    }

    fn import(&mut self, scope: &Scope<'_>, import: &mut Import) {
        for element in &mut import.0 {
            match element {
                ImportElement::ModuleAsIs { module, bind, .. } => {
                    let name = self.file.str(*module).split('.').next().unwrap();
                    let name = Span {
                        start: module.start,
                        end: module.start + name.len() as u32,
                    };
                    self.declaration(
                        scope,
                        bind,
                        Kind::ImportModule {
                            module: *module,
                            name,
                        },
                        *module,
                    );
                }
                ImportElement::ModuleRenamed { module, bind, .. } => {
                    self.declaration(
                        scope,
                        bind,
                        Kind::ImportModule {
                            module: *module,
                            name: bind.span,
                        },
                        *module,
                    );
                }
                ImportElement::Items { module, items } => {
                    for item in items {
                        let (span, bind) = match item {
                            ImportItem::AsIs { bind, .. } => (bind.span, bind),
                            ImportItem::Renamed { item, bind, .. } => (*item, bind),
                        };
                        self.declaration(
                            scope,
                            bind,
                            Kind::ImportItem {
                                module: *module,
                                item: span,
                                name: bind.span,
                            },
                            span,
                        );
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
                decorator.open_span | decorator.close_span,
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
            self.param(&inner, param, true, false, method && i == 0);
        }
        self.block(&inner, &mut func.body.stmts);
    }

    fn param(
        &mut self,
        scope: &Scope<'_>,
        param: &mut Param,
        signature: bool,
        is_pub: bool,
        is_self: bool,
    ) {
        let (kind, span) = Self::param_kind(param);
        let ident = match param {
            Param::Pos { ident, default } | Param::Key { ident, default, .. } => {
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
        if let Some(ident) = ident {
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
            let span = if signature { span } else { ident.span };
            self.declaration(scope, ident, kind, span);
        } else if signature {
            self.push(scope, kind, span);
        }
    }

    fn pattern(&mut self, scope: &Scope<'_>, pattern: &mut Pattern, is_pub: bool) {
        match pattern {
            Pattern::Ident(ident) => {
                self.declaration(
                    scope,
                    ident,
                    Kind::Bind {
                        name: ident.span,
                        is_pub,
                    },
                    ident.span,
                );
            }
            Pattern::Unpack(params) => {
                for param in params {
                    self.param(scope, param, false, is_pub, false);
                }
            }
        }
    }

    fn stmt(&mut self, scope: &Scope<'_>, stmt: &mut Stmt) {
        match stmt {
            Stmt::NlGuard(guard) => self.stmt(scope, &mut guard.body),
            Stmt::Prim(prim) => self.prim(scope, prim),
            Stmt::Let(node) => {
                self.prim(scope, &mut node.rhs);
                self.pattern(scope, &mut node.bind, node.pub_span.is_some());
            }
            Stmt::Bind(node) => {
                self.expr(scope, &mut node.expr);
                self.pattern(scope, &mut node.bind, false);
            }
            Stmt::Assign(node) => {
                self.lvalue(scope, &mut node.lhs);
                self.prim(scope, &mut node.rhs);
            }
            Stmt::Import(_) => {}
            Stmt::Def(def) => {
                let id = def.ident.res.and_then(|res| res.node);
                for decorator in &mut def.decorators {
                    self.expr(scope, &mut decorator.expr);
                }
                if let Some(id) = id {
                    self.decorators(id, &mut def.decorators);
                }
                self.function(scope, &mut def.func, id, true, false);
            }
            Stmt::Class(class) => self.class(scope, class),
            Stmt::Return(ret) => {
                self.push(
                    scope,
                    Kind::Return {
                        target: scope.return_target,
                    },
                    ret.span,
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
                self.expr(scope, &mut node.expr);
                self.branch(
                    scope,
                    &mut node.body,
                    node.bind.as_mut().map(|bind| &mut bind.pattern),
                    Kind::While,
                    node.while_span,
                    Self::block,
                );
            }
            Stmt::For(node) => {
                if let Some(expr) = &mut node.expr {
                    self.expr(scope, expr);
                }
                self.branch(
                    scope,
                    &mut node.body,
                    Some(&mut node.bind),
                    Kind::For,
                    node.for_span,
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
        let supers = class
            .super_refs
            .iter_mut()
            .map(|s| {
                self.reference(scope, &mut s.ident);
                Super {
                    span: s
                        .fields
                        .last()
                        .map_or(s.ident.span, |field| s.ident.span | *field),
                    target: if s.fields.is_empty() {
                        s.ident.res.and_then(|res| res.node)
                    } else {
                        None
                    },
                }
            })
            .collect();
        if let Some(id) = id
            && let Kind::Class { supers: out, .. } = &mut self.table[id].kind
        {
            *out = supers;
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
                    let kind = if method.special.is_some() {
                        Kind::SpecialMethod {
                            name: method.name_span,
                        }
                    } else {
                        Kind::Method {
                            name: method.name_span,
                            is_pub: method.pub_span.is_some(),
                        }
                    };
                    let id = self.push(scope, kind, method.def_span | method.name_span);
                    method.node = Some(id);
                    for decorator in &mut method.decorators {
                        self.expr(scope, &mut decorator.expr);
                    }
                    self.decorators(id, &mut method.decorators);
                    self.function(scope, &mut method.func, Some(id), true, true);
                }
                ClassMember::Field(field) => {
                    for decorator in &mut field.decorators {
                        self.expr(scope, &mut decorator.expr);
                    }
                    match &mut field.init {
                        FieldInit::None => {}
                        FieldInit::Expr(expr) | FieldInit::Const(expr, _) => self.expr(scope, expr),
                        FieldInit::Thunk(func) => self.function(scope, func, None, true, true),
                    }
                    for name in &mut field.fields {
                        let id = self.push(
                            scope,
                            Kind::Field {
                                name: name.ident.span,
                                is_pub: field.pub_span.is_some(),
                            },
                            name.ident.span,
                        );
                        name.node = Some(id);
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
                let id = self.push(scope, Kind::Try, node.try_span);
                self.function(scope, &mut node.body, Some(id), false, false);
                for handler in &mut node.handlers {
                    if let Some(expr) = &mut handler.class_expr {
                        self.expr(scope, expr);
                    }
                    let id = self.push(scope, Kind::Catch, handler.catch_span);
                    self.function(scope, &mut handler.func, Some(id), false, false);
                }
                if let Some((func, span)) = &mut node.finally {
                    let id = self.push(scope, Kind::Finally, *span);
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
            self.pattern(&inner, pattern, false);
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
            self.expr(scope, &mut branch.expr);
            self.branch(
                scope,
                &mut branch.body,
                branch.bind.as_mut().map(|b| &mut b.pattern),
                if elem { Kind::IfElem } else { Kind::If },
                branch.span,
                visit,
            );
        }
        if let Some((body, span)) = &mut node.else_branch {
            self.branch(scope, body, None, Kind::Else, *span, visit);
        }
    }

    fn for_elem<T: Element>(&mut self, scope: &Scope<'_>, node: &mut For<ExprBody<T>>) {
        if let Some(expr) = &mut node.expr {
            self.expr(scope, expr);
        }
        self.branch(
            scope,
            &mut node.body,
            Some(&mut node.bind),
            Kind::ForElem,
            node.for_span,
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
                let id = self.push(scope, Kind::Lambda, do_span.unwrap_or_else(|| func.span()));
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

trait Body {
    type Element;
    fn parts(&mut self) -> (&mut [Var], &mut [Self::Element]);
}
impl Body for Block {
    type Element = Stmt;
    fn parts(&mut self) -> (&mut [Var], &mut [Stmt]) {
        (&mut self.vars, &mut self.stmts)
    }
}
impl<T> Body for ExprBody<T> {
    type Element = T;
    fn parts(&mut self) -> (&mut [Var], &mut [T]) {
        (&mut self.vars, &mut self.elems)
    }
}

trait Element {
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
