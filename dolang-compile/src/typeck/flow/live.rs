//! Which of a function's slots each block may read before writing them.
//!
//! A slot dead at a block's entry holds a value no later step observes, so
//! [`Flow::merge`](super::Flow) forgets it there rather than joining it, and a
//! block isn't queued again just because a dead value grew. A read is any step
//! that consults a slot: a variable reference, a narrowing, a default, a capture
//! or an `(init)` escape. A write kills, except that assigning an ivar that isn't
//! volatile depends on whether it may have escaped, which the slot records, so it
//! reads the slot too. A fresh binding starts a cell that hasn't escaped, so it
//! doesn't; a hoisted binding isn't fresh, so it does.
//!
//! Edges are those of [`Terminal::successors`], and a block's handler, which a
//! step may raise to from anywhere in it, so everything live there is live at the
//! block's entry. An `Unpack` or `Next` binds its pattern only on the edge it
//! matches on. A `finally` body's end continues to each target a `Leave` into a
//! `finally` at its depth tags, which joins the contexts of every such body in the
//! function.

use std::cmp::Reverse;

use bitvec::{bitbox, boxed::BitBox};
use rustc_hash::FxHashMap;

use super::Frame;
use crate::typeck::cfg::{
    Against, BlockId, Expr, ExprKind, FuncId, FuncKind, Ir, Pattern, Step, Tag, Target, Terminal,
    VarFlags, VarId,
};

/// What a block or edge does to its function's slots, in order
struct Transfer {
    /// Read before any write
    uses: BitBox,
    /// Written
    defs: BitBox,
}

/// Collects a transfer in step order
struct Collect<'a> {
    ir: &'a Ir,
    slots: &'a FxHashMap<(FuncId, VarId), usize>,
    func: FuncId,
    transfer: Transfer,
}

impl Collect<'_> {
    fn slot(&self, var: VarId) -> Option<usize> {
        self.slots.get(&(self.func, var)).copied()
    }

    fn read(&mut self, var: VarId) {
        if let Some(slot) = self.slot(var)
            && !self.transfer.defs[slot]
        {
            self.transfer.uses.set(slot, true);
        }
    }

    fn write(&mut self, var: VarId) {
        if let Some(slot) = self.slot(var) {
            self.transfer.defs.set(slot, true);
        }
    }

    /// An assignment, which reads whether an ivar may have escaped
    fn assign(&mut self, var: VarId) {
        let flags = self.ir.var(var).flags;
        if flags.contains(VarFlags::INTERPROCEDURAL) && !flags.contains(VarFlags::VOLATILE) {
            self.read(var);
        }
        self.write(var);
    }

    /// A binding, a fresh cell unless it's hoisted
    fn bind(&mut self, var: VarId) {
        match self.ir.var(var).flags.contains(VarFlags::HOISTED) {
            true => self.assign(var),
            false => self.write(var),
        }
    }

    fn expr(&mut self, expr: &Expr) {
        expr.walk(&mut |expr| match expr.kind {
            ExprKind::Var(var) | ExprKind::Copy(var) => self.read(var),
            _ => {}
        });
    }

    /// A pattern's constant keys, then its bindings
    fn pattern(&mut self, pattern: &Pattern) {
        pattern.walk(&mut |expr| self.expr(expr));
        for var in pattern.vars() {
            self.bind(var);
        }
    }

    fn step(&mut self, step: &Step) {
        match step {
            Step::Let { pattern, value } => {
                self.expr(value);
                self.pattern(pattern);
            }
            Step::Assign(target) => match target {
                Target::Var { var, value } => {
                    self.expr(value);
                    self.assign(*var);
                }
                Target::Field { object, value, .. } => {
                    self.expr(object);
                    self.expr(value.pos());
                }
                Target::Index { object, args } => {
                    self.expr(object);
                    args.iter().for_each(|arg| self.expr(arg.pos()));
                }
                Target::Import { value, .. } => self.expr(value),
            },
            Step::Default { var, value } => {
                self.expr(value);
                self.read(*var);
                self.write(*var);
            }
            Step::Eval(value) | Step::Push(value) => self.expr(value),
            Step::Dup | Step::Pop => {}
            Step::Assume(assume) => {
                match &assume.against {
                    Against::Class(expr) | Against::Value(expr) => self.expr(expr),
                    Against::Decl(_) | Against::Type(_) | Against::Shape(_) => {}
                }
                self.read(assume.var);
            }
            Step::Capture(funcs) => {
                for &func in funcs {
                    for &var in &self.ir.func(func).escapes {
                        self.read(var);
                    }
                }
            }
            Step::Cache(vars) => vars.iter().for_each(|&var| self.write(var)),
            &Step::Escape { supers, .. } => {
                let func = self.ir.func(self.func);
                let supers = (func.supers.iter()).filter(|_| supers);
                for &var in func.fields.iter().chain(supers) {
                    self.read(var);
                }
            }
        }
    }

    fn terminal(&mut self, terminal: &Terminal) {
        match terminal {
            Terminal::If { cond: value, .. }
            | Terminal::Unpack { value, .. }
            | Terminal::Throw(value)
            | Terminal::ReturnFrom { value, .. } => self.expr(value),
            Terminal::Catch { clauses, .. } => {
                clauses.iter().for_each(|(class, _)| self.expr(class))
            }
            Terminal::Next { iter, .. } => iter.iter().for_each(|&var| self.read(var)),
            Terminal::Branch { .. }
            | Terminal::Return
            | Terminal::ElseFallthrough { .. }
            | Terminal::Leave { .. }
            | Terminal::EndFinally
            | Terminal::Guard { .. }
            | Terminal::Escape(_)
            | Terminal::Unreachable => {}
        }
    }
}

/// Each block's live slots at its entry, by block, each over its function's
/// slots. `rank` orders the sweeps, which converge faster backward.
pub(super) fn live(
    ir: &Ir,
    funcs: &[Frame],
    slots: &FxHashMap<(FuncId, VarId), usize>,
    rank: &[u32],
) -> Box<[BitBox]> {
    let width = |block: BlockId| funcs[ir.block(block).func.index()].vars.len();
    let collect = |func: FuncId| Collect {
        ir,
        slots,
        func,
        transfer: Transfer {
            uses: bitbox![0; funcs[func.index()].vars.len()],
            defs: bitbox![0; funcs[func.index()].vars.len()],
        },
    };
    // Each block's own transfer, and by block, the target and bindings of the edge
    // its pattern matches on
    let mut blocks = Vec::new();
    let mut edges: FxHashMap<BlockId, (BlockId, Transfer)> = FxHashMap::default();
    // Where each function's `finally` bodies end continuing to, by depth
    let mut ends: FxHashMap<(FuncId, u32), Vec<BlockId>> = FxHashMap::default();
    for (id, data) in ir.blocks() {
        let func = ir.func(data.func);
        let mut block = collect(data.func);
        if id == func.entry && matches!(func.kind, FuncKind::Decl(_)) {
            let Pattern::Unpack { items, .. } = &func.params else {
                unreachable!("parameters are a pattern of items")
            };
            items
                .iter()
                .filter_map(|item| item.var)
                .for_each(|var| block.bind(var));
        }
        if id == func.exit {
            block.read(func.result);
        }
        data.steps.iter().for_each(|step| block.step(step));
        block.terminal(&data.terminal);
        blocks.push(block.transfer);
        match &data.terminal {
            Terminal::Unpack { pattern, then, .. } => {
                let mut edge = collect(data.func);
                edge.pattern(pattern);
                edges.insert(id, (*then, edge.transfer));
            }
            Terminal::Next { pattern, body, .. } => {
                let mut edge = collect(data.func);
                edge.pattern(pattern);
                edges.insert(id, (*body, edge.transfer));
            }
            &Terminal::Leave {
                entry,
                tag: Tag::Goto(target),
            } => {
                let depth = ir.block(entry).depth;
                ends.entry((data.func, depth)).or_default().push(target);
            }
            _ => {}
        }
    }
    let mut order: Vec<BlockId> = ir.blocks().map(|(id, _)| id).collect();
    order.sort_by_key(|block| Reverse(rank[block.index()]));
    let mut live: Vec<BitBox> = (ir.blocks()).map(|(id, _)| bitbox![0; width(id)]).collect();
    let union = |into: &mut BitBox, from: &BitBox| {
        from.iter_ones().for_each(|slot| into.set(slot, true));
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &block in &order {
            let data = ir.block(block);
            let mut out = bitbox![0; width(block)];
            let edge = edges.get(&block);
            for target in data.terminal.successors() {
                match edge {
                    Some((bound, edge)) if *bound == target => {
                        let mut through = live[target.index()].clone();
                        edge.defs
                            .iter_ones()
                            .for_each(|slot| through.set(slot, false));
                        union(&mut through, &edge.uses);
                        union(&mut out, &through);
                    }
                    _ => union(&mut out, &live[target.index()]),
                }
            }
            if let Terminal::EndFinally = data.terminal {
                let targets = ends.get(&(data.func, data.depth)).into_iter().flatten();
                for target in targets {
                    union(&mut out, &live[target.index()]);
                }
            }
            let transfer = &blocks[block.index()];
            transfer
                .defs
                .iter_ones()
                .for_each(|slot| out.set(slot, false));
            union(&mut out, &transfer.uses);
            if let Some(handler) = data.handler {
                union(&mut out, &live[handler.index()]);
            }
            if out != live[block.index()] {
                live[block.index()] = out;
                changed = true;
            }
        }
    }
    live.into()
}
