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
        self, Assign, Block, Class, ClassMember, Decorator, Def, FieldInit, For, Function, Ident,
        If, IfBranch, LValue, MemberScope, Param, PatIdent, PatternBind, PrimStmt, Return, Stmt,
        Try, While, visit::Node,
    },
    source::Span,
    typeck::{
        cfg::{
            BlockId, Collection, Expr, ExprKind, FuncId, FuncKind, Item, Origin, Pattern,
            PatternItem, PatternKey, Signature, Step, Tag, Target, Terminal, VarId,
        },
        r#type::DeclId,
    },
};

impl<'u> Scope<'_, '_, 'u> {
    /// A function's parameters and body, lowered from its entry block
    pub(super) fn function(&mut self, func: &'u Function) {
        let frame = self.ctx.frame.clone();
        let items = self.in_frame(&frame, |scope| {
            (func.params.iter())
                .map(|param| scope.pattern_item(param))
                .collect()
        });
        self.graph().func_mut(self.ctx.func).params = Pattern::Unpack(items);
        self.defaults(&func.params, &frame);
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
    /// each item written without an annotation. Its exit joins its result into the
    /// result's variable.
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
                    Param::Pos { ty: Some(_), .. }
                        | Param::Key { ty: Some(_), .. }
                        | Param::ConstKey { ty: Some(_), .. }
                        | Param::Rest { ty: Some(_), .. }
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
            self.reassign(var);
            graph.block_mut(exit).steps.push(Step::Assign {
                target: Target::Var(var),
                value: expr(ExprKind::Var(result), Span::INVALID),
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
            // Imports bind names that lowering resolves itself
            Stmt::Import(_) | Stmt::TypeAlias(_) => self.value_nil(dest, span),
        }
        false
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
            PrimStmt::Try(node) => self.try_(node, dest),
        }
    }

    /// The value of a statement's right-hand side. An `if` or `try` leaves it in a
    /// synthetic variable.
    fn prim_value(&mut self, prim: &'u PrimStmt) -> Expr {
        match prim {
            PrimStmt::Expr(node) => self.expr(node),
            PrimStmt::If(_) | PrimStmt::Try(_) => {
                let var = self.synthetic();
                self.prim(prim, Some(var));
                expr(ExprKind::Var(var), prim.span())
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
        // A destructured value is needed again for `dest`
        let value = match (dest, pattern) {
            (Some(_), ast::Pattern::Unpack(_)) => expr(ExprKind::Var(self.temporary(value)), span),
            _ => value,
        };
        let copy = match value.kind {
            ExprKind::Var(var) => Some(var),
            _ => None,
        };
        let frame = self.ctx.frame.clone();
        let bound_pattern = self.pattern(pattern, &frame);
        let bound = match bound_pattern {
            Pattern::Bind(var) => Some(var),
            Pattern::Unpack(_) => copy,
        };
        self.emit(Step::Let {
            pattern: bound_pattern,
            value,
        });
        self.pattern_defaults(pattern, &frame);
        if let (Some(dest), Some(var)) = (dest, bound) {
            self.assign(dest, expr(ExprKind::Var(var), span));
        }
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
            self.reassign(var);
            self.assign(var, value);
            if let Some(dest) = dest {
                self.assign(dest, expr(ExprKind::Var(var), span));
            }
            return;
        }
        // The target's operands would lie below the value's, so a value that is
        // needed again, or that is lowered to blocks, is bound first
        let early = (dest.is_some() || !matches!(node.rhs, PrimStmt::Expr(_))).then(|| {
            let value = self.prim_value(&node.rhs);
            self.temporary(value)
        });
        let target = match &node.lhs {
            LValue::Ident(_) => unreachable!("assigned above"),
            LValue::Field { object, field, .. } => Target::Field {
                object: self.expr(object),
                member: self.member_key(*field, false, false),
                rule: self.graph().alloc_rule(),
            },
            LValue::PrivateField { object, field, .. } => Target::Field {
                object: self.expr(object),
                member: self.member_key(*field, false, true),
                rule: self.graph().alloc_rule(),
            },
            LValue::Index { exprs, .. } => {
                let object = self.expr(&exprs[0]);
                Target::Index {
                    object,
                    index: self.expr(&exprs[1]),
                    rule: self.graph().alloc_rule(),
                }
            }
        };
        let value = match early {
            Some(var) => expr(ExprKind::Var(var), span),
            None => self.prim_value(&node.rhs),
        };
        self.emit(Step::Assign { target, value });
        if let (Some(dest), Some(var)) = (dest, early) {
            self.assign(dest, expr(ExprKind::Var(var), span));
        }
    }

    /// Lower a pattern in `frame`, where its names are bound
    pub(super) fn pattern(&mut self, pattern: &'u ast::Pattern, frame: &Rc<Frame<'u>>) -> Pattern {
        self.in_frame(frame, |scope| match pattern {
            ast::Pattern::Ident(PatIdent { ident, ty }) => {
                Pattern::Bind(scope.binding(ident, ty.as_deref()))
            }
            ast::Pattern::Unpack(params) => Pattern::Unpack(
                (params.iter())
                    .map(|param| scope.pattern_item(param))
                    .collect(),
            ),
        })
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

    /// Join the defaults of parameters bound in `frame` into their variables. A
    /// default may read the pattern's earlier bindings and captures, so it's a step
    /// after the binding.
    fn defaults(&mut self, params: &'u [Param], frame: &Rc<Frame<'u>>) {
        self.in_frame(frame, |scope| {
            for param in params {
                let (Param::Pos {
                    ident,
                    default: Some(default),
                    ..
                }
                | Param::Key {
                    ident,
                    default: Some(default),
                    ..
                }
                | Param::ConstKey {
                    ident,
                    default: Some(default),
                    ..
                }) = param
                else {
                    continue;
                };
                let Some(var) = scope.var(ident) else {
                    continue;
                };
                let value = scope.expr(&default.expr);
                scope.emit(Step::Default { var, value });
            }
        });
    }

    /// Join the defaults of a pattern bound in `frame`
    fn pattern_defaults(&mut self, pattern: &'u ast::Pattern, frame: &Rc<Frame<'u>>) {
        if let ast::Pattern::Unpack(params) = pattern {
            self.defaults(params, frame);
        }
    }

    /// The target of an edge that binds a pattern: a new block joining its defaults,
    /// on the way to `target`, if it has any
    fn defaulted(
        &mut self,
        pattern: &'u ast::Pattern,
        frame: &Rc<Frame<'u>>,
        target: BlockId,
    ) -> BlockId {
        let ast::Pattern::Unpack(params) = pattern else {
            return target;
        };
        if !params.iter().any(has_default) {
            return target;
        }
        let from = self.bb;
        let block = self.block();
        self.switch(block);
        self.defaults(params, frame);
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

    /// A pattern item, without its default, which [`Self::defaults`] joins after the
    /// pattern binds
    fn pattern_item(&mut self, param: &'u Param) -> PatternItem {
        let (key, var) = match param {
            Param::Pos { ident, ty, .. } => (PatternKey::Pos, self.binding(ident, ty.as_deref())),
            Param::Key {
                key_span,
                ident,
                ty,
                ..
            } => (
                PatternKey::Key(self.symbol(*key_span)),
                self.binding(ident, ty.as_deref()),
            ),
            Param::ConstKey {
                key_expr,
                ident,
                ty,
                ..
            } => {
                let key = PatternKey::ConstKey(self.expr(key_expr));
                (key, self.binding(ident, ty.as_deref()))
            }
            Param::Rest { kind, ident, .. } => {
                return PatternItem {
                    key: PatternKey::Rest(*kind),
                    var: ident.as_ref().and_then(|ident| self.var(ident)),
                };
            }
        };
        PatternItem {
            key,
            var: Some(var),
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
        bind: Option<&'u PatternBind>,
        frame: &Rc<Frame<'u>>,
        then: BlockId,
        else_: BlockId,
    ) {
        let Some(bind) = bind else {
            self.cond(cond, then, else_);
            return;
        };
        let span = cond.span();
        let value = self.expr(cond);
        match &bind.pattern {
            // A name binds the value itself, on its truthiness
            ast::Pattern::Ident(PatIdent { ident, ty }) => {
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
            pattern @ ast::Pattern::Unpack(_) => {
                let bound = self.pattern(pattern, frame);
                let then = self.defaulted(pattern, frame, then);
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
        // An `if` without `else` is `nil`
        let branch_dest = if complete { dest } else { None };
        let join = self.block();
        let branches: Vec<&IfBranch<Block>> = std::iter::once(&node.tbranch)
            .chain(node.elif_branches.iter().map(|(branch, _)| branch))
            .collect();
        for (index, branch) in branches.iter().enumerate() {
            let fallback = if index + 1 < branches.len() || complete {
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
            self.queue_block(body, ctx, &branch.body, branch_dest, join);
            self.switch(fallback);
        }
        if let Some((block, _)) = &node.else_branch {
            let ctx = Ctx {
                frame: self.block_frame(block),
                ..self.ctx.clone()
            };
            self.queue_block(self.bb, ctx, block, branch_dest, join);
        }
        self.switch(join);
        if !complete {
            self.value_nil(dest, node.tbranch.span);
        }
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

    /// The head of a `for` whose body binds in `frame`: the iteratee is bound to
    /// the iterator, and the header takes its next item. Returns the header, body
    /// and exit blocks, with the header ended.
    pub(super) fn next_head<B>(
        &mut self,
        node: &'u For<B>,
        frame: &Rc<Frame<'u>>,
    ) -> (BlockId, BlockId, BlockId) {
        let iter = match self.entry(node.iter) {
            Some(Entry::Var(var)) => var,
            _ => self.synthetic(),
        };
        let value = match &node.expr {
            Some(value) => self.expr(value),
            None => expr(ExprKind::AmbientInput, node.for_span),
        };
        self.emit(Step::Let {
            pattern: Pattern::Bind(iter),
            value,
        });
        let header = self.block();
        let exit = self.block();
        let body = self.block();
        self.end(Terminal::Branch(header));
        self.switch(header);
        let pattern = self.pattern(&node.bind, frame);
        let entry = self.defaulted(&node.bind, frame, body);
        self.end(Terminal::Next {
            iter,
            pattern,
            body: entry,
            exit,
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
                Param::Pos {
                    ident,
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
                let items = self.in_frame(&frame, |scope| {
                    (params.iter())
                        .map(|param| scope.pattern_item(param))
                        .collect()
                });
                let args = ExprKind::Collection {
                    kind: Collection::Tuple,
                    items: vec![Item::Pos(operand)],
                    rule: self.graph().alloc_rule(),
                };
                self.emit(Step::Let {
                    pattern: Pattern::Unpack(items),
                    value: expr(args, span),
                });
                self.defaults(params, &frame);
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
                    rule: self.graph().alloc_rule(),
                };
                expr(call, span)
            })
    }

    fn def(&mut self, def: &'u Def, dest: Option<VarId>) {
        let decl = self.lower.decl(DeclKey::def(def));
        let id = self.function_value(decl, &def.func, false, self.ctx.class);
        let value = expr(ExprKind::Lambda(id), def.ident.span);
        let value = self.decorate(&def.decorators, value);
        self.bind_name(&def.ident, value, dest);
    }

    fn bind_name(&mut self, ident: &Ident, value: Expr, dest: Option<VarId>) {
        let span = ident.span;
        let Some(var) = self.var(ident) else {
            self.emit(Step::Eval(value));
            self.value_nil(dest, span);
            return;
        };
        self.emit(Step::Let {
            pattern: Pattern::Bind(var),
            value,
        });
        if let Some(dest) = dest {
            self.assign(dest, expr(ExprKind::Var(var), span));
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
        self.emit(Step::Let {
            pattern: Pattern::Bind(var),
            value,
        });
        for (member, value) in statics {
            self.emit(Step::Assign {
                target: Target::Field {
                    object: expr(ExprKind::Var(var), span),
                    member,
                    rule: self.graph().alloc_rule(),
                },
                value: expr(ExprKind::Var(value), span),
            });
        }
        if let Some(dest) = dest {
            self.assign(dest, expr(ExprKind::Var(var), span));
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

fn has_default(param: &Param) -> bool {
    matches!(
        param,
        Param::Pos {
            default: Some(_),
            ..
        } | Param::Key {
            default: Some(_),
            ..
        } | Param::ConstKey {
            default: Some(_),
            ..
        }
    )
}
