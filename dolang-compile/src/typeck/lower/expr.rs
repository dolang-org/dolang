//! Expressions, and the conditions of statements.

use std::{mem, rc::Rc};

use super::{
    Scope, expr,
    scope::{Entry, Frame, Spelled},
};
use crate::{
    ast::{
        self, Arg, ArrayElem, DictElem, ExprBody, For, GetVariant, Ident, If, Key, Pair, Single,
        visit::Node,
    },
    lex::Op,
    source::Span,
    typeck::{
        cfg::{
            Against, Assume, BlockId, Collection, Expr, ExprKind, FmtSpec, Item, Member, Relation,
            Step, Terminal, VarId,
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
            ast::Expr::FmtParam { spec, .. } => ExprKind::FmtParam {
                spec: self.spec(spec),
            },
            ast::Expr::FmtSeq { exprs, .. } => self.fmt_seq(exprs, span),
            ast::Expr::Group { expr, .. } => return self.expr(expr),
            ast::Expr::Ident(ident) => self.ident(ident),
            ast::Expr::Unary { op, expr, .. } => ExprKind::Unary {
                op: *op,
                operand: Box::new(self.expr(expr)),
            },
            ast::Expr::Binary {
                op: op @ (Op::AmpAmp | Op::BarBar),
                exprs,
                ..
            } => return self.short_circuit(*op, &exprs[0], &exprs[1], span),
            ast::Expr::Binary { op, exprs, .. } => {
                let left = self.expr(&exprs[0]);
                let right = self.expr(&exprs[1]);
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
                ExprKind::Lambda(self.function_value(decl, func, true, self.ctx.class))
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
                let index = self.expr(&exprs[1]);
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
        ExprKind::Concat(parts)
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
        ExprKind::Fmt(parts)
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
        ExprKind::BinConcat { parts }
    }

    fn collection(
        &mut self,
        kind: Collection,
        items: impl FnOnce(&mut Self) -> Vec<Item>,
    ) -> ExprKind {
        ExprKind::Collection {
            kind,
            items: items(self),
        }
    }

    fn ident(&mut self, ident: &Ident) -> ExprKind {
        match self.entry(ident.res) {
            Some(Entry::Var(var)) => {
                self.capture(var);
                ExprKind::Var(var)
            }
            Some(Entry::Item { module, item }) => ExprKind::Import {
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
    fn import_path(&self, node: &ast::Expr) -> Option<ExprKind> {
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
                args: self.args(args),
            };
        }
        if let ast::Expr::Get { object, field, .. } = callee {
            let receiver = self.expr(object);
            return ExprKind::Invoke {
                receiver: Box::new(receiver),
                member: self.member(field),
                args: self.args(args),
            };
        }
        let callee = self.expr(callee);
        ExprKind::Call {
            callee: Box::new(callee),
            args: self.args(args),
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
                Arg::Pos(Single { expr, .. }) => Item::Pos(self.item_value(expr)),
                Arg::Key(Key { key_span, expr, .. }) => {
                    Item::Key(self.symbol(*key_span), self.item_value(expr))
                }
                Arg::DynamicKey(Pair { key, value, .. }) => {
                    let key = self.item_value(key);
                    Item::Pair(key, self.item_value(value))
                }
                Arg::Expand(expand) => Item::Spread(self.item_value(&expand.expr)),
                Arg::For(node) => self.item_for(node, Self::args),
                Arg::If(node) => self.item_if(node, Self::args),
            })
            .collect()
    }

    fn array_items(&mut self, elems: &'u [ArrayElem]) -> Vec<Item> {
        elems
            .iter()
            .map(|elem| match elem {
                ArrayElem::Single(Single { expr, .. }) => Item::Pos(self.item_value(expr)),
                ArrayElem::Expand(expand) => Item::Spread(self.item_value(&expand.expr)),
                ArrayElem::For(node) => self.item_for(node, Self::array_items),
                ArrayElem::If(node) => self.item_if(node, Self::array_items),
            })
            .collect()
    }

    fn dict_items(&mut self, elems: &'u [DictElem]) -> Vec<Item> {
        elems
            .iter()
            .map(|elem| match elem {
                DictElem::Single(Single { expr, .. }) => Item::Pos(self.item_value(expr)),
                DictElem::Key(Key { key_span, expr, .. }) => {
                    Item::Key(self.symbol(*key_span), self.item_value(expr))
                }
                DictElem::Pair(Pair { key, value, .. }) => {
                    let key = self.item_value(key);
                    Item::Pair(key, self.item_value(value))
                }
                DictElem::Expand(expand) => Item::Spread(self.item_value(&expand.expr)),
                DictElem::For(node) => self.item_for(node, Self::dict_items),
                DictElem::If(node) => self.item_if(node, Self::dict_items),
            })
            .collect()
    }

    /// Lower an item's value. In a comprehension's body, a value other than a
    /// constant, lambda or collection goes in a variable, assigned where the body
    /// runs; the others stay in the tree, where the rule's expected type reaches
    /// them. A collection's own items are lowered as the body's are.
    fn item_value(&mut self, node: &'u ast::Expr) -> Expr {
        if self.ctx.hoist
            && matches!(
                node,
                ast::Expr::Array { .. }
                    | ast::Expr::Tuple { .. }
                    | ast::Expr::Record { .. }
                    | ast::Expr::Dict { .. }
            )
        {
            return self.expr(node);
        }
        let hoist = mem::replace(&mut self.ctx.hoist, false);
        let value = self.expr(node);
        self.ctx.hoist = hoist;
        if !hoist
            || matches!(
                value.kind,
                ExprKind::Literal(_) | ExprKind::Float | ExprKind::Bin | ExprKind::Lambda(_)
            )
        {
            return value;
        }
        let span = value.span;
        let var = self.synthetic();
        self.graph().var_mut(var).bottom = true;
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
            self.graph().var_mut(var).bottom = true;
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
        self.end(Terminal::Branch(exit));
        self.switch(exit);
        self.ctx.hoist = hoist;
        Item::For {
            items,
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
            self.end(Terminal::Branch(join));
            self.switch(fallback);
        }
        let mut else_ = match &node.else_branch {
            Some((body, _)) => {
                let frame = self.body_frame(body);
                let items = self.body_items(&frame, &body.elems, items);
                self.end(Terminal::Branch(join));
                self.switch(join);
                items
            }
            None => Vec::new(),
        };
        for (then, span) in arms.into_iter().zip(spans).rev() {
            else_ = vec![Item::If { then, else_, span }];
        }
        self.ctx.hoist = hoist;
        else_.pop().expect("an `if` has a first branch")
    }

    /// A short circuit whose value is used. Its left operand is pushed, and the
    /// result is on the stack at the join.
    fn short_circuit(
        &mut self,
        op: Op,
        left: &'u ast::Expr,
        right: &'u ast::Expr,
        span: Span,
    ) -> Expr {
        let narrowing = self.narrowing(left);
        let value = self.expr(left);
        self.push(value);
        self.emit(Step::Dup);
        let long = self.block();
        let join = self.block();
        let (then, else_, assumes) = match op {
            Op::AmpAmp => (long, join, narrowing.truthy),
            _ => (join, long, narrowing.falsy),
        };
        self.end(Terminal::If {
            cond: expr(ExprKind::Operand, span),
            then,
            else_,
        });
        self.switch(long);
        for assume in assumes {
            self.emit(Step::Assume(assume));
        }
        self.emit(Step::Pop);
        let value = self.expr(right);
        self.push(value);
        self.end(Terminal::Branch(join));
        self.switch(join);
        expr(ExprKind::Operand, span)
    }

    /// Push a value, unless it's already on top of the stack
    pub(super) fn push(&self, value: Expr) {
        if !matches!(value.kind, ExprKind::Operand) {
            self.emit(Step::Push(value));
        }
    }

    /// Branch on a condition to `then` or `else_`, narrowing on each edge
    pub(super) fn cond(&mut self, node: &'u ast::Expr, then: BlockId, else_: BlockId) {
        match node {
            ast::Expr::Group { expr, .. } => self.cond(expr, then, else_),
            ast::Expr::Binary {
                op: Op::AmpAmp,
                exprs,
                ..
            } => {
                let middle = self.block();
                self.cond(&exprs[0], middle, else_);
                self.switch(middle);
                self.cond(&exprs[1], then, else_);
            }
            ast::Expr::Binary {
                op: Op::BarBar,
                exprs,
                ..
            } => {
                let middle = self.block();
                self.cond(&exprs[0], then, middle);
                self.switch(middle);
                self.cond(&exprs[1], then, else_);
            }
            ast::Expr::Unary {
                op: Op::Bang, expr, ..
            } => self.cond(expr, else_, then),
            node => {
                let narrowing = self.narrowing(node);
                let cond = self.expr(node);
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
        landing.terminal = Terminal::Branch(target);
        block
    }

    /// The narrowing a condition gives when it's truthy and when it's falsy
    fn narrowing(&mut self, node: &'u ast::Expr) -> Narrowing {
        match node {
            ast::Expr::Group { expr, .. } => self.narrowing(expr),
            // Only `nil` and `false` are always falsy
            ast::Expr::Ident(ident) => match self.narrowed(ident) {
                Some(var) => Narrowing {
                    truthy: [Literal::Nil, Literal::Bool(false)]
                        .into_iter()
                        .map(|literal| Assume {
                            var,
                            relation: Relation::Exact,
                            negated: true,
                            against: Against::Value(expr(ExprKind::Literal(literal), ident.span)),
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

    /// The variable a narrowing test is about
    fn subject(&self, node: &ast::Expr) -> Option<VarId> {
        match strip(node) {
            ast::Expr::Ident(ident) => self.narrowed(ident),
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
                item: "type"
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
