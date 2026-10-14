//! Expressions, and the conditions of statements.

use std::{mem, rc::Rc};

use super::{
    Scope, expr,
    scope::{Entry, Frame, Spelled},
};
use crate::{
    ast::{
        self, Arg, ArrayElem, DictElem, ExprBody, For, GetVariant, Ident, If, Key, Pair, Single,
        SpecialMethod, visit::Node,
    },
    lex::Op,
    source::Span,
    typeck::{
        cfg::{
            Against, Assume, BlockId, Collection, EscapeKind, Expr, ExprKind, FmtSpec, Item,
            Member, Relation, Step, Terminal, VarFlags, VarId,
        },
        r#type::{Literal, MemberKey},
    },
};

/// The narrowing a condition gives each of its edges
#[derive(Default)]
struct Narrowing {
    truthy: Vec<Assume>,
    falsy: Vec<Assume>,
}

impl<'u> Scope<'_, '_, 'u> {
    pub(super) fn expr(&mut self, node: &'u ast::Expr) -> Expr {
        let span = node.span();
        let kind = match node {
            ast::Expr::Literal(span) => str_literal(self.text(*span)),
            ast::Expr::Escape(char, _) => str_literal(&char.to_string()),
            ast::Expr::Int(value, _) | ast::Expr::VerbatimInt(value, _) => {
                ExprKind::Literal(Literal::Int(*value))
            }
            ast::Expr::F64(..) | ast::Expr::VerbatimF64(..) => ExprKind::Float,
            ast::Expr::Bool(value, _) => ExprKind::Literal(Literal::Bool(*value)),
            ast::Expr::Nil(_) => ExprKind::Literal(Literal::Nil),
            ast::Expr::Sym(span) => ExprKind::Literal(Literal::Sym(self.symbol(*span))),
            ast::Expr::EscapeByte(..) => ExprKind::Bin,
            // Raises an error instead of giving a value
            ast::Expr::Stub(_) => ExprKind::Never,
            ast::Expr::BinConcat { exprs, .. } => self.bin_concat(exprs, span),
            ast::Expr::Concat { exprs, .. } => self.concat(exprs, span),
            ast::Expr::Fmt { value, spec, .. } => ExprKind::FmtValue {
                value: Box::new(self.expr(value)),
                spec: self.spec(spec),
            },
            ast::Expr::FmtParam { name, spec, .. } => {
                let (name, name_span) = match name {
                    ast::FmtParamName::Pos(index, span) => {
                        (Literal::Int(i128::from(*index)), *span)
                    }
                    ast::FmtParamName::Named(span) => (Literal::Sym(self.symbol(*span)), *span),
                };
                ExprKind::FmtParam {
                    name,
                    name_span,
                    spec: self.spec(spec),
                }
            }
            ast::Expr::FmtSeq { exprs, .. } => self.fmt_seq(exprs, span),
            ast::Expr::Group { expr, .. } => return self.expr(expr),
            ast::Expr::Cast {
                expr,
                annot,
                checked,
                ..
            } => ExprKind::Cast {
                value: Box::new(self.expr(expr)),
                ty: self
                    .annotation(annot)
                    .unwrap_or_else(|| self.lower.db.unknown()),
                checked: *checked,
            },
            ast::Expr::Ident(ident) => match self.ident(ident) {
                kind @ ExprKind::Var(_) => return self.spill(expr(kind, span)),
                kind => kind,
            },
            ast::Expr::Unary { op, expr, .. } => ExprKind::Unary {
                op: *op,
                operand: Box::new(Item::Pos(self.expr(expr))),
            },
            ast::Expr::Logical { .. } => {
                let hoist = mem::replace(&mut self.ctx.hoist, false);
                let join = self.block();
                self.short_circuit(node, join, join, true);
                self.switch(join);
                self.ctx.hoist = hoist;
                return self.spill(expr(ExprKind::Operand, span));
            }
            ast::Expr::Binary { op, exprs, .. } => {
                let left = Item::Pos(self.expr(&exprs[0]));
                let right = Item::Pos(self.expr(&exprs[1]));
                ExprKind::Binary {
                    op: *op,
                    operands: Box::new([left, right]),
                }
            }
            ast::Expr::Range { exprs, .. } => {
                let start = exprs[0].as_ref().map(|expr| self.expr(expr));
                let end = exprs[1].as_ref().map(|expr| self.expr(expr));
                ExprKind::Range {
                    bounds: Box::new([start, end]),
                }
            }
            ast::Expr::Call { arg0, args, .. } => self.call(arg0, args),
            ast::Expr::Lambda { func, .. } => {
                let decl = self.lower.decl(super::DeclKey::closure(func));
                let class = self.ctx.class;
                ExprKind::Lambda(self.function_value(decl, func, true, class, None, span))
            }
            ast::Expr::Get { object, field, .. } if let Some(var) = self.tracked(object, field) => {
                return self.spill(expr(ExprKind::Var(var), span));
            }
            ast::Expr::Get { object, field, .. } => match self.import_path(node) {
                Some(import) => import,
                None => ExprKind::Get {
                    object: Box::new(self.expr(object)),
                    member: self.member(field),
                },
            },
            ast::Expr::Index { exprs, .. } => {
                let object = self.expr(&exprs[0]);
                let index = Item::Pos(self.expr(&exprs[1]));
                ExprKind::Index {
                    object: Box::new(object),
                    index: Box::new(index),
                }
            }
            ast::Expr::Array { elems, .. } => {
                self.collection(Collection::Array, |scope| scope.array_items(elems))
            }
            ast::Expr::Tuple { elems, .. } => {
                self.collection(Collection::Tuple, |scope| scope.array_items(elems))
            }
            ast::Expr::Record { args, .. } => {
                self.collection(Collection::Record, |scope| scope.args(args))
            }
            ast::Expr::Dict { elems, .. } => {
                self.collection(Collection::Dict, |scope| scope.dict_items(elems))
            }
            ast::Expr::Error => ExprKind::Error,
        };
        expr(kind, span)
    }

    /// The evaluated parts of a format specification
    fn spec(&mut self, spec: &'u ast::FormatSpec) -> FmtSpec {
        let mut count =
            |count: &'u Option<ast::Expr>| count.as_ref().map(|count| Box::new(self.expr(count)));
        FmtSpec {
            width: count(&spec.width),
            precision: count(&spec.precision),
        }
    }

    /// The parts of a string, with adjacent literal text folded. `interp` lowers
    /// each other part.
    fn text_parts(
        &mut self,
        nodes: &'u [ast::Expr],
        span: Span,
        mut interp: impl FnMut(&mut Self, &'u ast::Expr) -> Expr,
    ) -> (Vec<Expr>, String) {
        let mut parts = Vec::new();
        let mut text = String::new();
        for node in nodes {
            match node {
                ast::Expr::Literal(span) => text.push_str(self.text(*span)),
                ast::Expr::Escape(char, _) => text.push(*char),
                node => {
                    if !text.is_empty() {
                        parts.push(expr(str_literal(&mem::take(&mut text)), span));
                    }
                    parts.push(interp(self, node));
                }
            }
        }
        (parts, text)
    }

    /// A string built from parts
    fn concat(&mut self, nodes: &'u [ast::Expr], span: Span) -> ExprKind {
        let (mut parts, text) = self.text_parts(nodes, span, Self::expr);
        if parts.is_empty() {
            return str_literal(&text);
        }
        if !text.is_empty() {
            parts.push(expr(str_literal(&text), span));
        }
        ExprKind::Concat(parts.into())
    }

    /// A `t"..."` sequence, in which an interpolation stating no specification is
    /// bound to an empty one
    fn fmt_seq(&mut self, nodes: &'u [ast::Expr], span: Span) -> ExprKind {
        let (mut parts, text) = self.text_parts(nodes, span, |scope, node| match node {
            ast::Expr::Fmt { .. } | ast::Expr::FmtParam { .. } => scope.expr(node),
            node => {
                let value = scope.expr(node);
                let kind = ExprKind::FmtValue {
                    value: Box::new(value),
                    spec: FmtSpec {
                        width: None,
                        precision: None,
                    },
                };
                expr(kind, node.span())
            }
        });
        if !text.is_empty() {
            parts.push(expr(str_literal(&text), span));
        }
        ExprKind::Fmt(parts.into())
    }

    /// A binary string, folded to a constant when it has no interpolations
    fn bin_concat(&mut self, nodes: &'u [ast::Expr], span: Span) -> ExprKind {
        let constant = |node: &ast::Expr| {
            matches!(
                node,
                ast::Expr::Literal(_) | ast::Expr::Escape(..) | ast::Expr::EscapeByte(..)
            )
        };
        if nodes.iter().all(constant) {
            return ExprKind::Bin;
        }
        let mut parts = Vec::new();
        let mut run = false;
        for node in nodes {
            if constant(node) {
                if !run {
                    parts.push(expr(ExprKind::Bin, span));
                }
                run = true;
            } else {
                parts.push(self.expr(node));
                run = false;
            }
        }
        ExprKind::BinConcat {
            parts: parts.into(),
        }
    }

    fn collection(
        &mut self,
        kind: Collection,
        items: impl FnOnce(&mut Self) -> Vec<Item>,
    ) -> ExprKind {
        ExprKind::Collection {
            kind,
            items: items(self).into(),
        }
    }

    fn ident(&mut self, ident: &Ident) -> ExprKind {
        match self.entry(ident.res) {
            Some(Entry::Var(var) | Entry::Item { var: Some(var), .. }) => {
                self.capture(var);
                // Any use of an `(init)`'s receiver but an access of a field that
                // doesn't dispatch may observe its fields
                if self.receiver() == Some(var) {
                    self.emit(Step::Escape {
                        span: ident.span,
                        kind: EscapeKind::Use,
                        except: None,
                        supers: self.ctx.partial != Some(ident.span),
                    });
                }
                ExprKind::Var(var)
            }
            Some(Entry::Item {
                module,
                item,
                var: None,
            }) => ExprKind::Import {
                module: self.lower.module(module),
                item: Some(self.lower.symbol(item)),
            },
            Some(Entry::Modules(modules)) => self
                .module_path(&modules, &[self.text(ident.span)])
                .unwrap_or(ExprKind::Namespace),
            None => ExprKind::Error,
        }
    }

    /// A dotted path through a module to an item of it, or to the module itself
    pub(super) fn import_path(&self, node: &ast::Expr) -> Option<ExprKind> {
        let mut fields = Vec::new();
        let mut node = node;
        let head = loop {
            match node {
                ast::Expr::Get {
                    object,
                    field: GetVariant::Normal(span),
                    ..
                } => {
                    fields.push(self.text(*span));
                    node = object;
                }
                ast::Expr::Ident(ident) => break ident,
                _ => return None,
            }
        };
        let Some(Entry::Modules(modules)) = self.entry(head.res) else {
            return None;
        };
        let path: Vec<&str> = std::iter::once(self.text(head.span))
            .chain(fields.into_iter().rev())
            .collect();
        self.module_path(&modules, &path)
    }

    fn module_path(&self, modules: &[Spelled<'u>], path: &[&str]) -> Option<ExprKind> {
        modules.iter().find_map(|module| {
            let parts: Vec<&str> = module.spelled.split('.').collect();
            let item = match path.strip_prefix(&parts[..])? {
                [] => None,
                [item] => Some(self.lower.symbol(item)),
                _ => return None,
            };
            Some(ExprKind::Import {
                module: self.lower.module(module.module),
                item,
            })
        })
    }

    fn call(&mut self, callee: &'u ast::Expr, args: &'u [Arg]) -> ExprKind {
        if let Some(import) = self.import_path(callee) {
            let callee = expr(import, callee.span());
            return ExprKind::Call {
                callee: Box::new(callee),
                args: self.args(args).into(),
            };
        }
        if let ast::Expr::Get {
            object,
            field:
                field @ GetVariant::SpecialMethod {
                    method: SpecialMethod::Init,
                    ..
                },
            ..
        } = callee
            && let Some(flag) = self.super_flag(object)
        {
            return self.super_init(object, field, callee.span(), args, flag);
        }
        if let ast::Expr::Get { object, field, .. } = callee
            && self.tracked(object, field).is_none()
        {
            let receiver = self.expr(object);
            return ExprKind::Invoke {
                receiver: Box::new(receiver),
                member: self.member(field),
                args: self.args(args).into(),
            };
        }
        let callee = self.expr(callee);
        ExprKind::Call {
            callee: Box::new(callee),
            args: self.args(args).into(),
        }
    }

    /// An `(init)`'s receiver, once its parameters are bound
    fn receiver(&self) -> Option<VarId> {
        self.ctx.init.as_ref()?.receiver
    }

    /// The flag of the superclass whose `(init)` is called on `object`, when it's
    /// named directly in an `(init)`
    fn super_flag(&self, object: &ast::Expr) -> Option<VarId> {
        let init = self.ctx.init.as_ref()?;
        let mut head = strip(object);
        while let ast::Expr::Get {
            object,
            field: GetVariant::Normal(_),
            ..
        } = head
        {
            head = strip(object);
        }
        let ast::Expr::Ident(ident) = head else {
            return None;
        };
        let class = self.super_decl(ident.span)?;
        (init.supers.iter()).find_map(|&(decl, var)| (decl == class).then_some(var))
    }

    /// A call of a superclass's `(init)`, which initializes it once it returns. It
    /// takes the receiver partly initialized.
    fn super_init(
        &mut self,
        object: &'u ast::Expr,
        field: &GetVariant,
        span: Span,
        args: &'u [Arg],
        flag: VarId,
    ) -> ExprKind {
        let receiver = self.expr(object);
        let partial = match args.first() {
            Some(Arg::Pos(single)) => match strip(&single.expr) {
                ast::Expr::Ident(ident) if self.var(ident) == self.receiver() => Some(ident.span),
                _ => None,
            },
            _ => None,
        };
        let outer = mem::replace(&mut self.ctx.partial, partial);
        let args = self.args(args).into();
        self.ctx.partial = outer;
        let call = ExprKind::Invoke {
            receiver: Box::new(receiver),
            member: self.member(field),
            args,
        };
        let result = self.temporary(expr(call, span));
        self.assign(flag, expr(ExprKind::Literal(Literal::Bool(true)), span));
        ExprKind::Copy(result)
    }

    /// The variable standing for a field of the receiver, when `object` is an
    /// `(init)`'s receiver and `field` an instance field its class declares, and
    /// whether accessing the field may dispatch to a property overriding it
    pub(super) fn own_field(
        &self,
        object: &ast::Expr,
        field: Span,
        private: bool,
    ) -> Option<(VarId, bool)> {
        let init = self.ctx.init.as_ref()?;
        let ast::Expr::Ident(ident) = strip(object) else {
            return None;
        };
        if self.var(ident)? != init.receiver? {
            return None;
        }
        let var = *init
            .fields
            .get(&self.member_key(field, false, private).key)?;
        Some((var, !private && !init.final_))
    }

    /// The variable standing for a field of the receiver whose accesses never
    /// dispatch, so they read and write it directly
    fn tracked(&self, object: &ast::Expr, field: &GetVariant) -> Option<VarId> {
        let (span, private) = match field {
            GetVariant::Normal(span) => (*span, false),
            GetVariant::Private { span, .. } => (*span, true),
            GetVariant::SpecialMethod { .. } => return None,
        };
        match self.own_field(object, span, private)? {
            (var, false) => Some(var),
            (_, true) => None,
        }
    }

    pub(super) fn member(&self, field: &GetVariant) -> Member {
        let (span, special, private) = match field {
            GetVariant::Normal(span) => (*span, false, false),
            GetVariant::SpecialMethod { span, .. } => (*span, true, false),
            GetVariant::Private { span, .. } => (*span, false, true),
        };
        self.member_key(span, special, private)
    }

    pub(super) fn member_key(&self, name: Span, special: bool, private: bool) -> Member {
        Member {
            key: MemberKey {
                name: self.symbol(name),
                special,
                private,
            },
            class: if private { self.ctx.class } else { None },
        }
    }

    pub(super) fn args(&mut self, args: &'u [Arg]) -> Vec<Item> {
        args.iter()
            .map(|arg| match arg {
                Arg::Pos(Single { expr, .. }) => Item::Pos(self.expr(expr)),
                Arg::Key(Key { key_span, expr, .. }) => {
                    Item::Key(self.symbol(*key_span), self.expr(expr))
                }
                Arg::DynamicKey(Pair { key, value, .. }) => {
                    let key = self.expr(key);
                    Item::Pair(key, self.expr(value))
                }
                Arg::Expand(expand) => Item::Spread(self.expr(&expand.expr)),
                Arg::For(node) => self.item_for(node, Self::args),
                Arg::If(node) => self.item_if(node, Self::args),
            })
            .collect()
    }

    fn array_items(&mut self, elems: &'u [ArrayElem]) -> Vec<Item> {
        elems
            .iter()
            .map(|elem| match elem {
                ArrayElem::Single(Single { expr, .. }) => Item::Pos(self.expr(expr)),
                ArrayElem::Expand(expand) => Item::Spread(self.expr(&expand.expr)),
                ArrayElem::For(node) => self.item_for(node, Self::array_items),
                ArrayElem::If(node) => self.item_if(node, Self::array_items),
            })
            .collect()
    }

    fn dict_items(&mut self, elems: &'u [DictElem]) -> Vec<Item> {
        elems
            .iter()
            .map(|elem| match elem {
                DictElem::Single(Single { expr, .. }) => Item::Pos(self.expr(expr)),
                DictElem::Key(Key { key_span, expr, .. }) => {
                    Item::Key(self.symbol(*key_span), self.expr(expr))
                }
                DictElem::Pair(Pair { key, value, .. }) => {
                    let key = self.expr(key);
                    Item::Pair(key, self.expr(value))
                }
                DictElem::Expand(expand) => Item::Spread(self.expr(&expand.expr)),
                DictElem::For(node) => self.item_for(node, Self::dict_items),
                DictElem::If(node) => self.item_if(node, Self::dict_items),
            })
            .collect()
    }

    /// In a comprehension's body, put a value that depends on where it's evaluated
    /// in a variable, assigned where the body runs: a variable read, which sees the
    /// body's narrowing, or a short circuit's result, which can't cross from the
    /// body's operand stack to the collection's. Everything else stays in the tree,
    /// where the rule's expected type reaches it.
    fn spill(&self, value: Expr) -> Expr {
        if !self.ctx.hoist {
            return value;
        }
        let span = value.span;
        let var = self.synthetic();
        self.graph().var_mut(var).flags.insert(VarFlags::BOTTOM);
        self.assign(var, value);
        expr(ExprKind::Copy(var), span)
    }

    /// Lower a comprehension body's items in `frame`, marking its bindings as
    /// starting at bottom, since the collection reads them only where the body ran
    fn body_items<T>(
        &mut self,
        frame: &Rc<Frame<'u>>,
        elems: &'u [T],
        items: fn(&mut Self, &'u [T]) -> Vec<Item>,
    ) -> Vec<Item> {
        let hoist = mem::replace(&mut self.ctx.hoist, true);
        let items = self.in_frame(frame, |scope| items(scope, elems));
        self.ctx.hoist = hoist;
        for var in frame.vars() {
            self.graph().var_mut(var).flags.insert(VarFlags::BOTTOM);
        }
        items
    }

    /// A new scope for a comprehension body
    fn body_frame<T>(&self, body: &'u ExprBody<T>) -> Rc<Frame<'u>> {
        self.lower.frame(
            self.ctx.func,
            Some(self.ctx.frame.clone()),
            &body.vars,
            &[],
            None,
        )
    }

    /// A comprehension's loop. Its body runs once, from the loop's head to its
    /// exit: nothing in it assigns, so no state crosses iterations.
    fn item_for<T>(
        &mut self,
        node: &'u For<ExprBody<T>>,
        items: fn(&mut Self, &'u [T]) -> Vec<Item>,
    ) -> Item {
        let hoist = mem::replace(&mut self.ctx.hoist, false);
        let frame = self.body_frame(&node.body);
        let (_, body, exit) = self.next_head(node, &frame);
        self.switch(body);
        let items = self.body_items(&frame, &node.body.elems, items);
        self.end(Terminal::Branch {
            target: exit,
            span: Span::INVALID,
        });
        self.switch(exit);
        self.ctx.hoist = hoist;
        Item::For {
            items: items.into(),
            span: node.for_span,
        }
    }

    /// A comprehension's filter, lowered as an `if` statement whose branches
    /// assign their items' values
    fn item_if<T>(
        &mut self,
        node: &'u If<ExprBody<T>>,
        items: fn(&mut Self, &'u [T]) -> Vec<Item>,
    ) -> Item {
        let hoist = mem::replace(&mut self.ctx.hoist, false);
        let complete = node.else_branch.is_some();
        let join = self.block();
        let branches: Vec<_> = std::iter::once(&node.tbranch)
            .chain(node.elif_branches.iter().map(|(branch, _)| branch))
            .collect();
        let mut arms = Vec::new();
        let mut spans = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            let fallback = if index + 1 < branches.len() || complete {
                self.block()
            } else {
                join
            };
            let frame = self.body_frame(&branch.body);
            let then = self.block();
            self.test(&branch.expr, branch.bind.as_ref(), &frame, then, fallback);
            self.switch(then);
            arms.push(self.body_items(&frame, &branch.body.elems, items));
            spans.push(branch.span);
            self.end(Terminal::Branch {
                target: join,
                span: Span::INVALID,
            });
            self.switch(fallback);
        }
        let mut else_ = match &node.else_branch {
            Some((body, _)) => {
                let frame = self.body_frame(body);
                let items = self.body_items(&frame, &body.elems, items);
                self.end(Terminal::Branch {
                    target: join,
                    span: Span::INVALID,
                });
                self.switch(join);
                items
            }
            None => Vec::new(),
        };
        for (then, span) in arms.into_iter().zip(spans).rev() {
            else_ = vec![Item::If {
                then: then.into(),
                else_: else_.into(),
                span,
            }];
        }
        self.ctx.hoist = hoist;
        else_.pop().expect("an `if` has a first branch")
    }

    /// Push a value, unless it's already on top of the stack
    pub(super) fn push(&self, value: Expr) {
        if !matches!(value.kind, ExprKind::Operand) {
            self.emit(Step::Push(value));
        }
    }

    /// Branch on a condition to `then` or `else_`, narrowing on each edge
    pub(super) fn cond(&mut self, node: &'u ast::Expr, then: BlockId, else_: BlockId) {
        self.short_circuit(node, then, else_, false);
    }

    /// Branch on a condition to `then` or `else_`, testing each operand of a short
    /// circuit once and narrowing on each edge. With `want_result`, the operand
    /// that decided is on the stack at the target; a target both edges share
    /// isn't tested for.
    fn short_circuit(
        &mut self,
        node: &'u ast::Expr,
        then: BlockId,
        else_: BlockId,
        want_result: bool,
    ) {
        match node {
            ast::Expr::Group { expr, .. } => self.short_circuit(expr, then, else_, want_result),
            ast::Expr::Unary {
                op: Op::Bang, expr, ..
            } if !want_result => self.short_circuit(expr, else_, then, false),
            ast::Expr::Logical { op, exprs, .. } => {
                let (last, init) = exprs.split_last().expect("a short circuit has operands");
                for operand in init {
                    let long = self.block();
                    let (then, else_) = match op {
                        Op::AmpAmp => (long, else_),
                        _ => (then, long),
                    };
                    self.short_circuit(operand, then, else_, want_result);
                    self.switch(long);
                    if want_result {
                        self.emit(Step::Pop);
                    }
                }
                self.short_circuit(last, then, else_, want_result);
            }
            node if then == else_ => {
                let value = self.expr(node);
                if want_result {
                    self.push(value);
                } else {
                    self.emit(Step::Eval(value));
                }
                self.end(Terminal::Branch {
                    target: then,
                    span: Span::INVALID,
                });
            }
            node => {
                let narrowing = self.narrowing(node);
                let mut cond = self.expr(node);
                if want_result {
                    let span = cond.span;
                    self.push(cond);
                    self.emit(Step::Dup);
                    cond = expr(ExprKind::Operand, span);
                }
                let then = self.landing(then, narrowing.truthy);
                let else_ = self.landing(else_, narrowing.falsy);
                self.end(Terminal::If { cond, then, else_ });
            }
        }
    }

    /// A block holding an edge's narrowing, on its way to `target`
    fn landing(&self, target: BlockId, assumes: Vec<Assume>) -> BlockId {
        if assumes.is_empty() {
            return target;
        }
        let block = self.block();
        let mut landing = self.graph().block_mut(block);
        landing.steps.extend(assumes.into_iter().map(Step::Assume));
        landing.terminal = Terminal::Branch {
            target,
            span: Span::INVALID,
        };
        block
    }

    /// The narrowing a condition gives when it's truthy and when it's falsy
    fn narrowing(&mut self, node: &'u ast::Expr) -> Narrowing {
        match node {
            ast::Expr::Group { expr, .. } => self.narrowing(expr),
            // Only `nil` and `false` are always falsy
            ast::Expr::Ident(_) | ast::Expr::Get { .. } => match self.subject(node) {
                Some(var) => Narrowing {
                    truthy: [Literal::Nil, Literal::Bool(false)]
                        .into_iter()
                        .map(|literal| Assume {
                            var,
                            relation: Relation::Exact,
                            negated: true,
                            against: Against::Value(expr(ExprKind::Literal(literal), node.span())),
                        })
                        .collect(),
                    falsy: Vec::new(),
                },
                None => Narrowing::default(),
            },
            ast::Expr::Call { arg0, args, .. } => {
                let [Arg::Pos(value), Arg::Pos(class)] = &args[..] else {
                    return Narrowing::default();
                };
                let (Some(var), true) = (self.subject(&value.expr), self.is_type(arg0)) else {
                    return Narrowing::default();
                };
                self.relation(var, Relation::Upper, false, |scope| {
                    scope.class_operand(&class.expr).map(Against::Class)
                })
            }
            ast::Expr::Binary {
                op: op @ (Op::EqEq | Op::BangEq),
                exprs,
                ..
            } => {
                let negated = *op == Op::BangEq;
                for (subject, other) in [(&exprs[0], &exprs[1]), (&exprs[1], &exprs[0])] {
                    if let Some(var) = self.subject(subject)
                        && is_literal(other)
                    {
                        return self.relation(var, Relation::Exact, negated, |scope| {
                            Some(Against::Value(scope.expr(other)))
                        });
                    }
                    // `(type x) == C`
                    if let ast::Expr::Call { arg0, args, .. } = strip(subject)
                        && let [Arg::Pos(value)] = &args[..]
                        && self.is_type(arg0)
                        && let Some(var) = self.subject(&value.expr)
                    {
                        return self.relation(var, Relation::Exact, negated, |scope| {
                            scope.class_operand(other).map(Against::Class)
                        });
                    }
                }
                Narrowing::default()
            }
            _ => Narrowing::default(),
        }
    }

    /// Narrowing by one relation, positive on the truthy edge unless `negated`. The
    /// type operand is lowered once for each edge.
    fn relation(
        &mut self,
        var: VarId,
        relation: Relation,
        negated: bool,
        mut against: impl FnMut(&mut Self) -> Option<Against>,
    ) -> Narrowing {
        let (Some(first), Some(second)) = (against(self), against(self)) else {
            return Narrowing::default();
        };
        let assume = |against, negated| Assume {
            var,
            relation,
            negated,
            against,
        };
        Narrowing {
            truthy: vec![assume(first, negated)],
            falsy: vec![assume(second, !negated)],
        }
    }

    /// The variable a narrowing test is about: a name's, or a tracked field's
    fn subject(&self, node: &ast::Expr) -> Option<VarId> {
        match strip(node) {
            ast::Expr::Ident(ident) => self.narrowed(ident),
            ast::Expr::Get { object, field, .. } => self.tracked(object, field),
            _ => None,
        }
    }

    fn narrowed(&self, ident: &Ident) -> Option<VarId> {
        self.var(ident)
    }

    /// Whether a callee is std's `type`
    fn is_type(&self, node: &ast::Expr) -> bool {
        let ast::Expr::Ident(ident) = strip(node) else {
            return false;
        };
        matches!(
            self.entry(ident.res),
            Some(Entry::Item {
                module: "std",
                item: "type",
                ..
            })
        )
    }

    /// A class operand to narrow against: a name or a dotted path that involves no
    /// checking rule, since each edge evaluates it again
    fn class_operand(&mut self, node: &'u ast::Expr) -> Option<Expr> {
        if !is_path(node) {
            return None;
        }
        let class = self.expr(node);
        let mut plain = true;
        class.walk(&mut |expr| plain &= !expr.is_rule());
        plain.then_some(class)
    }
}

/// Whether an expression is a name or a dotted path from one, which lowers to a
/// tree without blocks
pub(super) fn is_path(node: &ast::Expr) -> bool {
    match node {
        ast::Expr::Ident(_) => true,
        ast::Expr::Group { expr, .. } => is_path(expr),
        ast::Expr::Get {
            object,
            field: GetVariant::Normal(_),
            ..
        } => is_path(object),
        _ => false,
    }
}

fn strip(node: &ast::Expr) -> &ast::Expr {
    match node {
        ast::Expr::Group { expr, .. } => strip(expr),
        node => node,
    }
}

/// A literal a variable can be compared with to narrow it
fn is_literal(node: &ast::Expr) -> bool {
    matches!(
        strip(node),
        ast::Expr::Nil(_)
            | ast::Expr::Bool(..)
            | ast::Expr::Int(..)
            | ast::Expr::Sym(_)
            | ast::Expr::Literal(_)
    )
}

fn str_literal(text: &str) -> ExprKind {
    ExprKind::Literal(Literal::Str(text.into()))
}
