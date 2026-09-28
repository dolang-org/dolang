//! Structural validation of a finished graph. Stack depths are checked by flow
//! analysis, which knows each block's tag stack.

use std::collections::HashSet;

use super::{
    Against, BlockId, Expr, ExprKind, FuncId, FuncKind, Ir, Pattern, RuleId, Step, Tag, Target,
    Terminal, VarId,
};

/// Why a graph is malformed
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    /// An edge or handler leads to another function's block
    ForeignEdge {
        from: BlockId,
        to: BlockId,
    },
    /// An edge within a function goes to a block inside a `finally` body the source
    /// isn't in, other than by entering it
    Deeper {
        from: BlockId,
        to: BlockId,
    },
    /// A `Leave` enters a block that isn't directly inside the source's `finally`
    /// bodies, or its tag continues to a block inside one the source isn't in
    LeaveDepth {
        from: BlockId,
        to: BlockId,
    },
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
    Var {
        func: FuncId,
        var: VarId,
    },
    /// A parameter owned by another function
    Param {
        func: FuncId,
        var: VarId,
    },
    /// A signature on a function other than a nested closure, with a variable its
    /// parent doesn't own or it doesn't capture, or with an entry per parameter
    /// that isn't one per pattern item
    Signature(FuncId),
    /// A closure instantiated other than directly in its parent
    Lambda {
        func: FuncId,
        lambda: FuncId,
    },
    Rule(RuleId),
}

impl Ir {
    /// Check the graph's structure.
    ///
    /// # Errors
    ///
    /// The first defect found.
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        let mut rules = HashSet::new();
        for (id, func) in self.funcs() {
            for var in func.params.vars() {
                if self.var(var).owner != id {
                    return Err(Invalid::Param { func: id, var });
                }
            }
            if !matches!(self.block(func.exit).terminal, Terminal::Return) {
                return Err(Invalid::Return(func.exit));
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
                            Target::Field { object, rule, .. } => {
                                exprs.push(object);
                                check.rule(&mut rules, *rule)?;
                            }
                            Target::Index {
                                object,
                                index,
                                rule,
                            } => {
                                exprs.extend([object, index]);
                                check.rule(&mut rules, *rule)?;
                            }
                        }
                        exprs.push(value);
                    }
                    Step::Default { var, value } => {
                        vars.push(*var);
                        exprs.push(value);
                    }
                    Step::Eval(expr) | Step::Push(expr) => exprs.push(expr),
                    Step::Dup | Step::Pop => {}
                    Step::Assume(assume) => {
                        vars.push(assume.var);
                        match &assume.against {
                            Against::Class(expr) | Against::Value(expr) => exprs.push(expr),
                            Against::Type(_) => {}
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
                    vars.push(*iter);
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
                if let Some(rule) = expr.rule() {
                    check.rule(&mut rules, rule)?;
                }
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
    fn rule(&self, rules: &mut HashSet<RuleId>, rule: RuleId) -> Result<(), Invalid> {
        if rule.index() >= self.ir.rules() || !rules.insert(rule) {
            return Err(Invalid::Rule(rule));
        }
        Ok(())
    }

    fn expr(&self, expr: &Expr, vars: &mut Vec<VarId>) -> Result<(), Invalid> {
        match &expr.kind {
            ExprKind::Var(var) => vars.push(*var),
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
