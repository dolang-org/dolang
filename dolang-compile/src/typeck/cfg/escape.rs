//! Where each interprocedural variable may have escaped, decided before flow.
//!
//! A forward pass over each function's blocks finds, at each block's entry, the
//! variables it owns that a function it created may have captured: a
//! [`Step::Capture`] generates each of the created functions' [`Func::escapes`](super::Func::escapes),
//! and a fresh binding, one that isn't hoisted, kills its variable, since no
//! function has captured the new cell. Reassigning doesn't kill. Paths join by
//! union. The pass doesn't use types, so a path that only types prune, such as
//! one after a call that never returns, still counts.
//!
//! A variable its owner assigns where it may have escaped is [`VarFlags::UNCACHED`]:
//! a function that captures it can't keep a narrowed copy, since the owner may
//! change it between any two of the capturer's steps. Each function then loads
//! the captures it may cache at its entry, a [`Step::Cache`].
//!
//! Edges are those of [`Terminal::successors`], and a block's handler, which a
//! step may raise to from anywhere in it, so everything the block generates
//! reaches it. An `Unpack` or `Next` binds its pattern only on the edge it matches
//! on. A `finally` body's end continues to each target a `Leave` into a `finally`
//! at its depth tags.

use bitvec::{bitbox, boxed::BitBox};
use rustc_hash::FxHashMap;

use super::{BlockId, FuncId, Ir, Origin, Pattern, Step, Tag, Target, Terminal, VarFlags, VarId};

/// Trace under `typeck.escape`
macro_rules! trace {
    ($($arg:tt)*) => {
        dolang_util::debug_eprintln!(topic: "typeck.escape", $($arg)*)
    };
}

/// Runs a block's steps over the escaped bits of its function's variables
struct Walk<'a> {
    ir: &'a Ir,
    func: FuncId,
    /// Each variable's index in its owner's [`Func::vars`](super::Func::vars)
    index: &'a FxHashMap<VarId, usize>,
    bits: BitBox,
    /// Every bit set at some point, which the block's handler receives
    seen: BitBox,
    /// The variables the owner assigned where they may have escaped
    uncached: Vec<VarId>,
}

impl Walk<'_> {
    fn bit(&self, var: VarId) -> Option<usize> {
        let owned = self.ir.var(var).owner == self.func;
        owned.then(|| self.index.get(&var).copied()).flatten()
    }

    /// An assignment by the owner, which doesn't make a fresh cell
    fn assign(&mut self, var: VarId) {
        if let Some(bit) = self.bit(var)
            && self.bits[bit]
        {
            self.uncached.push(var);
        }
    }

    /// A binding, a fresh cell unless it's hoisted
    fn bind(&mut self, var: VarId) {
        if self.ir.var(var).flags.contains(VarFlags::HOISTED) {
            self.assign(var);
        } else if let Some(bit) = self.bit(var) {
            self.bits.set(bit, false);
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        pattern.vars().for_each(|var| self.bind(var));
    }

    fn step(&mut self, step: &Step) {
        match step {
            Step::Let { pattern, .. } => self.pattern(pattern),
            Step::Assign(Target::Var { var, .. }) | Step::Default { var, .. } => self.assign(*var),
            Step::Capture(funcs) => {
                for &func in funcs {
                    for &var in &self.ir.func(func).escapes {
                        if let Some(bit) = self.bit(var) {
                            self.bits.set(bit, true);
                            self.seen.set(bit, true);
                        }
                    }
                }
            }
            Step::Assign(_)
            | Step::Eval(_)
            | Step::Push(_)
            | Step::Dup
            | Step::Pop
            | Step::Assume(_)
            | Step::Cache(_)
            | Step::Escape { .. } => {}
        }
    }
}

impl Ir {
    /// Decide where each variable may have escaped, mark the variables their
    /// owners assign after that [`VarFlags::UNCACHED`], and load at each function's
    /// entry the captures it may cache
    pub(crate) fn settle_escapes(&mut self) {
        let (entries, uncached) = analyze(self);
        for var in uncached {
            let data = &mut self.vars[var.index()];
            if data.flags.contains(VarFlags::INTERPROCEDURAL)
                && !data.flags.contains(VarFlags::UNCACHED)
            {
                data.flags.insert(VarFlags::UNCACHED);
                trace!("uncached {var:?} {:?}", data.origin);
            }
        }
        for (block, entry) in self.blocks.iter_mut().zip(entries) {
            block.escaped = entry;
        }
        for func in &mut self.funcs {
            let cached: Vec<VarId> = func
                .captures
                .iter()
                .copied()
                .filter(|&var| cacheable(&self.vars[var.index()]))
                .collect();
            if !cached.is_empty() {
                self.blocks[func.entry.index()]
                    .steps
                    .insert(0, Step::Cache(cached));
            }
        }
    }
}

/// Each block's escaped bits at its entry, and the variables their owners assign
/// where they may have escaped
fn analyze(ir: &Ir) -> (Vec<BitBox>, Vec<VarId>) {
    let mut index = FxHashMap::default();
    for (_, func) in ir.funcs() {
        for (slot, &var) in func.vars.iter().enumerate() {
            index.insert(var, slot);
        }
    }
    let width = |ir: &Ir, block: BlockId| ir.func(ir.block(block).func).vars.len();
    // Where each function's `finally` bodies end continuing to, by depth
    let mut ends: FxHashMap<(FuncId, u32), Vec<BlockId>> = FxHashMap::default();
    for (_, data) in ir.blocks() {
        if let &Terminal::Leave {
            entry,
            tag: Tag::Goto(target),
        } = &data.terminal
        {
            let depth = ir.block(entry).depth;
            ends.entry((data.func, depth)).or_default().push(target);
        }
    }
    let mut entries: Vec<BitBox> = ir
        .blocks()
        .map(|(id, _)| bitbox![0; width(ir, id)])
        .collect();
    let mut uncached = Vec::new();
    let walk = |block: BlockId, entry: &BitBox| {
        let data = ir.block(block);
        let mut walk = Walk {
            ir,
            func: data.func,
            index: &index,
            bits: entry.clone(),
            seen: entry.clone(),
            uncached: Vec::new(),
        };
        data.steps.iter().for_each(|step| walk.step(step));
        walk
    };
    let mut queued = vec![true; entries.len()];
    let mut queue: Vec<BlockId> = ir.blocks().map(|(id, _)| id).collect();
    queue.reverse();
    while let Some(block) = queue.pop() {
        queued[block.index()] = false;
        let data = ir.block(block);
        let walk = walk(block, &entries[block.index()]);
        let mut flows: Vec<(BlockId, BitBox)> = Vec::new();
        for target in data.terminal.successors() {
            let pattern = match &data.terminal {
                Terminal::Unpack { pattern, then, .. } if *then == target => Some(pattern),
                Terminal::Next { pattern, body, .. } if *body == target => Some(pattern),
                _ => None,
            };
            let mut bits = walk.bits.clone();
            if let Some(pattern) = pattern {
                let mut edge = Walk {
                    bits,
                    seen: bitbox![0; 0],
                    uncached: Vec::new(),
                    ..walk
                };
                edge.pattern(pattern);
                bits = edge.bits;
            }
            flows.push((target, bits));
        }
        if let Terminal::EndFinally = data.terminal {
            let targets = ends.get(&(data.func, data.depth)).into_iter().flatten();
            flows.extend(targets.map(|&target| (target, walk.bits.clone())));
        }
        if let Some(handler) = data.handler {
            flows.push((handler, walk.seen.clone()));
        }
        for (target, bits) in flows {
            let entry = &mut entries[target.index()];
            let before = entry.clone();
            *entry |= &*bits;
            if *entry != before && !queued[target.index()] {
                queued[target.index()] = true;
                queue.push(target);
            }
        }
    }
    // Each assignment where its variable may have escaped, over the final
    // entries
    for (id, data) in ir.blocks() {
        let mut walk = walk(id, &entries[id.index()]);
        match &data.terminal {
            Terminal::Unpack { pattern, .. } | Terminal::Next { pattern, .. } => {
                walk.pattern(pattern)
            }
            _ => {}
        }
        uncached.append(&mut walk.uncached);
    }
    (entries, uncached)
}

/// Whether a function that captures a variable may keep a copy of it: only its
/// owner assigns it, never after it may have escaped, it isn't exported, and it
/// isn't a `do` block's [`Signature`](super::Signature) or a result, which are
/// joined as they're assigned
pub(super) fn cacheable(var: &super::Var) -> bool {
    !var.flags
        .intersects(VarFlags::VOLATILE | VarFlags::EXPORTED | VarFlags::UNCACHED)
        && !matches!(var.origin, Origin::Signature | Origin::Result)
}
