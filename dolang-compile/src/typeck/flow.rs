//! Type flow analysis of a unit's typing CFG.
//!
//! The analysis finds, at each point of the graph, the type each variable may hold
//! and whether it may still be unassigned, iterating to a fixed point over one work
//! queue for the whole region. A block's state is stored per context: the stack of
//! `finally` tags it was entered with, so a `finally` entered normally and one
//! entered to rethrow are never joined. States only grow. They join where control
//! merges, and where an edge retreats in the queue's order the join widens once it
//! has grown too often; an annotated local widens to its annotation instead.
//!
//! An assignment is a strong update along its path, and a literal assigned to a
//! declared local decays to its class when the class fits the local's annotation.
//! A default keeps its variable's annotation when it fits, and a `nil` or symbol
//! literal that doesn't is a sentinel joined into the variable's type.
//! Narrowing applies a condition's relations to the variable it tests, making the
//! edge unreachable when nothing is left. A branch on a duplicated stack entry
//! also narrows the entry it copied, which a short circuit leaves as its result.
//!
//! State shared between functions isn't flow-sensitive. An ivar joins into an
//! accumulator, which every function but its owner reads it as. Its owner caches
//! its type in its state, and narrows it there, unless it's volatile: assigned by
//! another function, whose calls could change it at any time. A volatile
//! variable's owner reads the accumulator too, and keeps only whether it may be
//! unassigned. Every assignment to a volatile variable joins its accumulator. Any
//! other ivar joins what its owner holds when a function it escapes through is
//! created, at a [`Step::Capture`], and each assignment on a path after one: no
//! function that reads it can run before. A binding other than a `def`'s or
//! `class`'s, which are hoisted, makes a fresh cell that no function has captured
//! yet, so it starts a new path before any capture. A non-local return joins its
//! value into its def's result the same way, as does a `do` block's signature:
//! what the calls it's passed to give its parameters and channels, and its
//! result. A block that reads a joined type depends on it, and is queued again
//! when it grows.
//!
//! A function also caches each ivar it captures that isn't volatile, loading it
//! from the accumulator at its entry ([`Step::Cache`]), and narrows it there,
//! until the owner assigns it on a path where it may have escaped. A spawned
//! function may run between any two of its owner's steps, so only a variable
//! that doesn't change once it escapes keeps what a test showed. That assignment
//! thaws the variable: the entries that loaded it run again and mark their
//! copies, and every read after goes to the accumulator.
//!
//! A step that can throw joins its state before it into its handler, with the
//! exception alone on the stack. A step or terminal with a call that gives bottom
//! ends its path there, after reaching its handler. The call may never return,
//! or what it gives may not be known yet: its callee, an argument, or its rule's
//! decision. Either way nothing after it is known to run, and the block runs
//! again when what it waits on grows.
//!
//! Calls, collection literals, binary strings, interpolations, `for` items and
//! unpacking patterns are checking rules, each solved by a solver of its own (see
//! [`rule`]). Rules that look up members give the dynamic type. When the queue
//! empties, each function's earliest block whose latest run left a rule undecided
//! runs again, once, defaulting the rules it leaves undecided, and iteration
//! resumes, in rounds until no block has one. A rule with a check it couldn't
//! resolve counts as undecided. Once none is left, the `do` block parameters and
//! channels that nothing gave anything become dynamic, which may start more
//! rounds. Then a final pass runs every block once more over its final state,
//! defaulting any rule it leaves undecided, to record what each variable reference
//! and binding saw and to report: contradicted rules, values that don't fit an
//! annotation or a declared result, and reads that may be unassigned. A block in a
//! `finally` is judged once per context, and a problem at a span is reported once.
//!
//! The final pass also warns about the code a call decided never to return makes
//! unreachable. A call that is all of a statement, as its root expression or a
//! result's value, ends its path as `throw` would, so the warning goes to the next
//! step or terminal of its block that has a span, if any. Otherwise what's
//! unreachable is the rest of the step or terminal the call is in.

/// Trace under `typeck.flow`
macro_rules! trace {
    ($($arg:tt)*) => {
        dolang_util::debug_eprintln!(topic: "typeck.flow", $($arg)*)
    };
}

mod eval;
mod live;
mod member;
mod problem;
mod rule;
mod shown;
mod state;
#[cfg(test)]
mod tests;
#[cfg(feature = "debug")]
mod trace;

use std::{
    collections::{BTreeSet, VecDeque},
    mem,
};

use bitvec::{bitbox, boxed::BitBox};
pub(crate) use eval::{class_object, function_value};
use problem::MemberUse;
pub(crate) use problem::Problem;
use rustc_hash::{FxHashMap, FxHashSet};
pub(crate) use state::Fact;
use state::{Contexts, CtxId, Group, Link, May, Operand, State, VarState};

use super::{
    cfg::{
        Against, Assume, BlockId, EscapeKind, Expr, ExprKind, FuncId, FuncKind, Ir, Origin,
        Pattern, PatternItem, PatternKey, Step, Tag, Target, Terminal, VarFlags, VarId,
    },
    elab::{Designated, Tables},
    solver::{Gas, NarrowTarget, Outcome, Provenance, Residual, Solver, Status, Widening},
    r#type::{
        Argument, Database, DeclId, Element, Function, Intrinsic, Kind, Literal, Multiplicity,
        SchemaItem, Type, TypeId, UnitId,
    },
};
use crate::{RestKind, source::Span};

/// What flow concluded about a unit
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(crate) struct Results {
    /// What each variable reference read and each binding bound, joined over every
    /// context, by span
    pub(crate) facts: FxHashMap<Span, Fact>,
    /// What the reporting pass diagnosed, each once
    pub(crate) problems: Vec<Problem>,
    /// The checks it couldn't decide, each once
    pub(crate) unresolved: Vec<(Span, Residual)>,
}

/// Analyze a unit's graph
pub(crate) fn analyze(ir: &Ir, db: &Database, tables: &Tables<'_>) -> Results {
    Flow::new(ir, db, tables, false).analyze()
}

/// The type a variable is read as by functions that don't cache it, which every
/// value that reaches them joins
struct Accumulator {
    ty: TypeId,
    widening: Widening,
    /// The blocks that read it, queued again when it grows or thaws
    readers: FxHashSet<(BlockId, CtxId)>,
    /// Whether its owner assigned it after it escaped, so that the functions that
    /// cache it read it here instead
    thawed: bool,
}

/// What flow knows of a function
struct Frame {
    /// Its declared type, under its rigids, if it's a function type
    declared: Option<Function>,
    /// The variable in each slot of its states: those it owns, then those it
    /// caches
    vars: Box<[VarId]>,
}

/// Where each block falls in the queue
struct Blocks {
    /// Each block's place in the queue: its reverse postorder index
    rank: Box<[u32]>,
    /// The blocks an edge that retreats in that order targets: the widening points
    widens: BitBox,
}

/// A block's state in a context
struct Entry {
    state: State,
    /// Each slot's widening state where the block is a widening point, and
    /// otherwise none
    widenings: Box<[Widening]>,
}

/// A call that gave bottom
#[derive(Clone, Copy)]
struct Divergence {
    span: Span,
    /// Whether it's decided never to return, rather than not known yet to
    decided: bool,
}

/// Where a block is being analyzed
#[derive(Clone, Copy)]
struct At {
    block: BlockId,
    ctx: CtxId,
    func: FuncId,
}

struct Flow<'a, 'u> {
    ir: &'a Ir,
    db: &'a Database,
    tables: &'a Tables<'u>,
    unit: UnitId,
    /// By function
    funcs: Box<[Frame]>,
    blocks: Blocks,
    /// By block, the slots of its function live at its entry
    live: Box<[BitBox]>,
    /// Each variable's slot in a function's states
    slots: FxHashMap<(FuncId, VarId), usize>,
    /// The accumulator of each ivar, and of each result a non-local return gives
    /// a value, once a value joins it or a block reads it
    accumulators: FxHashMap<VarId, Accumulator>,
    /// The function of the running block
    current: Option<FuncId>,
    /// The declarations whose rigids states may hold, which every solver assumes,
    /// each with the rigids its group is checked under
    scope: Vec<(DeclId, Vec<TypeId>)>,
    /// Each checked module's unit, by name
    modules: FxHashMap<&'u str, UnitId>,
    contexts: Contexts,
    states: FxHashMap<(BlockId, CtxId), Entry>,
    /// Deduplicating priority queue ordered by reverse-postorder rank; both
    /// forward and reversed traversal need its order.
    queue: BTreeSet<(u32, BlockId, CtxId)>,
    /// Take the queue's last entry rather than its first, to show that the result
    /// doesn't depend on the order
    reversed: bool,
    /// The blocks whose latest run left a rule undecided
    unsettled: FxHashSet<(BlockId, CtxId)>,
    /// The blocks whose next run defaults the rules it leaves undecided
    marked: FxHashSet<(BlockId, CtxId)>,
    /// Whether the running block defaults the rules it leaves undecided
    defaulting: bool,
    /// The first call in the running step or terminal that gave bottom, which
    /// ends its path
    diverged: Option<Divergence>,
    /// Filled by the final pass, which leaves states alone
    results: Option<Results>,
}

impl<'a, 'u> Flow<'a, 'u> {
    fn new(ir: &'a Ir, db: &'a Database, tables: &'a Tables<'u>, reversed: bool) -> Self {
        let blocks = order(ir);
        let mut scope = Vec::new();
        let mut slots = FxHashMap::default();
        let mut funcs = Vec::new();
        for (id, func) in ir.funcs() {
            let cached = (ir.block(func.entry).steps.first()).and_then(|step| match step {
                Step::Cache(vars) => Some(&vars[..]),
                _ => None,
            });
            let vars: Box<[VarId]> = (func.vars.iter())
                .chain(cached.into_iter().flatten())
                .copied()
                .collect();
            for (slot, &var) in vars.iter().enumerate() {
                slots.insert((id, var), slot);
            }
            funcs.push(Frame {
                declared: match func.kind {
                    FuncKind::Decl(decl) => declared_function(db, tables, decl),
                    FuncKind::Module(_) | FuncKind::DefaultInit(_) => None,
                },
                vars,
            });
            if let Some(decl) = func.kind.scope() {
                let key = (decl, tables.primary_sig(decl));
                for binder in tables.groups.get(&key).into_iter().flatten() {
                    let key = (binder.decl, binder.sig);
                    let owner = tables.sig_decls[&key];
                    if !scope.iter().any(|(decl, _)| *decl == owner) {
                        scope.push((owner, tables.group_rigids(db, key)));
                    }
                }
            }
        }
        let unit = (ir.funcs())
            .find_map(|(_, func)| match func.kind {
                FuncKind::Module(unit) => Some(unit),
                FuncKind::Decl(_) | FuncKind::DefaultInit(_) => None,
            })
            .expect("a unit's graph has its module function");
        let live = live::live(ir, &funcs, &slots, &blocks.rank);
        let modules = (tables.units.iter().enumerate())
            .filter_map(|(index, unit)| Some((unit.module?, UnitId::from_index(index))))
            .collect();
        Self {
            ir,
            db,
            tables,
            unit,
            funcs: funcs.into(),
            blocks,
            live,
            slots,
            accumulators: FxHashMap::default(),
            current: None,
            scope,
            modules,
            contexts: Contexts::new(),
            states: FxHashMap::default(),
            queue: BTreeSet::new(),
            reversed,
            unsettled: FxHashSet::default(),
            marked: FxHashSet::default(),
            defaulting: false,
            diverged: None,
            results: None,
        }
    }

    /// Iterate to a fixed point in rounds, then record what each reference and
    /// binding saw, and report
    fn analyze(mut self) -> Results {
        let bottom = self.db.bottom();
        // What an importer may assign an exported variable is already joined
        for (_, func) in self.ir.funcs() {
            for &var in &func.vars {
                let data = self.ir.var(var);
                if data.flags.contains(VarFlags::EXPORTED) {
                    let ty = data.annotation.unwrap_or(self.db.unknown());
                    self.accumulator(var).ty = ty;
                }
            }
        }
        for (id, func) in self.ir.funcs() {
            // A cached capture is loaded before it's read
            let vars = (self.funcs[id.index()].vars.iter())
                .map(|&var| {
                    let data = self.ir.var(var);
                    VarState {
                        ty: bottom,
                        may: if data.owner != id || data.flags.contains(VarFlags::BOTTOM) {
                            May::empty()
                        } else {
                            May::UNASSIGNED
                        },
                        link: None,
                    }
                })
                .collect();
            let state = State {
                vars,
                stack: Vec::new(),
                dup: false,
                groups: vec![Group::default(); func.groups as usize],
            };
            self.merge(func.entry, CtxId::ROOT, state);
        }
        loop {
            loop {
                let next = if self.reversed {
                    self.queue.pop_last()
                } else {
                    self.queue.pop_first()
                };
                let Some((_, block, ctx)) = next else {
                    break;
                };
                self.run(block, ctx);
            }
            // Upstream first: in each function, its earliest block with an undecided
            // rule. A later rule in that block runs after its inputs.
            let mut earliest: FxHashMap<FuncId, u32> = FxHashMap::default();
            for &(block, _) in &self.unsettled {
                let rank = self.blocks.rank[block.index()];
                let entry = earliest.entry(self.ir.block(block).func).or_insert(rank);
                *entry = (*entry).min(rank);
            }
            if earliest.is_empty() {
                // Nothing gave these `do` block parameters and channels anything
                if self.dynamic_signatures() {
                    continue;
                }
                break;
            }
            let marked: Vec<_> = (self.unsettled.iter().copied())
                .filter(|&(block, _)| {
                    earliest.get(&self.ir.block(block).func)
                        == Some(&self.blocks.rank[block.index()])
                })
                .collect();
            trace!(
                "defaulting round: {}",
                (marked.iter())
                    .map(|&(block, ctx)| self.place(block, ctx))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            for (block, ctx) in marked {
                self.marked.insert((block, ctx));
                self.enqueue(block, ctx);
            }
        }
        trace!("final pass");
        self.results = Some(Results::default());
        let mut keys: Vec<_> = self.states.keys().copied().collect();
        keys.sort_by_key(|&(block, ctx)| (self.blocks.rank[block.index()], block, ctx));
        for (block, ctx) in keys {
            self.run(block, ctx);
        }
        let mut results = self.results.take().expect("recorded by the final pass");
        results.problems.sort_by_key(|problem| {
            let span = crate::typeck::report::Report::span(problem);
            (span.start, span.end)
        });
        results
            .unresolved
            .sort_by_key(|&(span, _)| (span.start, span.end));
        results
    }

    /// Whether the unit is strict, so the checker doesn't make up `Unknown` in it
    fn strict(&self) -> bool {
        self.tables.units[self.unit.index()].strict
    }

    /// Where the checker can't type a construct: a strict unit records the check
    /// as undecided. Either way, `Unknown` stands for the result.
    fn gap(&mut self, span: Span, reason: &'static str) -> TypeId {
        if self.strict() {
            self.undecided(span, Residual::Unsupported(reason));
        }
        self.db.unknown()
    }

    /// A solver that holds the region's rigids as its assumptions, its work
    /// charged to `gas`
    fn solver<'g>(&self, gas: &'g Gas) -> Solver<'g>
    where
        'a: 'g,
    {
        let mut solver = Solver::new(self.db, gas);
        for (decl, rigids) in &self.scope {
            solver.assume_group(*decl, rigids.clone());
        }
        if !self.strict() {
            solver.gradual();
        }
        #[cfg(feature = "debug")]
        {
            solver.named(self.tables, crate::typeck::r#type::Style::Full);
        }
        solver
    }

    fn lub(&self, a: TypeId, b: TypeId) -> TypeId {
        let bottom = self.db.bottom();
        if a == b || b == bottom {
            a
        } else if a == bottom {
            b
        } else {
            self.solver(&Gas::default()).lub(a, b)
        }
    }

    /// Relate two closed types
    fn relate(&self, actual: TypeId, expected: TypeId) -> Outcome {
        let gas = Gas::default();
        let mut solver = self.solver(&gas);
        solver.constrain(
            solver.closed(actual),
            solver.closed(expected),
            Provenance::default(),
        );
        solver.solve().remove(0)
    }

    /// Whether `actual` can be shown to be a subtype of `expected`
    fn below(&self, actual: TypeId, expected: TypeId) -> bool {
        self.relate(actual, expected).status == Status::Proven
    }

    /// Report a problem, once
    fn problem(&mut self, problem: Problem) {
        let Some(results) = &mut self.results else {
            return;
        };
        if crate::typeck::report::Report::span(&problem) != Span::INVALID
            && !results.problems.contains(&problem)
        {
            results.problems.push(problem);
        }
    }

    /// Record a check that couldn't be decided, once
    fn undecided(&mut self, span: Span, residual: Residual) {
        let Some(results) = &mut self.results else {
            return;
        };
        if span != Span::INVALID && !results.unresolved.contains(&(span, residual)) {
            results.unresolved.push((span, residual));
        }
    }

    /// Check a value against a variable's annotation, or a function's declared
    /// result, when reporting
    fn check(&mut self, var: VarId, ty: TypeId, span: Span) {
        if !self.observing() || span == Span::INVALID || ty == self.db.bottom() {
            return;
        }
        let (annotation, result) = match self.ir.var(var).annotation {
            Some(annotation) => (annotation, false),
            None => match self.result_annotation(var) {
                Some(annotation) => (annotation, true),
                None => return,
            },
        };
        if self.conform(ty, annotation, span) == Status::Contradicted {
            let (found, annotation) = self.pair(ty, annotation);
            self.problem(Problem::Annotation {
                span,
                found,
                annotation,
                result,
            });
        }
    }

    /// Relate a value at `span` to the type it's checked against, recording the check
    /// if it can't be decided. A contradiction is the caller's to report.
    fn conform(&mut self, ty: TypeId, expected: TypeId, span: Span) -> Status {
        let outcome = self.relate(ty, expected);
        if outcome.status == Status::Unresolved {
            self.undecided(span, residual(&outcome));
        }
        outcome.status
    }

    /// The declared result a function's result variable is checked against: a
    /// def's, or a `do` block's written one
    fn result_annotation(&self, var: VarId) -> Option<TypeId> {
        let data = self.ir.var(var);
        if data.origin != Origin::Result {
            return None;
        }
        let func = self.ir.func(data.owner);
        if func
            .signature
            .as_ref()
            .is_some_and(|sig| sig.result.is_some())
        {
            return None;
        }
        let result = self.funcs[data.owner.index()].declared.as_ref()?.result;
        (result != self.db.unknown()).then_some(result)
    }

    /// The ambient channels a function's calls pass, when it declares them. A `do`
    /// block's omitted channels are inferred like its parameters, as what the
    /// calls it's passed to give them, which its block then depends on.
    fn channels(&mut self, at: At) -> (Option<TypeId>, Option<TypeId>) {
        let Some(declared) = self.funcs[at.func.index()].declared.clone() else {
            return (None, None);
        };
        let signature = self.ir.func(at.func).signature.as_ref();
        let (input, output) = signature.map_or((None, None), |sig| (sig.input, sig.output));
        let mut channel = |var: Option<VarId>, declared| match var {
            Some(var) => Some(self.joined(var, at)),
            None => declared,
        };
        (
            channel(input, declared.input),
            channel(output, declared.output),
        )
    }

    fn observing(&self) -> bool {
        self.results.is_some()
    }

    fn enqueue(&mut self, block: BlockId, ctx: CtxId) {
        if !self.observing() {
            self.queue
                .insert((self.blocks.rank[block.index()], block, ctx));
        }
    }

    /// Record what a reference or binding at `span` saw
    fn record(&mut self, span: Span, fact: Fact) {
        if span == Span::INVALID {
            return;
        }
        let Some(results) = &self.results else {
            return;
        };
        let fact = match results.facts.get(&span) {
            Some(old) => Fact {
                ty: self.lub(old.ty, fact.ty),
                unassigned: old.unassigned || fact.unassigned,
            },
            None => fact,
        };
        if let Some(results) = &mut self.results {
            results.facts.insert(span, fact);
        }
    }

    /// Continue to `target` from a block in `ctx`, leaving any `finally` bodies the
    /// target isn't in
    fn flow(&mut self, ctx: CtxId, target: BlockId, state: State) {
        let depth = self.ir.block(target).depth;
        let ctx = self.contexts.truncate(ctx, depth);
        self.merge(target, ctx, state);
    }

    /// Join a state into a block's, queueing the block if it grew. What's dead at
    /// its entry is forgotten first, so it doesn't grow with values nothing reads.
    fn merge(&mut self, block: BlockId, ctx: CtxId, mut state: State) {
        if self.observing() {
            return;
        }
        self.mask(block, &mut state);
        let key = (block, ctx);
        let Some(entry) = self.states.get_mut(&key) else {
            trace!(
                "enter {}: {}",
                self.place(block, ctx),
                self.render_state(block, &state)
            );
            let widenings = match self.blocks.widens[block.index()] {
                true => vec![Widening::default(); state.vars.len()].into(),
                false => Box::default(),
            };
            self.states.insert(key, Entry { state, widenings });
            self.enqueue(block, ctx);
            return;
        };
        let old = entry.state.clone();
        let mut widenings = mem::take(&mut entry.widenings);
        assert_eq!(
            old.stack.len(),
            state.stack.len(),
            "b{} is entered at two stack depths",
            block.index()
        );
        let func = self.ir.block(block).func;
        let mut vars = Vec::with_capacity(old.vars.len());
        for (slot, (old, new)) in old.vars.iter().zip(&state.vars).enumerate() {
            let ty = if old.ty == new.ty {
                old.ty
            } else if let Some(widening) = widenings.get_mut(slot) {
                let var = self.funcs[func.index()].vars[slot];
                self.widen(key, var, widening, old.ty, new.ty)
            } else {
                self.lub(old.ty, new.ty)
            };
            vars.push(VarState {
                ty,
                may: old.may | new.may,
                link: self.same((old.ty, old.link), (new.ty, new.link)),
            });
        }
        let stack = (old.stack.iter().zip(&state.stack))
            .map(|(old, new)| Operand {
                ty: self.lub(old.ty, new.ty),
                link: self.same((old.ty, old.link), (new.ty, new.link)),
            })
            .collect();
        let groups = (old.groups.iter().zip(&state.groups))
            .map(|(old, new)| self.join_groups(old, new))
            .collect();
        let mut joined = State {
            vars,
            stack,
            dup: old.dup && state.dup,
            groups,
        };
        // A join can drop the links that kept a group
        Self::unlinked_groups(&mut joined);
        let grew = joined != old;
        if grew {
            trace!(
                "grow {}: {}",
                self.place(block, ctx),
                self.render_state(block, &joined)
            );
        }
        let entry = self.states.get_mut(&key).expect("entered above");
        entry.widenings = widenings;
        if grew {
            entry.state = joined;
            self.enqueue(block, ctx);
        }
    }

    /// Reset each slot dead at a block's entry, and each group nothing left links to
    fn mask(&self, block: BlockId, state: &mut State) {
        let live = &self.live[block.index()];
        let bottom = self.db.bottom();
        for (slot, var) in state.vars.iter_mut().enumerate() {
            if !live[slot] {
                *var = VarState {
                    ty: bottom,
                    may: May::empty(),
                    link: None,
                };
            }
        }
        Self::unlinked_groups(state);
    }

    /// Reset each group that no slot or operand links to, which nothing can
    /// correlate through until an unpack replaces it
    fn unlinked_groups(state: &mut State) {
        let mut linked = vec![false; state.groups.len()];
        let links = (state.vars.iter().map(|var| var.link))
            .chain(state.stack.iter().map(|operand| operand.link))
            .flatten();
        for link in links {
            linked[link.group.index()] = true;
        }
        for (group, linked) in state.groups.iter_mut().zip(linked) {
            if !linked {
                *group = Group::default();
            }
        }
    }

    /// The link two paths' values keep where they join: the one both have, or
    /// one's where the other holds no value
    fn same(&self, old: (TypeId, Option<Link>), new: (TypeId, Option<Link>)) -> Option<Link> {
        let bottom = self.db.bottom();
        match (old, new) {
            ((_, old), (_, new)) if old == new => old,
            ((ty, _), (_, new)) if ty == bottom => new,
            ((_, old), (ty, _)) if ty == bottom => old,
            _ => None,
        }
    }

    /// Join two states' versions of a group entrywise, a column only one has being
    /// bottom in the other. Wherever the linked values lie together in a column
    /// on either side, they lie in the joined column of the same index.
    fn join_groups(&self, old: &Group, new: &Group) -> Group {
        if old == new {
            return old.clone();
        }
        let (longer, shorter) = match old.columns.len() >= new.columns.len() {
            true => (old, new),
            false => (new, old),
        };
        let mut joined = longer.clone();
        for (column, other) in joined.columns.iter_mut().zip(&shorter.columns) {
            for (entry, &ty) in column.iter_mut().zip(other) {
                *entry = self.lub(*entry, ty);
            }
        }
        joined
    }

    /// Join at a widening point, widening once the join has grown too often. An
    /// annotated local widens to its annotation.
    fn widen(
        &self,
        #[cfg_attr(not(feature = "debug"), expect(unused_variables, reason = "traced"))]
        (block, ctx): (BlockId, CtxId),
        var: VarId,
        widening: &mut Widening,
        old: TypeId,
        new: TypeId,
    ) -> TypeId {
        let gas = Gas::default();
        let solver = self.solver(&gas);
        let joined = solver.lub(old, new);
        let widened = widening.join(&solver, old, new);
        let result = match self.ir.var(var).annotation {
            Some(annotation) if widened != joined => annotation,
            _ => widened,
        };
        if result != joined {
            trace!(
                "widen {} at {}: {} to {}",
                self.var_name(var),
                self.place(block, ctx),
                self.show(joined),
                self.show(result)
            );
        }
        result
    }

    /// Join a value into a variable's joined type, queueing its readers if it grew.
    /// It widens as a join at a widening point does, an annotated variable to its
    /// annotation.
    fn join(&mut self, var: VarId, ty: TypeId) {
        if self.observing() {
            return;
        }
        let gas = Gas::default();
        let solver = self.solver(&gas);
        let annotation = self.ir.var(var).annotation;
        let accumulator = self.accumulator(var);
        let old = accumulator.ty;
        let joined = solver.lub(old, ty);
        let new = match (accumulator.widening.join(&solver, old, ty), annotation) {
            (widened, Some(annotation)) if widened != joined => annotation,
            (widened, _) => widened,
        };
        if new == old {
            return;
        }
        accumulator.ty = new;
        let readers = accumulator.readers.clone();
        trace!("accumulate {}: {}", self.var_name(var), self.show(new));
        for (block, ctx) in readers {
            self.enqueue(block, ctx);
        }
    }

    /// A variable's slot in its owner's states
    fn slot(&self, var: VarId) -> usize {
        let owner = self.ir.var(var).owner;
        self.slots[&(owner, var)]
    }

    /// The slot of a capture the running function caches, unless it's thawed
    fn cached(&self, at: At, state: &State, var: VarId) -> Option<usize> {
        if self.ir.var(var).owner == at.func {
            return None;
        }
        let slot = *self.slots.get(&(at.func, var))?;
        (!state.vars[slot].may.contains(May::THAWED)).then_some(slot)
    }

    /// Thaw a variable its owner assigns after it may have escaped: the functions
    /// that cache it read its accumulator from then on. Its readers include their
    /// entries, which load it, and run again to say so.
    fn thaw(&mut self, at: At, state: &State, var: VarId) {
        let data = self.ir.var(var);
        if self.observing()
            || data.owner != at.func
            || data.flags.contains(VarFlags::VOLATILE)
            || !state.vars[self.slot(var)].may.contains(May::ESCAPED)
        {
            return;
        }
        let accumulator = self.accumulator(var);
        if mem::replace(&mut accumulator.thawed, true) {
            return;
        }
        let readers = accumulator.readers.clone();
        trace!("thaw {}", self.var_name(var));
        for (block, ctx) in readers {
            self.enqueue(block, ctx);
        }
    }

    /// A variable's accumulator, made bottom if it has none yet
    fn accumulator(&mut self, var: VarId) -> &mut Accumulator {
        let bottom = self.db.bottom();
        self.accumulators.entry(var).or_insert_with(|| Accumulator {
            ty: bottom,
            widening: Widening::default(),
            readers: FxHashSet::default(),
            thawed: false,
        })
    }

    /// Make each `do` block parameter and channel that's still bottom dynamic,
    /// saying whether there was one. In a strict unit, each instead takes
    /// `Value`, as a def's omitted channel is seen there.
    fn dynamic_signatures(&mut self) -> bool {
        let bottom = self.db.bottom();
        let unknown = match self.strict() {
            true => self.db.top(),
            false => self.db.unknown(),
        };
        let [input, output] = [unknown; 2];
        let vars: Vec<(VarId, TypeId)> = (self.ir.funcs())
            .filter_map(|(_, func)| func.signature.as_ref())
            .flat_map(|signature| {
                (signature.params.iter().map(|&var| (var, unknown)))
                    .chain([(signature.input, input), (signature.output, output)])
                    .filter_map(|(var, ty)| Some((var?, ty)))
            })
            .filter(|(var, _)| {
                self.accumulators
                    .get(var)
                    .is_none_or(|acc| acc.ty == bottom)
            })
            .collect();
        for &(var, ty) in &vars {
            trace!("no call gives {}", self.var_name(var));
            self.join(var, ty);
        }
        !vars.is_empty()
    }

    /// A variable's joined type, making the block depend on it
    fn joined(&mut self, var: VarId, at: At) -> TypeId {
        let accumulator = self.accumulator(var);
        accumulator.readers.insert((at.block, at.ctx));
        accumulator.ty
    }

    fn run(&mut self, block: BlockId, ctx: CtxId) {
        let Some(mut state) = (self.states.get(&(block, ctx))).map(|entry| entry.state.clone())
        else {
            return;
        };
        if !self.observing() {
            self.unsettled.remove(&(block, ctx));
        }
        self.defaulting = self.marked.remove(&(block, ctx));
        trace!(
            "run {}{}: {}",
            self.place(block, ctx),
            match (self.observing(), self.defaulting) {
                (true, _) => " (final)",
                (false, true) => " (defaulting)",
                (false, false) => "",
            },
            self.render_state(block, &state)
        );
        let ir = self.ir;
        let data = ir.block(block);
        let func = ir.func(data.func);
        self.current = Some(data.func);
        let at = At {
            block,
            ctx,
            func: data.func,
        };
        if block == func.entry {
            self.bind_params(at, &mut state);
        }
        if block == func.exit {
            // A non-local return's value
            let returned = self.joined(func.result, at);
            let fact = &mut state.vars[self.slot(func.result)];
            fact.ty = self.lub(fact.ty, returned);
        }
        for (index, step) in data.steps.iter().enumerate() {
            if throws(step) {
                self.raise(at.ctx, data.handler, &state);
            }
            if !self.step(at, &mut state, step) {
                self.unreachable(at.block, index);
                return;
            }
        }
        self.terminal(at, state);
        self.unreachable(at.block, data.steps.len());
    }

    /// In the final pass, warn about the code a call decided never to return
    /// makes unreachable, if one ended the path at the step `index` of `block`,
    /// or past its last step, at its terminal
    fn unreachable(&mut self, block: BlockId, index: usize) {
        let Some(Divergence {
            span: call,
            decided: true,
        }) = self.diverged
        else {
            return;
        };
        if !self.observing() {
            return;
        }
        let data = self.ir.block(block);
        let span = match data.steps.get(index) {
            // The call is the whole statement: what's unreachable is what follows
            Some(Step::Eval(value)) if value.span == call => None,
            Some(Step::Assign(Target::Var { var, value }))
                if value.span == call && !matches!(self.ir.var(*var).origin, Origin::Source(_)) =>
            {
                None
            }
            Some(step) => Some(step_span(step)),
            None => Some(data.terminal.span()),
        };
        let span = span.or_else(|| {
            (data.steps[index + 1..].iter().map(step_span))
                .chain([data.terminal.span()])
                .find(|&span| span != Span::INVALID)
        });
        if let Some(span) = span.filter(|&span| span != Span::INVALID) {
            self.problem(Problem::Unreachable { span, call });
        }
    }

    /// Join a state into a handler, with the exception alone on the stack. Which
    /// exceptions a body raises isn't modeled, so the exception is `Value`.
    fn raise(&mut self, ctx: CtxId, handler: Option<BlockId>, state: &State) {
        let Some(handler) = handler else {
            return;
        };
        let state = State {
            vars: state.vars.clone(),
            stack: vec![Operand::new(self.db.top())],
            dup: false,
            groups: state.groups.clone(),
        };
        self.flow(ctx, handler, state);
    }

    /// The operands of a step or terminal's holes, popped from the stack in
    /// evaluation order
    fn operands(state: &mut State, count: usize) -> VecDeque<TypeId> {
        let at = state
            .stack
            .len()
            .checked_sub(count)
            .expect("an operand for each hole");
        state.stack.drain(at..).map(|operand| operand.ty).collect()
    }

    /// A call at `span` gave bottom, so the running step or terminal ends its
    /// path: for good if it's `decided` never to return, and otherwise until what
    /// it waits on grows. The first such call is the one it stops at.
    fn diverge(&mut self, span: Span, decided: bool) {
        if self.diverged.is_none() {
            trace!(
                "{}: {}",
                self.tables.locate(self.unit, span),
                match decided {
                    true => "never returns",
                    false => "gives nothing yet",
                }
            );
            self.diverged = Some(Divergence { span, decided });
        }
    }

    /// Apply a step. Returns whether its end is reachable: not if its pattern
    /// can't match, its narrowing leaves nothing, or a call in it gives bottom.
    fn step(&mut self, at: At, state: &mut State, step: &Step) -> bool {
        state.dup = false;
        self.diverged = None;
        match step {
            Step::Let { pattern, value } => {
                let link = self.source_link(at, state, value);
                let source = self.source_var(at, value);
                let mut operands = Self::operands(state, holes(value));
                let expected = match *pattern {
                    Pattern::Bind(var) => self.ir.var(var).annotation,
                    Pattern::Unpack { .. } => None,
                };
                let ty = self.expect(at, state, &mut operands, value, expected);
                if self.diverged.is_some() {
                    return false;
                }
                let bound = self.bind(at, state, pattern, (ty, source), value.span, true);
                if let Pattern::Bind(var) = *pattern {
                    self.relink(at, state, var, link);
                }
                return bound;
            }
            Step::Assign(target) => {
                let value = target.value();
                let count = match target {
                    Target::Var { .. } | Target::Import { .. } => 0,
                    Target::Field { object, .. } => holes(object),
                    Target::Index { object, args } => holes(object) + holes(args[0].pos()),
                };
                let link = self.source_link(at, state, value);
                let mut operands = Self::operands(state, count + holes(value));
                match *target {
                    Target::Var { var, .. } => {
                        let expected =
                            (self.ir.var(var).annotation).or_else(|| self.result_annotation(var));
                        let ty = self.expect(at, state, &mut operands, value, expected);
                        if self.diverged.is_some() {
                            return false;
                        }
                        self.assign(at, state, var, ty, value.span);
                        self.relink(at, state, var, link);
                    }
                    Target::Field {
                        ref object,
                        member,
                        value: ref item,
                    } => {
                        let span = object.span | value.span;
                        self.set(at, state, &mut operands, object, member, item, span);
                    }
                    Target::Index {
                        ref object,
                        ref args,
                    } => {
                        let span = object.span | value.span;
                        self.assign_index(at, state, &mut operands, object, &args[..], span);
                    }
                    Target::Import {
                        ref module,
                        item,
                        span,
                        ..
                    } => {
                        let expected = self.written_import(module, item, span);
                        let ty = self.expect(at, state, &mut operands, value, expected);
                        if self.diverged.is_some() {
                            return false;
                        }
                        match expected {
                            Some(expected) => self.store(at, ty, expected, value.span),
                            None if self.observing() && span != Span::INVALID => {
                                self.problem(Problem::MemberUse {
                                    span,
                                    name: self.db.symbol(item).to_owned(),
                                    misuse: MemberUse::Module,
                                });
                            }
                            None => {}
                        }
                    }
                }
            }
            Step::Default { var, value } => {
                let mut operands = Self::operands(state, holes(value));
                let annotation = self.ir.var(*var).annotation;
                // Without an annotation, what the default leaves open may be
                // what the item has where it's present
                let present = (annotation.is_none() && self.ir.var(*var).owner == at.func)
                    .then(|| state.vars[self.slot(*var)].ty)
                    .filter(|&ty| {
                        let mut unknown = false;
                        self.db.walk(ty, |node, _| {
                            unknown |= matches!(self.db.ty(node), Type::Unknown(_));
                        });
                        ty != self.db.bottom() && !unknown
                    });
                let ty = match present {
                    Some(present) => self.default_above(at, state, &mut operands, value, present),
                    None => self.expect(at, state, &mut operands, value, annotation),
                };
                if self.diverged.is_some() {
                    return false;
                }
                let ty = self.default(*var, ty, value.span);
                trace!("default {}: {}", self.var_name(*var), self.show(ty));
                let data = self.ir.var(*var);
                if data.owner == at.func && !data.flags.contains(VarFlags::VOLATILE) {
                    let slot = self.slot(*var);
                    state.vars[slot].ty = self.lub(state.vars[slot].ty, ty);
                    // Where the item is absent, each column has bottom for it
                    if let Some(link) = state.vars[slot].link {
                        let row = link.row as usize;
                        let group = &mut state.groups[link.group.index()];
                        for column in &mut group.columns {
                            column[row] = self.lub(column[row], ty);
                        }
                    }
                }
                self.thaw(at, state, *var);
                if self.publishes(at, state, *var) {
                    self.join(*var, ty);
                }
            }
            Step::Eval(value) => {
                let mut operands = Self::operands(state, holes(value));
                self.eval(at, state, &mut operands, value);
            }
            Step::Push(value) => {
                let link = self.source_link(at, state, value);
                let mut operands = Self::operands(state, holes(value));
                let ty = self.eval(at, state, &mut operands, value);
                state.stack.push(Operand { ty, link });
            }
            Step::Dup => {
                let top = *state.stack.last().expect("a value to duplicate");
                state.stack.push(top);
                state.dup = true;
            }
            Step::Pop => {
                state.stack.pop().expect("a value to discard");
            }
            Step::Assume(assume) => return self.assume(at, state, assume),
            Step::Capture(funcs) => {
                for &func in funcs {
                    for &var in &self.ir.func(func).escapes {
                        self.escape(state, var);
                    }
                }
            }
            &Step::Escape {
                span,
                kind,
                except,
                supers,
            } => {
                for &var in &self.ir.func(at.func).fields {
                    if Some(var) != except {
                        self.publish_field(state, var, span, kind);
                    }
                }
                if supers {
                    for &var in &self.ir.func(at.func).supers {
                        self.require_super(state, var, span, kind);
                    }
                }
            }
            Step::Cache(vars) => {
                for &var in vars {
                    let ty = self.joined(var, at);
                    let slot = &mut state.vars[self.slots[&(at.func, var)]];
                    slot.ty = ty;
                    if self.accumulators[&var].thawed {
                        slot.may.insert(May::THAWED);
                    }
                }
            }
        }
        self.diverged.is_none()
    }

    /// A variable escapes through a function being created: what its owner holds
    /// joins its accumulator, and every assignment after does too. A type that
    /// doesn't fit its annotation, such as one with a default's sentinel, joins
    /// with the annotation.
    fn escape(&mut self, state: &mut State, var: VarId) {
        let slot = self.slot(var);
        state.vars[slot].may.insert(May::ESCAPED);
        let ty = state.vars[slot].ty;
        let ty = match self.ir.var(var).annotation {
            Some(annotation) if self.relate(ty, annotation).status == Status::Contradicted => {
                self.lub(annotation, ty)
            }
            _ => ty,
        };
        self.join(var, ty);
    }

    /// An `(init)`'s receiver becomes observable at `span` while a field's variable
    /// holds what it does: reported if that doesn't fit the field's annotation.
    /// Anything may assign the field from here on, so the variable takes the
    /// annotation.
    fn publish_field(&mut self, state: &mut State, var: VarId, span: Span, kind: EscapeKind) {
        let slot = self.slot(var);
        let ty = state.vars[slot].ty;
        let data = self.ir.var(var);
        let Some(annotation) = data.annotation else {
            state.vars[slot].ty = self.db.unknown();
            return;
        };
        if self.observing()
            && ty != self.db.bottom()
            && self.conform(ty, annotation, span) == Status::Contradicted
        {
            let Origin::Field(key) = data.origin else {
                unreachable!("an `(init)` tracks fields")
            };
            let hash = if key.private { "#" } else { "" };
            let field = format!("{hash}{}", self.db.symbol(key.name));
            let (found, annotation) = self.pair(ty, annotation);
            self.problem(Problem::Uninitialized {
                span,
                field,
                found,
                annotation,
                kind,
            });
        }
        state.vars[slot].ty = annotation;
    }

    /// An `(init)`'s receiver becomes observable at `span`: reported unless a
    /// superclass's flag says it's initialized. It's reported once, so the flag
    /// says so from here on.
    fn require_super(&mut self, state: &mut State, var: VarId, span: Span, kind: EscapeKind) {
        let slot = self.slot(var);
        let ty = state.vars[slot].ty;
        let initialized = self.db.intern(Type::Literal(Literal::Bool(true)));
        if self.observing()
            && ty != self.db.bottom()
            && self.relate(ty, initialized).status != Status::Proven
        {
            let Origin::Super(class) = self.ir.var(var).origin else {
                unreachable!("an `(init)` tracks superclasses")
            };
            let class = self.shown(self.db.intern(Type::Decl(class)));
            self.problem(Problem::SuperUninitialized { span, class, kind });
        }
        state.vars[slot].ty = initialized;
    }

    /// Whether assigning a variable joins its accumulator: an ivar's assignment by
    /// another function, or by its owner once it may have escaped or if it's
    /// volatile
    fn publishes(&self, at: At, state: &State, var: VarId) -> bool {
        let data = self.ir.var(var);
        data.flags.contains(VarFlags::INTERPROCEDURAL)
            && (data.flags.contains(VarFlags::VOLATILE)
                || data.owner != at.func
                || state.vars[self.slot(var)].may.contains(May::ESCAPED))
    }

    /// The type an assignment stores: a literal assigned to a declared local or a
    /// field decays to its class, when that fits the annotation. A field keeps its
    /// annotation instead of a value that doesn't fit, which is reported where it's
    /// written.
    fn settle(&mut self, var: VarId, ty: TypeId) -> TypeId {
        let var = self.ir.var(var);
        if let (Origin::Field(_), Some(annotation)) = (var.origin, var.annotation)
            && self.relate(ty, annotation).status == Status::Contradicted
        {
            return annotation;
        }
        if !matches!(var.origin, Origin::Source(_) | Origin::Field(_)) {
            return ty;
        }
        let decayed = self.db.decay(ty);
        match var.annotation {
            _ if decayed == ty => ty,
            Some(annotation) if !self.below(decayed, annotation) => ty,
            _ => decayed,
        }
    }

    /// The type a default at `span` joins into its variable. Its literals decay.
    /// An annotated variable keeps its annotation when the default fits it, and
    /// otherwise takes a `nil` or symbol literal as a sentinel, which the body
    /// narrows away; any other default is reported.
    fn default(&mut self, var: VarId, ty: TypeId, span: Span) -> TypeId {
        let decayed = self.db.decay(ty);
        let Some(annotation) = self.ir.var(var).annotation else {
            return decayed;
        };
        if ty == self.db.bottom() || self.below(decayed, annotation) {
            return annotation;
        }
        if let Some(Literal::Nil | Literal::Sym(_)) = self.db.literal(ty) {
            return self.lub(annotation, ty);
        }
        let outcome = self.relate(ty, annotation);
        match outcome.status {
            Status::Proven => {}
            Status::Contradicted => {
                let (found, annotation) = self.pair(ty, annotation);
                self.problem(Problem::Default {
                    span,
                    found,
                    annotation,
                });
            }
            Status::Unresolved => self.undecided(span, residual(&outcome)),
        }
        annotation
    }

    /// Assign a variable, strongly where its owner caches its type, checking the
    /// value at `span` against its annotation. Returns the type stored.
    fn assign(&mut self, at: At, state: &mut State, var: VarId, ty: TypeId, span: Span) -> TypeId {
        self.check(var, ty, span);
        let ty = self.settle(var, ty);
        let data = self.ir.var(var);
        self.thaw(at, state, var);
        if data.owner == at.func {
            let slot = &mut state.vars[self.slot(var)];
            slot.ty = if data.flags.contains(VarFlags::VOLATILE) {
                self.db.bottom()
            } else {
                ty
            };
            slot.may.remove(May::UNASSIGNED);
            slot.link = None;
        }
        if self.publishes(at, state, var) {
            // A value that doesn't fit the annotation is reported where it's
            // written, and every read elsewhere sees the annotation instead
            let joined = match data.annotation {
                Some(annotation) if self.relate(ty, annotation).status == Status::Contradicted => {
                    annotation
                }
                _ => ty,
            };
            self.join(var, joined);
        }
        ty
    }

    /// Bind a pattern to a value at `span`, recording each binding, and linking
    /// the variable the value was read from, if any, to the whole of an unpack
    /// that correlates. Unpacking is a rule. Returns whether the pattern can
    /// match, reporting it if it can't. An `irrefutable` pattern raises on a
    /// mismatch, so a strict unit requires it to match every value of the type.
    fn bind(
        &mut self,
        at: At,
        state: &mut State,
        pattern: &Pattern,
        (ty, source): (TypeId, Option<VarId>),
        span: Span,
        irrefutable: bool,
    ) -> bool {
        match *pattern {
            Pattern::Bind(var) => self.binding(at, state, var, ty, span),
            Pattern::Unpack { ref items, group } => {
                let Some((types, columns)) = self.unpack(at, state, items, ty, span, irrefutable)
                else {
                    if self.observing() {
                        let found = self.subject(ty);
                        self.problem(Problem::Impossible { span, found });
                    }
                    return false;
                };
                // The group's earlier unpack no longer correlates anything
                let unlinked = |link: &mut Option<Link>| {
                    if link.is_some_and(|link| link.group == group) {
                        *link = None;
                    }
                };
                for var in &mut state.vars {
                    unlinked(&mut var.link);
                }
                for operand in &mut state.stack {
                    unlinked(&mut operand.link);
                }
                let correlated = !columns.columns.is_empty();
                state.groups[group.index()] = columns;
                // The most recent unpack of a variable correlates it, unless an
                // item rebinds it
                if correlated && let Some(source) = source {
                    let row = u32::try_from(items.len()).expect("too many pattern items");
                    let link = Some(Link { group, row });
                    self.relink(at, state, source, link);
                    // It holds only what its row's columns do, which is nothing
                    // until the unpack gives something
                    let slot = &mut state.vars[self.slot(source)];
                    if slot.link == link {
                        let gas = Gas::default();
                        let solver = self.solver(&gas);
                        let whole = (state.groups[group.index()].columns.iter())
                            .fold(self.db.bottom(), |joined, column| {
                                solver.lub(joined, column[items.len()])
                            });
                        slot.ty = solver.refine(slot.ty, whole);
                    }
                }
                for (row, (item, ty)) in items.iter().zip(types).enumerate() {
                    if let Some(var) = item.var {
                        let span = match self.ir.var(var).origin {
                            Origin::Source(span) => span,
                            _ => Span::INVALID,
                        };
                        self.binding(at, state, var, ty, span);
                        if correlated {
                            let row = u32::try_from(row).expect("too many pattern items");
                            self.relink(at, state, var, Some(Link { group, row }));
                        }
                    }
                }
            }
        }
        true
    }

    /// The group row a value comes from: a variable's own, or the top operand's
    /// for an operand, before it's taken
    fn source_link(&self, at: At, state: &State, value: &Expr) -> Option<Link> {
        match value.kind {
            ExprKind::Operand => state.stack.last()?.link,
            _ => state.vars[self.slot(self.source_var(at, value)?)].link,
        }
    }

    /// The variable the running function owns that a value is read from
    fn source_var(&self, at: At, value: &Expr) -> Option<VarId> {
        match value.kind {
            ExprKind::Var(var) | ExprKind::Copy(var) if self.ir.var(var).owner == at.func => {
                Some(var)
            }
            _ => None,
        }
    }

    /// Link a variable the running function caches the type of to a group row,
    /// once it's assigned
    fn relink(&self, at: At, state: &mut State, var: VarId, link: Option<Link>) {
        let data = self.ir.var(var);
        if data.owner == at.func && !data.flags.contains(VarFlags::VOLATILE) {
            state.vars[self.slot(var)].link = link;
        }
    }

    /// Bind a variable, checking the value at `span` against its annotation
    fn binding(&mut self, at: At, state: &mut State, var: VarId, ty: TypeId, span: Span) {
        let data = self.ir.var(var);
        if data.owner == at.func && !data.flags.contains(VarFlags::HOISTED) {
            // A fresh cell, which no function has captured yet
            state.vars[self.slot(var)].may.remove(May::ESCAPED);
        }
        let ty = self.assign(at, state, var, ty, span);
        if let Origin::Source(span) = self.ir.var(var).origin {
            let fact = Fact {
                ty,
                unassigned: false,
            };
            self.record(span, fact);
        }
    }

    /// Bind a function's parameters at its entry. A def's come from its declared
    /// type, and a `do` block's from its signature variables, or else its declared
    /// type.
    fn bind_params(&mut self, at: At, state: &mut State) {
        let func = self.ir.func(at.func);
        if !matches!(func.kind, FuncKind::Decl(_)) {
            return;
        }
        let Pattern::Unpack { items, .. } = &func.params else {
            unreachable!("parameters are a pattern of items")
        };
        let declared = (self.declared_params(at.func, items))
            .unwrap_or_else(|| vec![self.db.unknown(); items.len()]);
        let types: Vec<TypeId> = match &func.signature {
            Some(signature) => (signature.params.iter().zip(declared))
                .map(|(&slot, declared)| match slot {
                    Some(var) => self.joined(var, at),
                    None => declared,
                })
                .collect(),
            None => declared,
        };
        for (item, ty) in items.iter().zip(types) {
            if let Some(var) = item.var {
                self.binding(at, state, var, ty, Span::INVALID);
            }
        }
    }

    /// The types a function's declared type gives its parameters, under its
    /// rigids: a single its item's type, and a rest a `Tuple` or `Record` of its
    /// items, as the runtime binds it. `None` without a declared type. If they
    /// don't line up, each parameter is a gap.
    fn declared_params(&mut self, func: FuncId, params: &[PatternItem]) -> Option<Vec<TypeId>> {
        let schema = self.funcs[func.index()].declared.as_ref()?.params;
        let shares = match self.db.ty(schema) {
            Type::Schema(items) => shares(params, items),
            _ => None,
        };
        let Some(shares) = shares else {
            let types = (params.iter())
                .map(|param| match param.var.map(|var| self.ir.var(var).origin) {
                    Some(Origin::Source(span)) => {
                        self.gap(span, "a parameter its declared type doesn't line up with")
                    }
                    _ => self.db.unknown(),
                })
                .collect();
            return Some(types);
        };
        let types = (params.iter().zip(shares))
            .map(|(param, share)| match (&param.key, share) {
                (&PatternKey::Rest(kind), items) => self.rest_type(kind, items),
                (_, [item]) => match item.element {
                    Element::Positional(ty) | Element::Keyed { value: ty, .. } => ty,
                    Element::Include(_) => unreachable!("not a rest"),
                },
                _ => unreachable!("a single's share is one item"),
            })
            .collect();
        Some(types)
    }

    /// What a rest of `kind` binds for its share of a parameter schema: a `*` rest
    /// a `Tuple`, and any other a `Record`
    fn rest_type(&self, kind: RestKind, items: &[SchemaItem]) -> TypeId {
        let base = match kind {
            RestKind::Pos => self.intrinsic(Intrinsic::Tuple),
            RestKind::Key | RestKind::Mixed => self.designated_type(Designated::Record),
        };
        if base == self.db.unknown() {
            return base;
        }
        let schema = self.db.intern(Type::Schema(items.to_vec().into()));
        self.db.intern(Type::Apply {
            base,
            args: vec![Argument::Positional(schema)].into(),
            kind: Kind::Type,
        })
    }

    /// What a variable holds: its fact where its owner or the running function
    /// caches its type, and its joined type elsewhere. Its owner still knows
    /// whether it may be unassigned.
    fn read(&mut self, at: At, state: &State, var: VarId) -> Fact {
        let data = self.ir.var(var);
        if let Some(slot) = self.cached(at, state, var) {
            return Fact {
                ty: state.vars[slot].ty,
                unassigned: false,
            };
        }
        if data.owner != at.func {
            return Fact {
                ty: self.joined(var, at),
                unassigned: false,
            };
        }
        let fact = state.vars[self.slot(var)].fact();
        if !data.flags.contains(VarFlags::VOLATILE) {
            return fact;
        }
        Fact {
            ty: self.joined(var, at),
            unassigned: fact.unassigned,
        }
    }

    /// Narrow a variable whose type the running function caches. Returns whether
    /// anything is left.
    fn assume(&mut self, at: At, state: &mut State, assume: &Assume) -> bool {
        let data = self.ir.var(assume.var);
        let slot = if data.owner == at.func {
            if data.flags.contains(VarFlags::VOLATILE) {
                // An accumulator isn't narrowed
                return true;
            }
            self.slot(assume.var)
        } else {
            match self.cached(at, state, assume.var) {
                Some(slot) => slot,
                None => return true,
            }
        };
        let mut none = VecDeque::new();
        let bottom = self.db.bottom();
        // An operand with no value yet, such as a captured variable whose
        // accumulator hasn't grown, leaves nothing: narrowing nothing would let the
        // edge's state join a wider type than the operand later narrows to
        let shape;
        let target = match &assume.against {
            &Against::Decl(decl) => Some(NarrowTarget::Class(decl)),
            Against::Shape(items) => match self.designated(Designated::Unpack) {
                Some(unpack) => {
                    shape = self.pattern_shape(at, state, items);
                    Some(NarrowTarget::Shape {
                        unpack,
                        pattern: &shape,
                    })
                }
                None => None,
            },
            Against::Class(class) => {
                let ty = self.eval(at, state, &mut none, class);
                if ty == bottom {
                    return false;
                }
                self.class_of(ty)
                    .map(|(class, _)| NarrowTarget::Class(class))
            }
            Against::Value(value) => {
                let ty = self.eval(at, state, &mut none, value);
                if ty == bottom {
                    return false;
                }
                let literal = self.db.literal(ty).is_some();
                literal.then(|| NarrowTarget::Literal(self.db.regular(ty)))
            }
            &Against::Type(ty) => match self.db.literal(ty) {
                Some(_) => Some(NarrowTarget::Literal(self.db.regular(ty))),
                _ => self.class_of_instance(ty).map(NarrowTarget::Class),
            },
        };
        let Some(target) = target else {
            return true;
        };
        if matches!(target, NarrowTarget::Literal(_))
            && assume.relation != super::cfg::Relation::Exact
        {
            return true;
        }
        let fact = &mut state.vars[slot];
        if fact.ty == bottom {
            return true;
        }
        let gas = Gas::default();
        let narrowed = self
            .solver(&gas)
            .narrow(fact.ty, assume.relation, assume.negated, target);
        trace!(
            "assume {}{} {:?} {}: {} to {}{}",
            if assume.negated { "not " } else { "" },
            self.var_name(assume.var),
            assume.relation,
            match target {
                NarrowTarget::Class(decl) => self.tables.qualified(decl),
                NarrowTarget::Literal(ty) => self.show(ty),
                NarrowTarget::Shape { .. } => "a pattern".into(),
            },
            self.show(fact.ty),
            self.show(narrowed),
            if narrowed == bottom {
                " (unreachable)"
            } else {
                ""
            }
        );
        fact.ty = narrowed;
        let link = fact.link;
        narrowed != bottom && link.is_none_or(|link| self.correlate(state, link, narrowed))
    }

    /// Drop the columns of a group where a value narrowed to `narrowed` can't lie,
    /// by its row, and refine each value linked to the group by what its row's
    /// remaining columns give, which hold it on their own. Returns whether any
    /// column is left.
    fn correlate(&self, state: &mut State, link: Link, narrowed: TypeId) -> bool {
        let bottom = self.db.bottom();
        let gas = Gas::default();
        let solver = self.solver(&gas);
        let group = &mut state.groups[link.group.index()];
        if group.columns.is_empty() {
            return true;
        }
        let row = link.row as usize;
        let mut dropped = false;
        for column in &mut group.columns {
            if column[row] != bottom && solver.disjoint_types(column[row], narrowed) {
                column.fill(bottom);
                dropped = true;
            }
        }
        if !dropped {
            return true;
        }
        trace!(
            "correlate g{}.{}: dropped columns",
            link.group.index(),
            link.row
        );
        let rows = (group.columns.first()).map_or(0, |column| column.len());
        let rows: Vec<TypeId> = (0..rows)
            .map(|row| {
                (group.columns.iter())
                    .fold(bottom, |joined, column| solver.lub(joined, column[row]))
            })
            .collect();
        if rows.iter().all(|&ty| ty == bottom) {
            return false;
        }
        let refined = |ty: &mut TypeId, linked: Option<Link>| {
            if let Some(other) = linked
                && other.group == link.group
            {
                *ty = solver.refine(*ty, rows[other.row as usize]);
            }
        };
        for var in &mut state.vars {
            refined(&mut var.ty, var.link);
        }
        for operand in &mut state.stack {
            refined(&mut operand.ty, operand.link);
        }
        true
    }

    /// A type narrowed to its truthy values: without `nil` and `false`, the only
    /// values always falsy
    fn truthy(&self, ty: TypeId) -> TypeId {
        let gas = Gas::default();
        let solver = self.solver(&gas);
        [Literal::Nil, Literal::Bool(false)]
            .into_iter()
            .fold(ty, |ty, literal| {
                let target = NarrowTarget::Literal(self.db.intern(Type::Literal(literal)));
                solver.narrow(ty, super::cfg::Relation::Exact, true, target)
            })
    }

    /// Continue from a block's end along the edges its terminal takes. None is
    /// taken if a call in it gives bottom, though the handler still is where it
    /// can throw first.
    fn terminal(&mut self, at: At, mut state: State) {
        let data = self.ir.block(at.block);
        self.diverged = None;
        match &data.terminal {
            &Terminal::Branch { target, .. } => self.flow(at.ctx, target, state),
            Terminal::If { cond, then, else_ } => {
                if has_rule(cond) {
                    self.raise(at.ctx, data.handler, &state);
                }
                let dup = state.dup && matches!(cond.kind, ExprKind::Operand);
                state.dup = false;
                let mut operands = Self::operands(&mut state, holes(cond));
                self.eval(at, &mut state, &mut operands, cond);
                if self.diverged.is_some() {
                    return;
                }
                let mut truthy = state.clone();
                if dup {
                    // The value copied for the test is the one left
                    let top = &mut truthy.stack.last_mut().expect("the copied value").ty;
                    let bottom = self.db.bottom();
                    let narrowed = self.truthy(*top);
                    let empty = narrowed == bottom && *top != bottom;
                    *top = narrowed;
                    if empty {
                        self.flow(at.ctx, *else_, state);
                        return;
                    }
                }
                self.flow(at.ctx, *then, truthy);
                self.flow(at.ctx, *else_, state);
            }
            Terminal::Unpack {
                pattern,
                value,
                then,
                else_,
            } => {
                let source = self.source_var(at, value);
                let mut operands = Self::operands(&mut state, holes(value));
                self.raise(at.ctx, data.handler, &state);
                let ty = self.eval(at, &mut state, &mut operands, value);
                if self.diverged.is_some() {
                    return;
                }
                let mut bound = state.clone();
                if self.bind(at, &mut bound, pattern, (ty, source), value.span, false) {
                    self.flow(at.ctx, *then, bound);
                }
                self.flow(at.ctx, *else_, state);
            }
            Terminal::Catch { clauses, otherwise } => {
                let count = clauses.iter().map(|(class, _)| holes(class)).sum();
                let mut operands = Self::operands(&mut state, count);
                if clauses.iter().any(|(class, _)| has_rule(class)) {
                    self.raise(at.ctx, data.handler, &state);
                }
                let gas = Gas::default();
                let solver = self.solver(&gas);
                // A clause whose class isn't known, or a catch-all, binds a
                // dynamic exception outside a strict unit
                let strict = self.strict();
                let unknown = self.db.unknown();
                let unclassed = |rest| match strict {
                    true => rest,
                    false => unknown,
                };
                let mut rest = state.stack.last().expect("the exception").ty;
                for (class, clause) in clauses {
                    let ty = self.eval(at, &mut state, &mut operands, class);
                    if self.diverged.is_some() {
                        return;
                    }
                    let caught = match self.class_of(ty).map(|(class, _)| class) {
                        Some(class) => {
                            let target = NarrowTarget::Class(class);
                            let upper = super::cfg::Relation::Upper;
                            let caught = solver.narrow(rest, upper, false, target);
                            rest = solver.narrow(rest, upper, true, target);
                            caught
                        }
                        None => unclassed(rest),
                    };
                    if caught != self.db.bottom() {
                        let mut entered = state.clone();
                        *entered.stack.last_mut().expect("the exception") = Operand::new(caught);
                        self.flow(at.ctx, *clause, entered);
                    }
                }
                // A generated rethrow only pops the exception, so this is for a catch-all
                *state.stack.last_mut().expect("the exception") = Operand::new(unclassed(rest));
                self.flow(at.ctx, *otherwise, state);
            }
            Terminal::Next {
                iter,
                pattern,
                body,
                exit,
                span,
            } => {
                self.raise(at.ctx, data.handler, &state);
                let iteratee = iter.map(|iter| self.read(at, &state, iter).ty);
                let item = self.next(at, &mut state, iteratee, *span);
                if self.diverged.is_some() {
                    return;
                }
                let mut bound = state.clone();
                if self.bind(at, &mut bound, pattern, (item, None), *span, true) {
                    self.flow(at.ctx, *body, bound);
                }
                self.flow(at.ctx, *exit, state);
            }
            &Terminal::ElseFallthrough { span, kind } => {
                self.raise(at.ctx, data.handler, &state);
                if self.observing() {
                    self.problem(Problem::ElseFallthrough { span, kind });
                }
            }
            Terminal::Throw(value) => {
                if has_rule(value) {
                    self.raise(at.ctx, data.handler, &state);
                }
                let mut operands = Self::operands(&mut state, holes(value));
                self.eval(at, &mut state, &mut operands, value);
                if self.diverged.is_some() {
                    return;
                }
                self.raise(at.ctx, data.handler, &state);
            }
            &Terminal::Leave { entry, tag } => {
                let ctx = self.contexts.push(at.ctx, tag);
                self.merge(entry, ctx, state);
            }
            Terminal::EndFinally => {
                let tags = self.contexts.tags(at.ctx);
                let tag = *tags.last().expect("a `finally` is entered with a tag");
                let depth = u32::try_from(tags.len() - 1).expect("depths fit");
                let outer = self.contexts.truncate(at.ctx, depth);
                match tag {
                    Tag::Goto(target) => self.flow(outer, target, state),
                    Tag::Rethrow => self.raise(outer, data.handler, &state),
                }
            }
            Terminal::Guard { next, targets } => {
                for &target in targets {
                    let mut jumped = state.clone();
                    jumped.stack.clear();
                    jumped.dup = false;
                    self.flow(at.ctx, target, jumped);
                }
                self.flow(at.ctx, *next, state);
            }
            Terminal::ReturnFrom { func, value } => {
                if has_rule(value) {
                    self.raise(at.ctx, data.handler, &state);
                }
                let result = self.ir.func(*func).result;
                let expected = self.result_annotation(result);
                let mut operands = Self::operands(&mut state, holes(value));
                let ty = self.expect(at, &mut state, &mut operands, value, expected);
                if self.diverged.is_some() {
                    return;
                }
                self.check(result, ty, value.span);
                self.join(result, ty);
            }
            Terminal::Return | Terminal::Escape(_) | Terminal::Unreachable => {}
        }
    }

    /// The declaration a designated role of `std` or `strand` is, if it's checked
    fn designated(&self, role: Designated) -> Option<DeclId> {
        self.tables.designated_decl(role)
    }

    /// A designated class's instance type, or `Unknown` if it isn't checked
    fn designated_type(&self, role: Designated) -> TypeId {
        self.designated(role)
            .map_or(self.db.unknown(), |decl| self.db.intern(Type::Decl(decl)))
    }

    fn intrinsic(&self, intrinsic: Intrinsic) -> TypeId {
        self.db.intrinsic(intrinsic).unwrap_or(self.db.unknown())
    }

    /// A literal term's type, which is fresh
    fn literal(&self, literal: &Literal) -> TypeId {
        self.db.intern(Type::Fresh(literal.clone()))
    }
}

/// A declaration's type under its rigids, if it's a function type. An overloaded
/// def's implementation isn't described by its type until overloads are resolved.
fn declared_function(db: &Database, tables: &Tables<'_>, decl: DeclId) -> Option<Function> {
    if tables.sig_count(decl) != 1 {
        return None;
    }
    let mut ty = db.declaration(decl).ty;
    if let Type::Quantified { body, .. } = db.ty(ty) {
        let rigids = tables.group_rigids(db, (decl, tables.primary_sig(decl)));
        ty = db.substitute(*body, &rigids);
    }
    match db.ty(ty) {
        Type::Function(function) => Some(function.clone()),
        _ => None,
    }
}

/// Why a check couldn't be decided
fn residual(outcome: &Outcome) -> Residual {
    (outcome.diagnostics.iter())
        .find_map(|diagnostic| match diagnostic.issue {
            super::solver::Issue::Residual(residual) => Some(residual),
            super::solver::Issue::Contradiction(_) => None,
        })
        .unwrap_or(Residual::Unsupported("no reason recorded"))
}

/// How many operand holes an expression has
fn holes(expr: &Expr) -> usize {
    let mut count = 0;
    expr.walk(&mut |expr| count += usize::from(matches!(expr.kind, ExprKind::Operand)));
    count
}

fn has_rule(expr: &Expr) -> bool {
    let mut found = false;
    expr.walk(&mut |expr| found |= expr.is_rule());
    found
}

/// The span of the source a step evaluates, or `Span::INVALID` if it evaluates
/// none
fn step_span(step: &Step) -> Span {
    match step {
        Step::Let { value, .. }
        | Step::Default { value, .. }
        | Step::Eval(value)
        | Step::Push(value)
        | Step::Assign(Target::Var { value, .. } | Target::Import { value, .. }) => value.span,
        Step::Assign(target @ (Target::Field { object, .. } | Target::Index { object, .. })) => {
            object.span | target.value().span
        }
        Step::Dup
        | Step::Pop
        | Step::Assume(_)
        | Step::Capture(_)
        | Step::Cache(_)
        | Step::Escape { .. } => Span::INVALID,
    }
}

/// Whether a step can throw: it has a rule, which may call or fail, or it unpacks
fn throws(step: &Step) -> bool {
    match step {
        Step::Let { pattern, value } => {
            matches!(pattern, Pattern::Unpack { .. }) || has_rule(value)
        }
        Step::Assign(target) => !matches!(target, Target::Var { .. }) || has_rule(target.value()),
        Step::Default { value, .. } | Step::Eval(value) | Step::Push(value) => has_rule(value),
        Step::Dup
        | Step::Pop
        | Step::Assume(_)
        | Step::Capture(_)
        | Step::Cache(_)
        | Step::Escape { .. } => false,
    }
}

/// Each block's place in the queue, its reverse postorder index over every edge
/// from each function's entry in turn, and whether an edge to it retreats in that
/// order, which makes it a widening point. Blocks no edge reaches come last.
fn order(ir: &Ir) -> Blocks {
    let count = ir.blocks().count();
    let successors = |block: BlockId| {
        let data = ir.block(block);
        let mut next: Vec<BlockId> = data.terminal.successors().collect();
        if let Terminal::Leave {
            tag: Tag::Goto(target),
            ..
        } = data.terminal
        {
            next.push(target);
        }
        next.extend(data.handler);
        next
    };
    let mut visited = vec![false; count];
    let mut postorder = Vec::with_capacity(count);
    let roots = ir
        .funcs()
        .map(|(_, func)| func.entry)
        .chain(ir.blocks().map(|(id, _)| id));
    for root in roots {
        if visited[root.index()] {
            continue;
        }
        visited[root.index()] = true;
        let mut stack = vec![(root, successors(root), 0)];
        while let Some((block, next, index)) = stack.last_mut() {
            if let Some(&target) = next.get(*index) {
                *index += 1;
                if !visited[target.index()] {
                    visited[target.index()] = true;
                    stack.push((target, successors(target), 0));
                }
            } else {
                postorder.push(*block);
                stack.pop();
            }
        }
    }
    let mut rank = vec![0; count];
    for (index, block) in postorder.iter().rev().enumerate() {
        rank[block.index()] = u32::try_from(index).expect("graph too large");
    }
    let mut widens = bitbox![0; count];
    for (block, _) in ir.blocks() {
        for target in successors(block) {
            if rank[target.index()] <= rank[block.index()] {
                widens.set(target.index(), true);
            }
        }
    }
    Blocks {
        rank: rank.into(),
        widens,
    }
}

/// Each parameter's share of its declared schema's `items`, in order: one item
/// for a single, and for a rest an included pack or the run of repeated items its
/// kind takes. `None` if they don't line up.
fn shares<'i>(params: &[PatternItem], items: &'i [SchemaItem]) -> Option<Vec<&'i [SchemaItem]>> {
    let mut shares = Vec::with_capacity(params.len());
    let mut start = 0;
    for param in params {
        let first = items.get(start)?;
        let single = first.multiplicity != Multiplicity::Repeated
            && !matches!(first.element, Element::Include(_));
        let len = match param.key {
            PatternKey::Rest(_) if matches!(first.element, Element::Include(_)) => 1,
            PatternKey::Rest(kind) => (items[start..].iter())
                .take_while(|item| {
                    item.multiplicity == Multiplicity::Repeated
                        && match item.element {
                            Element::Positional(_) => kind != RestKind::Key,
                            Element::Keyed { .. } => kind != RestKind::Pos,
                            Element::Include(_) => false,
                        }
                })
                .count(),
            _ if single => 1,
            _ => return None,
        };
        if len == 0 {
            return None;
        }
        shares.push(&items[start..start + len]);
        start += len;
    }
    (start == items.len()).then_some(shares)
}
