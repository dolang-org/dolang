//! Statements, blocks and functions.

use std::{mem, rc::Rc};

use super::{
    Ctx, End, Job, Scope, expr,
    expr::is_path,
    jump::{Finally, Loop},
    nil,
    scope::{DeclKey, Entry, Frame},
};
use crate::{
    ast::{
        self, Assign, Block, Class, ClassMember, CondPattern, Decorator, Def, FieldInit, For,
        Function, Ident, If, IfBranch, LValue, Match, MemberScope, PatBind, PatIdent, PatItem,
        PrimStmt, Return, Stmt, Try, While, visit::Node,
    },
    source::Span,
    typeck::{
        cfg::{
            Against, Assume, BlockId, Collection, Expr, ExprKind, FuncId, FuncKind, Item, Origin,
            Pattern, PatternItem, PatternKey, Relation, Signature, Step, Tag, Target, Terminal,
            VarId,
        },
        elab::Referent,
        flow::{class_object, function_value},
        r#type::{DeclId, DeclKind, Literal, TypeId, UnitSpan},
    },
};

/// The sub-patterns a pattern leaves to match: for each, the synthetic variable its
/// item binds the value to, its items, and its span
type Nested<'u> = Vec<(VarId, &'u ast::Pattern, Span)>;

impl<'u> Scope<'_, '_, 'u> {
    /// A function's parameters and body, lowered from its entry block
    pub(super) fn function(&mut self, func: &'u Function) {
        let frame = self.ctx.frame.clone();
        let mut nested = Vec::new();
        let items = self.in_frame(&frame, |scope| {
            scope.pattern_items(&func.params, &mut nested)
        });
        self.graph().func_mut(self.ctx.func).params = Pattern::Unpack(items);
        self.nested_lets(nested, &frame);
        self.defaults(&func.params, &frame, false);
        let (result, exit) = {
            let func = self.graph().func(self.ctx.func);
            (func.result, func.exit)
        };
        if self.ctx.lambda {
            self.signature(func, result, exit);
        }
        if let Some(span) = func.stub_span {
            self.assign(result, expr(ExprKind::Never, span));
            self.end(Terminal::Branch(exit));
            return;
        }
        if !self.stmts(&func.body.stmts, Some(result)) {
            self.end(Terminal::Branch(exit));
        }
    }

    /// A `do` block's signature: a variable of its parent's, which it captures, for
    /// each item written without an annotation other than a rest, whose type its
    /// declared schema gives. Its exit joins its result into the result's variable.
    fn signature(&self, func: &Function, result: VarId, exit: BlockId) {
        let graph = self.graph();
        let parent = graph
            .func(self.ctx.func)
            .parent
            .expect("a `do` block is nested");
        let slot = |annotated: bool| {
            (!annotated).then(|| {
                let var = graph.alloc_var(parent, Origin::Signature, None);
                graph.var_mut(var).bottom = true;
                self.capture(var);
                var
            })
        };
        let params = (func.params.iter())
            .map(|param| {
                slot(matches!(
                    param,
                    PatItem::Pos { ty: Some(_), .. }
                        | PatItem::Key { ty: Some(_), .. }
                        | PatItem::ConstKey { ty: Some(_), .. }
                        | PatItem::Rest { .. }
                ))
            })
            .collect();
        let signature = Signature {
            params,
            input: slot(func.input.is_some()),
            output: slot(func.output.is_some()),
            result: slot(func.ret.is_some()),
        };
        if let Some(var) = signature.result {
            self.assigned(var);
            graph.block_mut(exit).steps.push(Step::Assign {
                target: Target::Var(var),
                value: expr(ExprKind::Copy(result), Span::INVALID),
            });
        }
        graph.func_mut(self.ctx.func).signature = Some(signature);
    }

    /// Lower statements in order, the last one's value going to `dest`. Returns
    /// whether control left the block.
    pub(super) fn stmts(&mut self, stmts: &'u [Stmt], dest: Option<VarId>) -> bool {
        for (index, stmt) in stmts.iter().enumerate() {
            let dest = if index + 1 == stmts.len() { dest } else { None };
            if self.stmt(stmt, dest) {
                return true;
            }
        }
        if stmts.is_empty() {
            self.value_nil(dest, Span::INVALID);
        }
        false
    }

    fn stmt(&mut self, stmt: &'u Stmt, dest: Option<VarId>) -> bool {
        let span = stmt.span();
        match stmt {
            Stmt::NlGuard(guard) => {
                // Closures created in the statement may jump to targets here, which
                // are found once the closures are lowered
                let next = self.block();
                self.end(Terminal::Guard {
                    next,
                    targets: Vec::new(),
                });
                let guard_block = self.bb;
                self.lower
                    .guards
                    .borrow_mut()
                    .entry(guard_block)
                    .or_default();
                self.switch(next);
                let outer = self.ctx.guard.replace(guard_block);
                let left = self.stmt(&guard.body, dest);
                self.ctx.guard = outer;
                return left;
            }
            Stmt::Prim(prim) => self.prim(prim, dest),
            Stmt::Let(node) => {
                let value = self.prim_value(&node.rhs);
                self.bind(&node.bind, value, dest);
            }
            Stmt::Bind(node) => {
                let value = self.expr(&node.expr);
                self.bind(&node.bind, value, dest);
            }
            Stmt::Assign(node) => self.assignment(node, dest),
            Stmt::Break(..) => {
                self.loop_jump(false);
                return true;
            }
            Stmt::Continue(..) => {
                self.loop_jump(true);
                return true;
            }
            Stmt::Return(Return { expr: value, .. }) => {
                let value = match value {
                    Some(value) => self.expr(value),
                    None => nil(span),
                };
                self.return_(value);
                return true;
            }
            Stmt::Throw(node) => {
                let value = self.expr(&node.expr);
                self.end(Terminal::Throw(value));
                return true;
            }
            Stmt::While(node) => {
                self.while_(node);
                self.value_nil(dest, span);
            }
            Stmt::For(node) => {
                self.for_(node);
                self.value_nil(dest, span);
            }
            Stmt::Def(def) if def.is_type_only() => self.value_nil(dest, span),
            Stmt::Def(def) => self.def(def, dest),
            Stmt::Class(class) if class.is_protocol() => self.value_nil(dest, span),
            Stmt::Class(class) => self.class(class, dest),
            Stmt::Import(import) => {
                self.import(import);
                self.value_nil(dest, span);
            }
            Stmt::TypeAlias(_) => self.value_nil(dest, span),
        }
        false
    }

    /// Assign each item an import binds the module's export, spanned as the item's
    /// name there. Module imports bind names that lowering resolves itself.
    fn import(&self, import: &'u ast::Import) {
        for element in &import.elements {
            let ast::ImportElement::Items { items, .. } = element else {
                continue;
            };
            for node in items {
                if let Some(Entry::Item {
                    module,
                    item,
                    var: Some(var),
                }) = self.entry(node.bind().res)
                {
                    let value = ExprKind::Import {
                        module: self.lower.module(module),
                        item: Some(self.lower.symbol(item)),
                    };
                    self.assign(var, expr(value, node.item()));
                }
            }
        }
    }

    fn value_nil(&self, dest: Option<VarId>, span: Span) {
        if let Some(dest) = dest {
            self.assign(dest, nil(span));
        }
    }

    fn prim(&mut self, prim: &'u PrimStmt, dest: Option<VarId>) {
        match prim {
            PrimStmt::Expr(node) => {
                let value = self.expr(node);
                match dest {
                    Some(dest) => self.assign(dest, value),
                    None => self.emit(Step::Eval(value)),
                }
            }
            PrimStmt::If(node) => self.if_(node, dest),
            PrimStmt::Match(node) => self.match_(node, dest),
            PrimStmt::Try(node) => self.try_(node, dest),
        }
    }

    /// The value of a statement's right-hand side. An `if`, `match` or `try` leaves it in a
    /// synthetic variable.
    fn prim_value(&mut self, prim: &'u PrimStmt) -> Expr {
        match prim {
            PrimStmt::Expr(node) => self.expr(node),
            PrimStmt::If(_) | PrimStmt::Match(_) | PrimStmt::Try(_) => {
                let var = self.synthetic();
                self.prim(prim, Some(var));
                expr(ExprKind::Copy(var), prim.span())
            }
        }
    }

    /// Bind a synthetic variable to a value
    fn temporary(&self, value: Expr) -> VarId {
        let var = self.synthetic();
        self.emit(Step::Let {
            pattern: Pattern::Bind(var),
            value,
        });
        var
    }

    /// Bind a pattern to a value, whose operands are the only ones on the stack
    fn bind(&mut self, pattern: &'u ast::Pattern, value: Expr, dest: Option<VarId>) {
        let span = value.span;
        if matches!(
            pattern,
            ast::Pattern::TypeTest(_) | ast::Pattern::Constant { .. } | ast::Pattern::Alt(_)
        ) {
            let var = match value.kind {
                ExprKind::Var(var) | ExprKind::Copy(var) => {
                    self.emit(Step::Eval(value));
                    var
                }
                _ => self.temporary(value),
            };
            let frame = self.ctx.frame.clone();
            self.nested_lets(vec![(var, pattern, pattern.span())], &frame);
            self.pattern_defaults(pattern, &frame, false);
            self.value_nil(dest, span);
            return;
        }
        let frame = self.ctx.frame.clone();
        let (bound, nested) = self.pattern(pattern, &frame);
        self.emit(Step::Let {
            pattern: bound,
            value,
        });
        self.nested_lets(nested, &frame);
        self.pattern_defaults(pattern, &frame, false);
        self.value_nil(dest, span);
    }

    fn assignment(&mut self, node: &'u Assign, dest: Option<VarId>) {
        let span = node.equal_span;
        if let LValue::Ident(ident) = &node.lhs {
            let value = self.prim_value(&node.rhs);
            let Some(var) = self.var(ident) else {
                self.emit(Step::Eval(value));
                self.value_nil(dest, span);
                return;
            };
            self.assigned(var);
            self.assign(var, value);
            self.value_nil(dest, span);
            return;
        }
        // The target's operands would lie below the value's, so a value that is
        // lowered to blocks is bound first
        let early = (!matches!(node.rhs, PrimStmt::Expr(_))).then(|| {
            let value = self.prim_value(&node.rhs);
            self.temporary(value)
        });
        let target = match &node.lhs {
            LValue::Ident(_) => unreachable!("assigned above"),
            LValue::Field { object, field, .. } => match self.import_path(object) {
                // A module's member is the module's export
                Some(ExprKind::Import { module, item: None }) => Target::Import {
                    module,
                    item: self.lower.symbol(self.text(*field)),
                    span: object.span() | *field,
                },
                _ => Target::Field {
                    object: self.expr(object),
                    member: self.member_key(*field, false, false),
                },
            },
            LValue::PrivateField { object, field, .. } => Target::Field {
                object: self.expr(object),
                member: self.member_key(*field, false, true),
            },
            LValue::Index { exprs, .. } => {
                let object = self.expr(&exprs[0]);
                Target::Index {
                    object,
                    index: self.expr(&exprs[1]),
                }
            }
        };
        let value = match early {
            Some(var) => expr(ExprKind::Copy(var), node.rhs.span()),
            None => self.prim_value(&node.rhs),
        };
        self.emit(Step::Assign { target, value });
        self.value_nil(dest, span);
    }

    /// Lower a pattern's top level in `frame`, where its names are bound, with the
    /// sub-patterns its items leave to match
    fn pattern(
        &mut self,
        pattern: &'u ast::Pattern,
        frame: &Rc<Frame<'u>>,
    ) -> (Pattern, Nested<'u>) {
        let mut nested = Vec::new();
        let pattern = self.in_frame(frame, |scope| match pattern {
            ast::Pattern::Ident(PatIdent { ident, ty, .. }) => {
                Pattern::Bind(scope.binding(ident, ty.as_deref()))
            }
            ast::Pattern::Unpack(pat_items) => {
                Pattern::Unpack(scope.pattern_items(pat_items, &mut nested))
            }
            ast::Pattern::TypeTest(_) | ast::Pattern::Constant { .. } | ast::Pattern::Alt(_) => {
                let var = scope.synthetic();
                nested.push((var, pattern, pattern.span()));
                Pattern::Bind(var)
            }
        });
        (pattern, nested)
    }

    /// Match sub-patterns, each against the variable its item bound, and theirs in
    /// turn. A mismatch raises.
    fn nested_lets(&mut self, nested: Nested<'u>, frame: &Rc<Frame<'u>>) {
        for (var, pattern, span) in nested {
            if let ast::Pattern::Alt(alt) = pattern {
                let join = self.block();
                let entry = self.alternatives(var, alt, span, frame, None, join);
                self.end(Terminal::Branch(entry));
                self.switch(join);
                continue;
            }
            let pattern = if let ast::Pattern::TypeTest(test) = pattern {
                let class = self.pattern_class(test);
                self.emit(Step::Eval(expr(
                    ExprKind::TypeTest {
                        value: Box::new(expr(ExprKind::Copy(var), span)),
                        class,
                    },
                    span,
                )));
                if let Some(class) = class {
                    self.emit(Step::Assume(Assume {
                        var,
                        relation: Relation::Upper,
                        negated: false,
                        against: Against::Decl(class),
                    }));
                }
                &*test.pattern
            } else {
                pattern
            };
            if let ast::Pattern::Constant {
                value: constant, ..
            } = pattern
            {
                let value = self.pattern_constant(constant, span);
                self.emit(Step::Assume(Assume {
                    var,
                    relation: Relation::Exact,
                    negated: false,
                    against: Against::Value(value),
                }));
                continue;
            }
            let (pattern, inner) = self.pattern(pattern, frame);
            self.emit(Step::Let {
                pattern,
                value: expr(ExprKind::Copy(var), span),
            });
            self.nested_lets(inner, frame);
        }
    }

    /// Use the folded value, including strings with constant interpolations.
    fn pattern_constant(&mut self, value: &ast::Const, span: Span) -> Expr {
        let kind = match value {
            ast::Const::Nil => ExprKind::Literal(Literal::Nil),
            ast::Const::Bool(value) => ExprKind::Literal(Literal::Bool(*value)),
            ast::Const::Int(value) => ExprKind::Literal(Literal::Int(*value)),
            ast::Const::Str(value) => ExprKind::Literal(Literal::Str(value.as_str().into())),
            ast::Const::Sym(symbol) => ExprKind::Literal(Literal::Sym(self.symbol(*symbol))),
            ast::Const::F64(_) => ExprKind::Float,
            ast::Const::Bin(_) => ExprKind::Bin,
            ast::Const::Error => ExprKind::Error,
        };
        expr(kind, span)
    }

    fn pattern_class(&self, test: &ast::TypePattern) -> Option<DeclId> {
        let head = UnitSpan {
            unit: self.lower.unit,
            span: test.class.ident.span,
        };
        match self.lower.tables.referents.get(&head) {
            Some(Referent::Decl(decl))
                if self.lower.tables.decls[decl.index()].kind == DeclKind::Class =>
            {
                Some(*decl)
            }
            _ => None,
        }
    }

    /// Blocks matching sub-patterns in turn, as [`Self::nested_lets`] does, which
    /// continue to `then` once all match, or to `else_` at the first that doesn't.
    /// Returns the first, or `then` if there are none.
    fn nested_tests(
        &mut self,
        nested: Nested<'u>,
        frame: &Rc<Frame<'u>>,
        then: BlockId,
        else_: BlockId,
    ) -> BlockId {
        if nested.is_empty() {
            return then;
        }
        let from = self.bb;
        let entry = self.block();
        self.switch(entry);
        let mut pending: Nested<'u> = nested.into_iter().rev().collect();
        while let Some((var, pattern, span)) = pending.pop() {
            if let ast::Pattern::Alt(alt) = pattern {
                let join = if pending.is_empty() {
                    then
                } else {
                    self.block()
                };
                let entry = self.alternatives(var, alt, span, frame, Some(else_), join);
                self.end(Terminal::Branch(entry));
                if !pending.is_empty() {
                    self.switch(join);
                }
                continue;
            }
            if let ast::Pattern::Constant {
                value: constant, ..
            } = pattern
            {
                let value = self.pattern_constant(constant, span);
                let comparison = self.pattern_constant(constant, span);
                let rejected = self.pattern_constant(constant, span);
                let success = self.block();
                let failure = self.block();
                self.end(Terminal::If {
                    cond: expr(
                        ExprKind::Binary {
                            op: crate::lex::Op::EqEq,
                            operands: Box::new([expr(ExprKind::Copy(var), span), comparison]),
                        },
                        span,
                    ),
                    then: success,
                    else_: failure,
                });
                self.switch(failure);
                self.emit(Step::Assume(Assume {
                    var,
                    relation: Relation::Exact,
                    negated: true,
                    against: Against::Value(rejected),
                }));
                self.end(Terminal::Branch(else_));
                self.switch(success);
                self.emit(Step::Assume(Assume {
                    var,
                    relation: Relation::Exact,
                    negated: false,
                    against: Against::Value(value),
                }));
                if pending.is_empty() {
                    self.end(Terminal::Branch(then));
                }
                continue;
            }
            if let ast::Pattern::TypeTest(test) = pattern {
                let class = self.pattern_class(test);
                let success = self.block();
                let failure = self.block();
                self.end(Terminal::If {
                    cond: expr(
                        ExprKind::TypeTest {
                            value: Box::new(expr(ExprKind::Copy(var), span)),
                            class,
                        },
                        span,
                    ),
                    then: success,
                    else_: failure,
                });
                self.switch(failure);
                if let Some(class) = class {
                    self.emit(Step::Assume(Assume {
                        var,
                        relation: Relation::Upper,
                        negated: true,
                        against: Against::Decl(class),
                    }));
                }
                self.end(Terminal::Branch(else_));
                self.switch(success);
                if let Some(class) = class {
                    self.emit(Step::Assume(Assume {
                        var,
                        relation: Relation::Upper,
                        negated: false,
                        against: Against::Decl(class),
                    }));
                }
                pending.push((var, &test.pattern, span));
                continue;
            }
            let (pattern, inner) = self.pattern(pattern, frame);
            pending.extend(inner.into_iter().rev());
            let next = if pending.is_empty() {
                then
            } else {
                self.block()
            };
            // A name inside a tested pattern captures even a falsy value.
            if matches!(pattern, Pattern::Bind(_)) {
                self.emit(Step::Let {
                    pattern,
                    value: expr(ExprKind::Copy(var), span),
                });
                self.end(Terminal::Branch(next));
            } else {
                self.end(Terminal::Unpack {
                    pattern,
                    value: expr(ExprKind::Copy(var), span),
                    then: next,
                    else_,
                });
            }
            if !pending.is_empty() {
                self.switch(next);
            }
        }
        self.switch(from);
        entry
    }

    /// Blocks matching `var` against alternatives in turn, continuing to `join`
    /// on a match. If none matches, the last continues to `else_`, or raises
    /// without one. Defaults join after the whole pattern. Returns the first.
    fn alternatives(
        &mut self,
        var: VarId,
        alt: &'u ast::Alternation,
        span: Span,
        frame: &Rc<Frame<'u>>,
        else_: Option<BlockId>,
        join: BlockId,
    ) -> BlockId {
        let from = self.bb;
        let mut next = else_;
        for (index, pattern) in alt.alts.iter().enumerate().rev() {
            let entry = match next {
                Some(else_) => self.nested_tests(vec![(var, pattern, span)], frame, join, else_),
                // The last alternative of a plain pattern has no mismatch edge
                None => {
                    debug_assert_eq!(index + 1, alt.alts.len());
                    let entry = self.block();
                    self.switch(entry);
                    self.nested_lets(vec![(var, pattern, span)], frame);
                    self.end(Terminal::Branch(join));
                    entry
                }
            };
            next = Some(entry);
        }
        self.switch(from);
        next.expect("an alternative")
    }

    /// Run `f` in `frame`
    pub(super) fn in_frame<R>(
        &mut self,
        frame: &Rc<Frame<'u>>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let outer = mem::replace(&mut self.ctx.frame, frame.clone());
        let result = f(self);
        self.ctx.frame = outer;
        result
    }

    /// Join the defaults of pattern items bound in `frame` into their variables, and
    /// those of their sub-patterns where they occur. A default may read the
    /// pattern's earlier bindings and captures, so it's a step after the binding.
    fn defaults(&mut self, items: &'u [PatItem], frame: &Rc<Frame<'u>>, may_be_absent: bool) {
        self.in_frame(frame, |scope| {
            for item in items {
                let (PatItem::Pos { bind, default, .. }
                | PatItem::Key { bind, default, .. }
                | PatItem::ConstKey { bind, default, .. }) = item
                else {
                    continue;
                };
                let ident = match bind {
                    PatBind::Ident(ident) => ident,
                    PatBind::Nested { pattern, .. } => {
                        scope.pattern_defaults(
                            pattern,
                            frame,
                            may_be_absent || bind.optional().is_some(),
                        );
                        continue;
                    }
                };
                let (Some(default), Some(var)) = (default, scope.var(ident)) else {
                    continue;
                };
                let value = scope.expr(&default.expr);
                scope.emit(Step::Default { var, value });
            }
        });
    }

    /// Join the defaults of a pattern bound in `frame`. `may_be_absent` says
    /// whether an optional ancestor can leave its value absent, so a collapsed
    /// binding's default can apply. It is syntactic, not a flow presence fact.
    fn pattern_defaults(
        &mut self,
        pattern: &'u ast::Pattern,
        frame: &Rc<Frame<'u>>,
        may_be_absent: bool,
    ) {
        match pattern {
            ast::Pattern::Unpack(items) => self.defaults(items, frame, may_be_absent),
            ast::Pattern::TypeTest(test) => {
                self.pattern_defaults(&test.pattern, frame, may_be_absent)
            }
            ast::Pattern::Ident(PatIdent {
                ident,
                default: Some(default),
                ..
            }) if may_be_absent => {
                self.in_frame(frame, |scope| {
                    if let Some(var) = scope.var(ident) {
                        let value = scope.expr(&default.expr);
                        scope.emit(Step::Default { var, value });
                    }
                });
            }
            // Matching has joined the bindings. Join every alternative's defaults
            // conservatively, after earlier items' defaults; only the first can
            // supply a collapsed default when an optional ancestor is absent.
            ast::Pattern::Alt(alt) => {
                for (index, pattern) in alt.alts.iter().enumerate() {
                    self.pattern_defaults(pattern, frame, may_be_absent && index == 0);
                }
            }
            ast::Pattern::Constant { .. } | ast::Pattern::Ident(_) => {}
        }
    }

    /// The target of an edge that binds a pattern: a new block matching the
    /// sub-patterns `nested` and joining its defaults, on the way to `target`, if
    /// there are any
    fn defaulted(
        &mut self,
        pattern: &'u ast::Pattern,
        nested: Nested<'u>,
        frame: &Rc<Frame<'u>>,
        target: BlockId,
    ) -> BlockId {
        if nested.is_empty() && !pattern_has_default(pattern) {
            return target;
        }
        let from = self.bb;
        let block = self.block();
        self.switch(block);
        self.nested_lets(nested, frame);
        self.pattern_defaults(pattern, frame, false);
        self.end(Terminal::Branch(target));
        self.switch(from);
        block
    }

    /// The variable a pattern binds a name to, or a stand-in if it's unresolved
    fn binding(&mut self, ident: &Ident, annot: Option<&ast::Annot>) -> VarId {
        let var = self.var(ident).unwrap_or_else(|| self.synthetic());
        self.annotate(var, annot);
        var
    }

    /// A pattern's items, adding the sub-patterns they leave to match to `nested`
    fn pattern_items(&mut self, items: &'u [PatItem], nested: &mut Nested<'u>) -> Vec<PatternItem> {
        (items.iter())
            .map(|item| self.pattern_item(item, nested))
            .collect()
    }

    /// The variable a pattern item binds: its name's, or for a sub-pattern, a
    /// synthetic one that the sub-pattern, added to `nested`, matches later
    fn item_binding(
        &mut self,
        bind: &'u PatBind,
        annot: Option<&ast::Annot>,
        nested: &mut Nested<'u>,
    ) -> VarId {
        match bind {
            PatBind::Ident(ident) => self.binding(ident, annot),
            PatBind::Nested { pattern, .. } => {
                let var = self.synthetic();
                nested.push((var, pattern, bind.span()));
                var
            }
        }
    }

    /// A pattern item, without its default, which [`Self::defaults`] joins after the
    /// pattern binds
    fn pattern_item(&mut self, item: &'u PatItem, nested: &mut Nested<'u>) -> PatternItem {
        let (key, var) = match item {
            PatItem::Pos { bind, ty, .. } => (
                PatternKey::Pos,
                self.item_binding(bind, ty.as_deref(), nested),
            ),
            PatItem::Key {
                key_span, bind, ty, ..
            } => (
                PatternKey::Key(self.symbol(*key_span)),
                self.item_binding(bind, ty.as_deref(), nested),
            ),
            PatItem::ConstKey {
                key_expr, bind, ty, ..
            } => {
                let key = PatternKey::ConstKey(self.expr(key_expr));
                (key, self.item_binding(bind, ty.as_deref(), nested))
            }
            PatItem::Rest { kind, ident, .. } => {
                return PatternItem {
                    key: PatternKey::Rest(*kind),
                    var: ident.as_ref().and_then(|ident| self.var(ident)),
                    default: false,
                };
            }
        };
        PatternItem {
            key,
            var: Some(var),
            default: has_default(item),
        }
    }

    /// A new scope for a statement block
    fn block_frame(&self, block: &'u Block) -> Rc<Frame<'u>> {
        self.lower.frame(
            self.ctx.func,
            Some(self.ctx.frame.clone()),
            &block.vars,
            &block.stmts,
            None,
        )
    }

    /// Queue a block's statements to start at `bb`, continuing at `next` in the
    /// current `finally` context
    fn queue_block(
        &self,
        bb: BlockId,
        ctx: Ctx<'u>,
        block: &'u Block,
        dest: Option<VarId>,
        next: BlockId,
    ) {
        let end = End::Goto {
            target: next,
            finally: self.ctx.finally.clone(),
        };
        self.queue(
            bb,
            ctx,
            Job::Block {
                stmts: &block.stmts,
                dest,
                end,
            },
        );
    }

    /// Branch on an `if` or `while` condition, binding its pattern, if it has one,
    /// in `frame` on the success edge
    pub(super) fn test(
        &mut self,
        cond: &'u ast::Expr,
        bind: Option<&'u CondPattern>,
        frame: &Rc<Frame<'u>>,
        then: BlockId,
        else_: BlockId,
    ) {
        let Some(bind) = bind else {
            self.cond(cond, then, else_);
            return;
        };
        let value = self.expr(cond);
        self.test_pattern(value, &bind.pattern, true, frame, then, else_);
    }

    /// Branch on whether `pattern` matches `value`, binding it in `frame` on the
    /// success edge. With `truthy`, a lone name also tests the value's
    /// truthiness, as `if let` does; otherwise it always matches.
    fn test_pattern(
        &mut self,
        value: Expr,
        pattern: &'u ast::Pattern,
        truthy: bool,
        frame: &Rc<Frame<'u>>,
        then: BlockId,
        else_: BlockId,
    ) {
        let span = value.span;
        match pattern {
            ast::Pattern::Ident(PatIdent { ident, ty, .. }) if !truthy => {
                let var = self.in_frame(frame, |scope| scope.binding(ident, ty.as_deref()));
                self.emit(Step::Let {
                    pattern: Pattern::Bind(var),
                    value,
                });
                self.end(Terminal::Branch(then));
            }
            // A name binds the value itself, on its truthiness
            ast::Pattern::Ident(PatIdent { ident, ty, .. }) => {
                self.push(value);
                self.emit(Step::Dup);
                let bound = self.block();
                let failed = self.block();
                self.end(Terminal::If {
                    cond: expr(ExprKind::Operand, span),
                    then: bound,
                    else_: failed,
                });
                let var = self.in_frame(frame, |scope| scope.binding(ident, ty.as_deref()));
                self.switch(bound);
                self.emit(Step::Let {
                    pattern: Pattern::Bind(var),
                    value: expr(ExprKind::Operand, span),
                });
                self.end(Terminal::Branch(then));
                self.switch(failed);
                self.emit(Step::Pop);
                self.end(Terminal::Branch(else_));
            }
            ast::Pattern::TypeTest(_) | ast::Pattern::Constant { .. } | ast::Pattern::Alt(_) => {
                let var = match value.kind {
                    ExprKind::Var(var) | ExprKind::Copy(var) => {
                        self.emit(Step::Eval(value));
                        var
                    }
                    _ => self.temporary(value),
                };
                let then = self.defaulted(pattern, Vec::new(), frame, then);
                let entry = self.nested_tests(vec![(var, pattern, span)], frame, then, else_);
                self.end(Terminal::Branch(entry));
            }
            pattern @ ast::Pattern::Unpack(_) => {
                let (bound, nested) = self.pattern(pattern, frame);
                let then = self.defaulted(pattern, Vec::new(), frame, then);
                let then = self.nested_tests(nested, frame, then, else_);
                self.end(Terminal::Unpack {
                    pattern: bound,
                    value,
                    then,
                    else_,
                });
            }
        }
    }

    fn if_(&mut self, node: &'u If<Block>, dest: Option<VarId>) {
        let complete = node.else_branch.is_some();
        let join = self.block();
        let branches: Vec<&IfBranch<Block>> = std::iter::once(&node.tbranch)
            .chain(node.elif_branches.iter().map(|(branch, _)| branch))
            .collect();
        for (index, branch) in branches.iter().enumerate() {
            let fallback = if index + 1 < branches.len() || complete || dest.is_some() {
                self.block()
            } else {
                join
            };
            let frame = self.block_frame(&branch.body);
            let body = self.block();
            self.test(&branch.expr, branch.bind.as_ref(), &frame, body, fallback);
            let ctx = Ctx {
                frame,
                ..self.ctx.clone()
            };
            self.queue_block(body, ctx, &branch.body, dest, join);
            self.switch(fallback);
        }
        if let Some((block, _)) = &node.else_branch {
            let ctx = Ctx {
                frame: self.block_frame(block),
                ..self.ctx.clone()
            };
            self.queue_block(self.bb, ctx, block, dest, join);
        }
        if !complete && dest.is_some() {
            self.value_nil(dest, node.tbranch.span);
            self.end(Terminal::Branch(join));
        }
        self.switch(join);
    }

    fn match_(&mut self, node: &'u Match, dest: Option<VarId>) {
        let complete = node.else_branch.is_some();
        let join = self.block();
        let span = node.scrutinee.span();
        // Later arms test the same variable, which earlier arms' failed tests narrow
        let value = self.expr(&node.scrutinee);
        let var = match value.kind {
            ExprKind::Var(var) | ExprKind::Copy(var) => {
                self.emit(Step::Eval(value));
                var
            }
            _ => self.temporary(value),
        };
        for (index, arm) in node.arms.iter().enumerate() {
            let fallback = if index + 1 < node.arms.len() || complete || dest.is_some() {
                self.block()
            } else {
                join
            };
            let frame = self.block_frame(&arm.body);
            let body = self.block();
            let entry = if arm.guard.is_some() {
                self.block()
            } else {
                body
            };
            let value = expr(ExprKind::Var(var), span);
            self.test_pattern(value, &arm.pattern, false, &frame, entry, fallback);
            if let Some(guard) = &arm.guard {
                self.switch(entry);
                self.in_frame(&frame, |scope| {
                    scope.test(&guard.expr, guard.bind.as_ref(), &frame, body, fallback)
                });
            }
            let ctx = Ctx {
                frame,
                ..self.ctx.clone()
            };
            self.queue_block(body, ctx, &arm.body, dest, join);
            self.switch(fallback);
        }
        if let Some((block, _)) = &node.else_branch {
            let ctx = Ctx {
                frame: self.block_frame(block),
                ..self.ctx.clone()
            };
            self.queue_block(self.bb, ctx, block, dest, join);
        }
        if !complete && dest.is_some() {
            self.value_nil(dest, node.match_span);
            self.end(Terminal::Branch(join));
        }
        self.switch(join);
    }

    fn loop_ctx(&self, frame: Rc<Frame<'u>>, exit: BlockId, next: BlockId) -> Ctx<'u> {
        Ctx {
            frame,
            loop_: Some(Loop {
                exit,
                next,
                finally: self.ctx.finally.clone(),
            }),
            ..self.ctx.clone()
        }
    }

    fn while_(&mut self, node: &'u While) {
        let header = self.block();
        let exit = self.block();
        let body = self.block();
        self.end(Terminal::Branch(header));
        self.switch(header);
        let frame = self.block_frame(&node.body);
        self.test(&node.expr, node.bind.as_ref(), &frame, body, exit);
        let ctx = self.loop_ctx(frame, exit, header);
        self.queue_block(body, ctx, &node.body, None, header);
        self.switch(exit);
    }

    fn for_(&mut self, node: &'u For<Block>) {
        let frame = self.block_frame(&node.body);
        let (header, body, exit) = self.next_head(node, &frame);
        let ctx = self.loop_ctx(frame, exit, header);
        self.queue_block(body, ctx, &node.body, None, header);
        self.switch(exit);
    }

    /// The head of a `for` whose body binds in `frame`: the iteratee, if any, is
    /// bound to the iterator, and the header takes its next item, or without one,
    /// the ambient input's. Returns the header, body and exit blocks, with the
    /// header ended.
    pub(super) fn next_head<B>(
        &mut self,
        node: &'u For<B>,
        frame: &Rc<Frame<'u>>,
    ) -> (BlockId, BlockId, BlockId) {
        let (iter, span) = match &node.expr {
            Some(value) => {
                let iter = match self.entry(node.iter) {
                    Some(Entry::Var(var)) => var,
                    _ => self.synthetic(),
                };
                let value = self.expr(value);
                let span = value.span;
                self.emit(Step::Let {
                    pattern: Pattern::Bind(iter),
                    value,
                });
                (Some(iter), span)
            }
            None => (None, node.for_span),
        };
        let header = self.block();
        let exit = self.block();
        let body = self.block();
        self.end(Terminal::Branch(header));
        self.switch(header);
        let (pattern, nested) = self.pattern(&node.bind, frame);
        let entry = self.defaulted(&node.bind, nested, frame, body);
        self.end(Terminal::Next {
            iter,
            pattern,
            body: entry,
            exit,
            span,
        });
        (header, body, exit)
    }

    /// A `try`, inlined: its parts are lowered in the enclosing function. The body's
    /// handler dispatches to the catch clauses, and every way out of the body or a
    /// clause enters the `finally`.
    fn try_(&mut self, node: &'u Try, dest: Option<VarId>) {
        let lower = self.lower;
        let graph = &lower.graph;
        let outer = self.ctx.clone();
        let (func, handler, depth) = (outer.func, outer.handler, outer.depth);
        let join = self.block();
        let finally = node.finally.as_ref().map(|(part, _)| {
            let entry = graph.alloc_block(func, handler, depth + 1);
            let ctx = Ctx {
                frame: self.block_frame(&part.body),
                depth: depth + 1,
                ..outer.clone()
            };
            self.queue(
                entry,
                ctx,
                Job::Block {
                    stmts: &part.body.stmts,
                    dest: None,
                    end: End::EndFinally,
                },
            );
            Rc::new(Finally::new(
                entry,
                outer.finally.clone(),
                func,
                depth,
                handler,
            ))
        });
        // Raise the exception on the stack again, running the `finally` first
        let rethrow = || {
            let block = graph.alloc_block(func, handler, depth);
            let mut rethrow = graph.block_mut(block);
            match &finally {
                Some(finally) => {
                    rethrow.steps.push(Step::Pop);
                    rethrow.terminal = Terminal::Leave {
                        entry: finally.entry,
                        tag: Tag::Rethrow,
                    };
                }
                None => rethrow.terminal = Terminal::Throw(expr(ExprKind::Operand, node.try_span)),
            }
            block
        };
        let part_handler = finally.as_ref().map(|_| rethrow()).or(handler);
        let dispatch =
            (!node.handlers.is_empty()).then(|| graph.alloc_block(func, part_handler, depth));
        let part_ctx = |frame, handler| Ctx {
            frame,
            handler,
            finally: finally.clone().or_else(|| outer.finally.clone()),
            ..outer.clone()
        };

        let body = graph.alloc_block(func, dispatch.or(part_handler), depth);
        let ctx = part_ctx(self.block_frame(&node.body.body), dispatch.or(part_handler));
        self.queue_block(body, ctx, &node.body.body, dest, join);
        self.end(Terminal::Branch(body));

        let Some(mut dispatch) = dispatch else {
            self.switch(join);
            return;
        };
        let mut clauses = Vec::new();
        let mut otherwise = None;
        for clause in &node.handlers {
            let block = graph.alloc_block(func, part_handler, depth);
            let ctx = part_ctx(self.block_frame(&clause.func.body), part_handler);
            let body = self.catch_binding(block, &clause.func, ctx.clone());
            self.queue_block(body, ctx, &clause.func.body, dest, join);
            match &clause.class_expr {
                Some(class) => {
                    // A dispatch evaluates its classes as it tries them, so a class
                    // that may need blocks ends it and starts the next one
                    if !is_path(class) && !clauses.is_empty() {
                        let next = graph.alloc_block(func, part_handler, depth);
                        graph.block_mut(dispatch).terminal = Terminal::Catch {
                            clauses: mem::take(&mut clauses),
                            otherwise: next,
                        };
                        dispatch = next;
                    }
                    let from = self.bb;
                    let caller =
                        mem::replace(&mut self.ctx, part_ctx(outer.frame.clone(), part_handler));
                    self.switch(dispatch);
                    let class = self.expr(class);
                    dispatch = self.bb;
                    self.ctx = caller;
                    self.switch(from);
                    clauses.push((class, block));
                }
                // A catch-all clause is tried last, wherever it's written
                None => otherwise = Some(block),
            }
        }
        let otherwise = otherwise.unwrap_or_else(rethrow);
        graph.block_mut(dispatch).terminal = Terminal::Catch { clauses, otherwise };
        self.switch(join);
    }

    /// Bind a catch clause's parameters in `block`, taking the exception on the
    /// stack. Returns the block the clause's body continues in, after any defaults.
    fn catch_binding(&mut self, block: BlockId, clause: &'u Function, ctx: Ctx<'u>) -> BlockId {
        let caller = mem::replace(&mut self.ctx, ctx);
        let from = self.bb;
        self.switch(block);
        let frame = self.ctx.frame.clone();
        let span = clause.span();
        let operand = expr(ExprKind::Operand, span);
        match &clause.params[..] {
            [] => self.emit(Step::Pop),
            [
                PatItem::Pos {
                    bind: PatBind::Ident(ident),
                    ty,
                    default: None,
                },
            ] => {
                let var = self.in_frame(&frame, |scope| scope.binding(ident, ty.as_deref()));
                self.emit(Step::Let {
                    pattern: Pattern::Bind(var),
                    value: operand,
                });
            }
            // The clause is called with the exception as its one argument
            params => {
                let mut nested = Vec::new();
                let items = self.in_frame(&frame, |scope| scope.pattern_items(params, &mut nested));
                let args = ExprKind::Collection {
                    kind: Collection::Tuple,
                    items: vec![Item::Pos(operand)],
                };
                self.emit(Step::Let {
                    pattern: Pattern::Unpack(items),
                    value: expr(args, span),
                });
                self.nested_lets(nested, &frame);
                self.defaults(params, &frame, false);
            }
        }
        let body = self.bb;
        self.switch(from);
        self.ctx = caller;
        body
    }

    /// Create a nested function and queue its body
    pub(super) fn function_value(
        &self,
        decl: DeclId,
        func: &'u Function,
        lambda: bool,
        class: Option<DeclId>,
    ) -> FuncId {
        let graph = self.graph();
        let id = graph.alloc_func(FuncKind::Decl(decl), Some(self.ctx.func));
        let mut block = graph.block_mut(self.bb);
        match block.steps.last_mut() {
            Some(Step::Capture(funcs)) => funcs.push(id),
            _ => block.steps.push(Step::Capture(vec![id])),
        }
        drop(block);
        let frame = self.lower.frame(
            id,
            Some(self.ctx.frame.clone()),
            &func.body.vars,
            &func.body.stmts,
            Some(self.ctx.clone()),
        );
        let entry = graph.func(id).entry;
        self.queue(
            entry,
            Ctx::function(id, lambda, frame, class),
            Job::Function(func),
        );
        id
    }

    /// Apply decorators to a value, the last one written first. The decorators are
    /// evaluated first, in order, as the tree has it.
    fn decorate(&mut self, decorators: &'u [Decorator], value: Expr) -> Expr {
        let decorators: Vec<_> = (decorators.iter())
            .map(|decorator| (self.expr(&decorator.expr), decorator.open_span))
            .collect();
        decorators
            .into_iter()
            .rev()
            .fold(value, |value, (decorator, span)| {
                let call = ExprKind::Call {
                    callee: Box::new(decorator),
                    args: vec![Item::Pos(value)],
                };
                expr(call, span)
            })
    }

    fn def(&mut self, def: &'u Def, dest: Option<VarId>) {
        let decl = self.lower.decl(DeclKey::def(def));
        let id = self.function_value(decl, &def.func, false, self.ctx.class);
        let value = expr(ExprKind::Lambda(id), def.ident.span);
        let value = self.decorate(&def.decorators, value);
        let declared = (def.decorators.is_empty())
            .then(|| {
                self.declared(decl, || {
                    function_value(self.lower.db, self.lower.tables, decl)
                })
            })
            .flatten();
        self.bind_name(&def.ident, value, declared, dest);
    }

    /// The type a def's or class's variable is declared with: its value's, unless
    /// it's lifted over an enclosing declaration's binders, which its value would
    /// need applied. Decorators replace the value, so their caller leaves it out.
    fn declared(&self, decl: DeclId, value: impl FnOnce() -> TypeId) -> Option<TypeId> {
        let lifted = self.lower.tables.lifted.get(&decl);
        lifted.is_none_or(|lifted| lifted.is_empty()).then(value)
    }

    fn bind_name(
        &mut self,
        ident: &Ident,
        value: Expr,
        declared: Option<TypeId>,
        dest: Option<VarId>,
    ) {
        let span = ident.span;
        let Some(var) = self.var(ident) else {
            self.emit(Step::Eval(value));
            self.value_nil(dest, span);
            return;
        };
        if declared.is_some() {
            self.graph().var_mut(var).annotation = declared;
        }
        self.emit(Step::Let {
            pattern: Pattern::Bind(var),
            value,
        });
        if let Some(dest) = dest {
            self.assign(dest, expr(ExprKind::Copy(var), span));
        }
    }

    /// A class statement. Its methods and field initializers are functions nested
    /// here, and its static fields are assigned once the class exists.
    fn class(&mut self, class: &'u Class, dest: Option<VarId>) {
        let decl = self.lower.decl(DeclKey::class(class));
        let span = class.ident.span;
        let mut statics = Vec::new();
        for member in &class.body.members {
            match member {
                ClassMember::Method(method) => {
                    self.decorator_evals(&method.decorators);
                    if !method.type_only {
                        let id = self.lower.decl(DeclKey::method(method));
                        self.function_value(id, &method.func, false, Some(decl));
                    }
                }
                ClassMember::Field(field) => {
                    self.decorator_evals(&field.decorators);
                    match &field.init {
                        FieldInit::Thunk(func) => {
                            let id = self.lower.decl(DeclKey::closure(func));
                            self.function_value(id, func, false, Some(decl));
                        }
                        FieldInit::Expr(value) | FieldInit::Const(value, _)
                            if field.scope == MemberScope::Static =>
                        {
                            let value = self.expr(value);
                            let var = self.temporary(value);
                            let private = field.pub_span.is_none();
                            for name in &field.fields {
                                let mut member = self.member_key(name.ident.span, false, private);
                                member.class = private.then_some(decl);
                                statics.push((member, var));
                            }
                        }
                        // Instance and class field defaults are checked with `(init)`
                        FieldInit::Expr(_) | FieldInit::Const(..) | FieldInit::None => {}
                    }
                }
            }
        }
        let value = expr(ExprKind::Class(decl), span);
        let value = self.decorate(&class.decorators, value);
        let Some(var) = self.var(&class.ident) else {
            self.emit(Step::Eval(value));
            self.value_nil(dest, span);
            return;
        };
        if class.decorators.is_empty()
            && let Some(ty) = self.declared(decl, || class_object(self.lower.db, decl))
        {
            self.graph().var_mut(var).annotation = Some(ty);
        }
        self.emit(Step::Let {
            pattern: Pattern::Bind(var),
            value,
        });
        for (member, value) in statics {
            self.emit(Step::Assign {
                target: Target::Field {
                    object: expr(ExprKind::Copy(var), span),
                    member,
                },
                value: expr(ExprKind::Copy(value), span),
            });
        }
        if let Some(dest) = dest {
            self.assign(dest, expr(ExprKind::Copy(var), span));
        }
    }

    /// Evaluate member decorators, which only annotate their member
    fn decorator_evals(&mut self, decorators: &'u [Decorator]) {
        for decorator in decorators {
            let value = self.expr(&decorator.expr);
            self.emit(Step::Eval(value));
        }
    }
}

/// Whether any item has a default, at any level
fn pattern_has_default(pattern: &ast::Pattern) -> bool {
    match pattern {
        ast::Pattern::Constant { .. } => false,
        ast::Pattern::Ident(ident) => ident.default.is_some(),
        ast::Pattern::Unpack(items) => any_default(items),
        ast::Pattern::TypeTest(test) => pattern_has_default(&test.pattern),
        ast::Pattern::Alt(alt) => alt.alts.iter().any(pattern_has_default),
    }
}

fn any_default(items: &[PatItem]) -> bool {
    items.iter().any(|item| match item {
        PatItem::Pos { bind, default, .. }
        | PatItem::Key { bind, default, .. }
        | PatItem::ConstKey { bind, default, .. } => {
            default.is_some()
                || matches!(bind, PatBind::Nested { pattern, .. }
                    if pattern_has_default(pattern))
        }
        PatItem::Rest { .. } => false,
    })
}

fn has_default(item: &PatItem) -> bool {
    match item {
        PatItem::Pos { bind, default, .. }
        | PatItem::Key { bind, default, .. }
        | PatItem::ConstKey { bind, default, .. } => default.is_some() || bind.optional().is_some(),
        PatItem::Rest { .. } => false,
    }
}
