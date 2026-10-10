//! Structural validation of a finished graph. Stack depths are checked by flow
//! analysis, which knows each block's tag stack.

#[cfg(any(test, debug_assertions))]
use std::collections::HashMap;

use super::{
    Against, BlockId, Expr, ExprKind, FuncId, FuncKind, Ir, Origin, Pattern, Step, Tag, Target,
    Terminal, VarId,
};

/// Why a graph is malformed
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    /// An edge or handler leads to another function's block
    ForeignEdge { from: BlockId, to: BlockId },
    /// An edge within a function goes to a block inside a `finally` body the source
    /// isn't in, other than by entering it
    Deeper { from: BlockId, to: BlockId },
    /// A `Leave` enters a block that isn't directly inside the source's `finally`
    /// bodies, or its tag continues to a block inside one the source isn't in
    LeaveDepth { from: BlockId, to: BlockId },
    /// `EndFinally` outside any `finally`
    EndOutside(BlockId),
    /// `Return` other than in its function's exit block, or an exit block that
    /// doesn't return
    Return(BlockId),
    /// A non-local `break`, `continue` or `return` that doesn't leave for an
    /// enclosing function
    NonLocal(BlockId),
    /// A variable used by a function that neither owns it nor captures it from an
    /// enclosing one
    Var { func: FuncId, var: VarId },
    /// A parameter owned by another function
    Param { func: FuncId, var: VarId },
    /// A signature on a function other than a nested closure, with a variable its
    /// parent doesn't own or it doesn't capture, or with an entry per parameter
    /// that isn't one per pattern item
    Signature(FuncId),
    /// A closure instantiated other than directly in its parent
    Lambda { func: FuncId, lambda: FuncId },
    /// A nested function that isn't named once, by `Capture` steps in its
    /// parent's blocks
    Capture(FuncId),
    /// An escaping variable its function's parent doesn't own, or that's joined
    /// as it's assigned
    Escape { func: FuncId, var: VarId },
}

impl Ir {
    /// Check the graph's structure.
    ///
    /// # Errors
    ///
    /// The first defect found.
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        for (id, func) in self.funcs() {
            for var in func.params.vars() {
                if self.var(var).owner != id {
                    return Err(Invalid::Param { func: id, var });
                }
            }
            if !matches!(self.block(func.exit).terminal, Terminal::Return) {
                return Err(Invalid::Return(func.exit));
            }
            for &var in &func.escapes {
                let data = self.var(var);
                if Some(data.owner) != func.parent
                    || data.volatile
                    || data.origin == Origin::Signature
                {
                    return Err(Invalid::Escape { func: id, var });
                }
            }
            if let Some(signature) = &func.signature {
                let items = match &func.params {
                    Pattern::Unpack(items) => items.len(),
                    Pattern::Bind(_) => usize::MAX,
                };
                let valid = matches!(func.kind, FuncKind::Decl(_))
                    && func.parent.is_some_and(|parent| {
                        signature.vars().all(|var| {
                            self.var(var).owner == parent && func.captures.contains(&var)
                        })
                    })
                    && signature.params.len() == items;
                if !valid {
                    return Err(Invalid::Signature(id));
                }
            }
        }
        let mut created = vec![0usize; self.funcs().count()];
        for (_, block) in self.blocks() {
            for step in &block.steps {
                let Step::Capture(funcs) = step else {
                    continue;
                };
                for &func in funcs {
                    if self.func(func).parent != Some(block.func) {
                        return Err(Invalid::Capture(func));
                    }
                    created[func.index()] += 1;
                }
            }
        }
        for (id, func) in self.funcs() {
            if func.parent.is_some() && created[id.index()] != 1 {
                return Err(Invalid::Capture(id));
            }
        }
        for (id, block) in self.blocks() {
            let check = Check {
                ir: self,
                func: block.func,
            };
            let mut exprs = Vec::new();
            let mut vars = Vec::new();
            for step in &block.steps {
                match step {
                    Step::Let { pattern, value } => {
                        pattern.walk(&mut |expr| exprs.push(expr));
                        vars.extend(pattern.vars());
                        exprs.push(value);
                    }
                    Step::Assign { target, value } => {
                        match target {
                            Target::Var(var) => vars.push(*var),
                            Target::Field { object, .. } => exprs.push(object),
                            Target::Index { object, index, .. } => exprs.extend([object, index]),
                            Target::Import { .. } => {}
                        }
                        exprs.push(value);
                    }
                    Step::Default { var, value } => {
                        vars.push(*var);
                        exprs.push(value);
                    }
                    Step::Eval(expr) | Step::Push(expr) => exprs.push(expr),
                    Step::Dup | Step::Pop | Step::Capture(_) => {}
                    Step::Assume(assume) => {
                        vars.push(assume.var);
                        match &assume.against {
                            Against::Class(expr) | Against::Value(expr) => exprs.push(expr),
                            Against::Type(_) | Against::Decl(_) => {}
                        }
                    }
                }
            }
            match &block.terminal {
                Terminal::If { cond, .. } => exprs.push(cond),
                Terminal::Unpack { pattern, value, .. } => {
                    pattern.walk(&mut |expr| exprs.push(expr));
                    vars.extend(pattern.vars());
                    exprs.push(value);
                }
                Terminal::Catch { clauses, .. } => {
                    exprs.extend(clauses.iter().map(|(expr, _)| expr))
                }
                Terminal::Next { iter, pattern, .. } => {
                    vars.extend(*iter);
                    pattern.walk(&mut |expr| exprs.push(expr));
                    vars.extend(pattern.vars());
                }
                Terminal::Throw(expr) => exprs.push(expr),
                Terminal::Return => {
                    if self.func(block.func).exit != id {
                        return Err(Invalid::Return(id));
                    }
                }
                Terminal::EndFinally => {
                    if block.depth == 0 {
                        return Err(Invalid::EndOutside(id));
                    }
                }
                Terminal::Escape => {
                    if self.func(block.func).parent.is_none() {
                        return Err(Invalid::NonLocal(id));
                    }
                }
                Terminal::ReturnFrom { func, value } => {
                    if !self.encloses(*func, block.func) {
                        return Err(Invalid::NonLocal(id));
                    }
                    exprs.push(value);
                }
                Terminal::Branch(_)
                | Terminal::Leave { .. }
                | Terminal::Guard { .. }
                | Terminal::Unreachable => {}
            }
            let mut nested = Vec::new();
            for expr in exprs {
                expr.walk(&mut |expr| nested.push(expr));
            }
            for expr in nested {
                check.expr(expr, &mut vars)?;
            }
            for var in vars {
                check.var(var)?;
            }
            check.edges(id)?;
        }
        Ok(())
    }

    /// Whether `ancestor` strictly encloses `func`
    fn encloses(&self, ancestor: FuncId, func: FuncId) -> bool {
        let mut current = self.func(func).parent;
        while let Some(parent) = current {
            if parent == ancestor {
                return true;
            }
            current = self.func(parent).parent;
        }
        false
    }
}

struct Check<'a> {
    ir: &'a Ir,
    func: FuncId,
}

impl Check<'_> {
    fn expr(&self, expr: &Expr, vars: &mut Vec<VarId>) -> Result<(), Invalid> {
        match &expr.kind {
            ExprKind::Var(var) | ExprKind::Copy(var) => vars.push(*var),
            ExprKind::Lambda(lambda) if self.ir.func(*lambda).parent != Some(self.func) => {
                return Err(Invalid::Lambda {
                    func: self.func,
                    lambda: *lambda,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn var(&self, var: VarId) -> Result<(), Invalid> {
        let owner = self.ir.var(var).owner;
        let func = self.ir.func(self.func);
        if owner == self.func || self.ir.encloses(owner, self.func) && func.captures.contains(&var)
        {
            return Ok(());
        }
        Err(Invalid::Var {
            func: self.func,
            var,
        })
    }

    fn edges(&self, from: BlockId) -> Result<(), Invalid> {
        let ir = self.ir;
        let block = ir.block(from);
        if let Some(handler) = block.handler {
            let target = ir.block(handler);
            if target.func != block.func {
                return Err(Invalid::ForeignEdge { from, to: handler });
            }
            if target.depth > block.depth {
                return Err(Invalid::Deeper { from, to: handler });
            }
        }
        if let Terminal::Leave {
            tag: Tag::Goto(to), ..
        } = block.terminal
        {
            let target = ir.block(to);
            if target.func != block.func {
                return Err(Invalid::ForeignEdge { from, to });
            }
            if target.depth > block.depth {
                return Err(Invalid::LeaveDepth { from, to });
            }
        }
        for to in block.terminal.successors() {
            let target = ir.block(to);
            if target.func != block.func {
                return Err(Invalid::ForeignEdge { from, to });
            }
            match block.terminal {
                Terminal::Leave { .. } if target.depth != block.depth + 1 => {
                    return Err(Invalid::LeaveDepth { from, to });
                }
                Terminal::Leave { .. } => {}
                _ if target.depth > block.depth => return Err(Invalid::Deeper { from, to }),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(any(test, debug_assertions))]
fn operands(expr: &Expr) -> usize {
    let mut count = 0;
    expr.walk(&mut |expr| count += matches!(expr.kind, ExprKind::Operand) as usize);
    count
}

#[cfg(any(test, debug_assertions))]
fn pattern_operands(pattern: &Pattern) -> usize {
    let mut count = 0;
    pattern.walk(&mut |expr| count += operands(expr));
    count
}

#[cfg(any(test, debug_assertions))]
impl Ir {
    /// Check that each block is entered at one stack depth, that nothing pops more than
    /// is there, and that a statement boundary a jump or `finally` leaves from has an
    /// empty stack. A handler is entered with the exception alone on the stack.
    pub(crate) fn check_stack_depths(&self) {
        let ir = self;
        struct Walk {
            depths: HashMap<BlockId, usize>,
            work: Vec<BlockId>,
        }
        impl Walk {
            fn enter(&mut self, block: BlockId, depth: usize) {
                match self.depths.insert(block, depth) {
                    Some(old) => assert_eq!(old, depth, "b{} entered at two depths", block.index()),
                    None => self.work.push(block),
                }
            }
        }
        let mut walk = Walk {
            depths: HashMap::new(),
            work: Vec::new(),
        };
        for (_, func) in ir.funcs() {
            walk.enter(func.entry, 0);
        }
        while let Some(id) = walk.work.pop() {
            let block = ir.block(id);
            let mut depth = walk.depths[&id];
            let mut enter = |block, depth| walk.enter(block, depth);
            let pop = |depth: &mut usize, count: usize| {
                *depth = depth
                    .checked_sub(count)
                    .unwrap_or_else(|| panic!("b{} pops an empty stack", id.index()));
            };
            if let Some(handler) = block.handler {
                enter(handler, 1);
            }
            for step in &block.steps {
                match step {
                    Step::Let { pattern, value } => {
                        pop(&mut depth, operands(value) + pattern_operands(pattern))
                    }
                    Step::Assign { target, value } => {
                        let target = match target {
                            Target::Var(_) | Target::Import { .. } => 0,
                            Target::Field { object, .. } => operands(object),
                            Target::Index { object, index, .. } => {
                                operands(object) + operands(index)
                            }
                        };
                        pop(&mut depth, target + operands(value));
                    }
                    Step::Default { value, .. } | Step::Eval(value) => {
                        pop(&mut depth, operands(value))
                    }
                    Step::Push(value) => {
                        pop(&mut depth, operands(value));
                        depth += 1;
                    }
                    Step::Dup => {
                        assert!(depth > 0, "b{} duplicates an empty stack", id.index());
                        depth += 1;
                    }
                    Step::Pop => pop(&mut depth, 1),
                    Step::Capture(_) => {}
                    Step::Assume(assume) => match &assume.against {
                        Against::Class(expr) | Against::Value(expr) => {
                            assert_eq!(operands(expr), 0)
                        }
                        Against::Type(_) | Against::Decl(_) => {}
                    },
                }
            }
            let empty = |depth: usize| assert_eq!(depth, 0, "b{} leaves with a stack", id.index());
            match &block.terminal {
                Terminal::Branch(next) => enter(*next, depth),
                Terminal::If { cond, then, else_ } => {
                    pop(&mut depth, operands(cond));
                    enter(*then, depth);
                    enter(*else_, depth);
                }
                Terminal::Unpack {
                    pattern,
                    value,
                    then,
                    else_,
                } => {
                    pop(&mut depth, operands(value) + pattern_operands(pattern));
                    enter(*then, depth);
                    enter(*else_, depth);
                }
                Terminal::Catch { clauses, otherwise } => {
                    for (class, _) in clauses {
                        pop(&mut depth, operands(class));
                    }
                    assert_eq!(depth, 1, "b{} dispatches the exception alone", id.index());
                    for (_, clause) in clauses {
                        enter(*clause, depth);
                    }
                    enter(*otherwise, depth);
                }
                Terminal::Next {
                    pattern,
                    body,
                    exit,
                    ..
                } => {
                    // A comprehension's loop may run above a command's earlier arguments
                    pop(&mut depth, pattern_operands(pattern));
                    enter(*body, depth);
                    enter(*exit, depth);
                }
                Terminal::Throw(value) | Terminal::ReturnFrom { value, .. } => {
                    pop(&mut depth, operands(value))
                }
                Terminal::Leave { entry, tag } => {
                    empty(depth);
                    enter(*entry, 0);
                    if let Tag::Goto(next) = tag {
                        enter(*next, 0);
                    }
                }
                Terminal::Guard { next, targets } => {
                    empty(depth);
                    enter(*next, 0);
                    for target in targets {
                        enter(*target, 0);
                    }
                }
                Terminal::Return | Terminal::EndFinally => empty(depth),
                Terminal::Escape | Terminal::Unreachable => {}
            }
        }
    }
}
