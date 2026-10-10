//! Jumps: routes through `finally` bodies, and `break`, `continue` and `return`,
//! local or out of a `do` block.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use super::{Ctx, Scope, expr};
use crate::{
    source::Span,
    typeck::cfg::{BlockId, Expr, ExprKind, FuncId, Step, Tag, Target, Terminal},
};

/// A `try` with a `finally`, as the blocks of its body and handlers see it
pub(super) struct Finally {
    /// The first block of the `finally` body
    pub(super) entry: BlockId,
    /// The `finally` enclosing the `try` statement
    pub(super) parent: Option<Rc<Finally>>,
    pub(super) func: FuncId,
    /// The depth of the `try` statement
    pub(super) depth: u32,
    /// The handler outside the `try` statement
    pub(super) handler: Option<BlockId>,
    /// For each target reached from inside, the block that enters the `finally` on
    /// the way to it
    routes: RefCell<HashMap<BlockId, BlockId>>,
}

impl Finally {
    pub(super) fn new(
        entry: BlockId,
        parent: Option<Rc<Finally>>,
        func: FuncId,
        depth: u32,
        handler: Option<BlockId>,
    ) -> Self {
        Self {
            entry,
            parent,
            func,
            depth,
            handler,
            routes: RefCell::new(HashMap::new()),
        }
    }
}

/// The targets of a loop's `break` and `continue`
#[derive(Clone)]
pub(super) struct Loop {
    pub(super) exit: BlockId,
    pub(super) next: BlockId,
    /// The `finally` context the loop statement is in
    pub(super) finally: Option<Rc<Finally>>,
}

fn same(a: Option<&Rc<Finally>>, b: Option<&Rc<Finally>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Scope<'_, '_, '_> {
    /// The block to continue at, from the current `finally` context, to reach
    /// `target` in the context `to`
    pub(super) fn route(&self, target: BlockId, to: Option<&Rc<Finally>>) -> BlockId {
        route(self, self.ctx.finally.as_ref(), target, to)
    }

    /// `break` or `continue`, to the innermost loop
    pub(super) fn loop_jump(&mut self, next: bool) {
        if let Some(target) = &self.ctx.loop_ {
            let block = if next { target.next } else { target.exit };
            let finally = target.finally.clone();
            let target = self.route(block, finally.as_ref());
            self.end(Terminal::Branch(target));
            return;
        }
        // Out of a `do` block: to the loop in scope where it was created
        let mut outer = origin(&self.ctx).cloned();
        while let Some(ctx) = outer {
            if let Some(target) = &ctx.loop_ {
                let block = if next { target.next } else { target.exit };
                let target = route(self, ctx.finally.as_ref(), block, target.finally.as_ref());
                self.phantom(&ctx, target);
                break;
            }
            outer = origin(&ctx).cloned();
        }
        self.escape();
    }

    /// `return`, with its value already lowered
    pub(super) fn return_(&mut self, value: Expr) {
        if !self.ctx.lambda {
            self.local_return(value);
            return;
        }
        // Out of `do` blocks, to the def that encloses them
        let mut outer = origin(&self.ctx).cloned();
        while let Some(ctx) = outer {
            if !ctx.lambda {
                let target = self.phantom_return(&ctx);
                self.phantom(&ctx, target);
                self.end(Terminal::ReturnFrom {
                    func: ctx.func,
                    value,
                });
                return;
            }
            outer = origin(&ctx).cloned();
        }
        // A `do` block outside any def returns from itself
        self.local_return(value);
    }

    fn local_return(&mut self, value: Expr) {
        let (result, exit) = {
            let func = self.graph().func(self.ctx.func);
            (func.result, func.exit)
        };
        self.assign(result, value);
        let target = self.route(exit, None);
        self.end(Terminal::Branch(target));
    }

    fn escape(&mut self) {
        if self.graph().func(self.ctx.func).parent.is_some() {
            self.end(Terminal::Escape);
        } else {
            self.end(Terminal::Unreachable);
        }
    }

    /// Give the guard point `ctx` is in a phantom edge to `target`
    fn phantom(&self, ctx: &Ctx<'_>, target: BlockId) {
        if let Some(guard) = ctx.guard {
            self.lower
                .guards
                .borrow_mut()
                .entry(guard)
                .or_default()
                .push(target);
        }
    }

    /// The block a phantom return from `ctx`'s guard point continues at, which
    /// assigns the result `Never` and leaves for the exit
    fn phantom_return(&self, ctx: &Ctx<'_>) -> BlockId {
        let make = || {
            let graph = self.graph();
            let block = graph.alloc_block(ctx.func, ctx.handler, ctx.depth);
            let (result, exit) = {
                let func = graph.func(ctx.func);
                (func.result, func.exit)
            };
            let target = route(self, ctx.finally.as_ref(), exit, None);
            let mut block_mut = graph.block_mut(block);
            block_mut.steps.push(Step::Assign(Target::Var {
                var: result,
                value: expr(ExprKind::Never, Span::INVALID),
            }));
            block_mut.terminal = Terminal::Branch(target);
            block
        };
        match ctx.guard {
            Some(guard) => {
                if let Some(&block) = self.lower.returns.borrow().get(&guard) {
                    return block;
                }
                let block = make();
                self.lower.returns.borrow_mut().insert(guard, block);
                block
            }
            None => make(),
        }
    }
}

/// The context the function enclosing `ctx` was created in
fn origin<'c, 'u>(ctx: &'c Ctx<'u>) -> Option<&'c Ctx<'u>> {
    let mut frame = &*ctx.frame;
    loop {
        if let Some(origin) = &frame.origin {
            return Some(origin);
        }
        frame = frame.parent()?;
    }
}

/// The block to continue at, from the `finally` context `from`, to reach `target`
/// in the context `to`. Leaving each `finally` on the way goes through a trampoline
/// made once per `try` and target.
fn route(
    scope: &Scope<'_, '_, '_>,
    from: Option<&Rc<Finally>>,
    target: BlockId,
    to: Option<&Rc<Finally>>,
) -> BlockId {
    let Some(finally) = from else {
        return target;
    };
    if same(from, to) {
        return target;
    }
    if let Some(&block) = finally.routes.borrow().get(&target) {
        return block;
    }
    let next = route(scope, finally.parent.as_ref(), target, to);
    let graph = scope.graph();
    let block = graph.alloc_block(finally.func, finally.handler, finally.depth);
    graph.block_mut(block).terminal = Terminal::Leave {
        entry: finally.entry,
        tag: Tag::Goto(next),
    };
    finally.routes.borrow_mut().insert(target, block);
    block
}
