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
//! Narrowing applies a condition's relations to the variable it tests, making the
//! edge unreachable when nothing is left.
//!
//! Captured state isn't flow-sensitive. A captured variable's assignments, in any
//! function, join into one type, which every nested function reads it as. Its
//! owner keeps narrowing it, but a flagged one reverts to the joined type after any
//! step that can call. A non-local return joins its value into its def's result the
//! same way. A block that reads a joined type depends on it, and is queued again
//! when it grows.
//!
//! A step that can throw joins its state before it into its handler, with the
//! exception alone on the stack.
//!
//! Calls, collection literals, binary strings, interpolations, `for` items and
//! unpacking patterns are checking rules, each solved by a solver of its own (see
//! [`rule`]). Rules that look up members give the dynamic type. When the queue
//! empties, the rules still undecided in each function's earliest block that has
//! any are frozen to default, and iteration resumes, in rounds until none is left. Then a final pass runs every block once
//! more over its final state, to record what each variable reference and binding
//! saw and to report: contradicted rules, values that don't fit an annotation or a
//! declared result, and reads that may be unassigned. A block in a `finally` is
//! judged once per context, and a problem at a span is reported once.

mod eval;
mod problem;
mod rule;
mod state;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, VecDeque};

pub(crate) use problem::Problem;
use rule::{Conclusion, Site};
pub(crate) use state::Fact;
use state::{Contexts, CtxId, State};

use super::{
    cfg::{
        Against, Assume, BlockId, Expr, ExprKind, FuncId, FuncKind, Ir, Origin, Pattern, Step, Tag,
        Target, Terminal, VarId,
    },
    elab::{Designated, Tables},
    solver::{NarrowTarget, Outcome, Provenance, Residual, Solver, Status, Widening},
    r#type::{Database, DeclId, Element, Function, Intrinsic, Literal, Type, TypeId, UnitId},
};
use crate::source::Span;

/// What flow concluded about a unit
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Results {
    /// What each variable reference read and each binding bound, joined over every
    /// context, by span
    pub(crate) facts: HashMap<Span, Fact>,
    /// What the reporting pass diagnosed, each once
    pub(crate) problems: Vec<Problem>,
    /// The checks it couldn't decide, each once
    pub(crate) unresolved: Vec<(Span, Residual)>,
}

/// Analyze a unit's graph
pub(crate) fn analyze(ir: &Ir, db: &Database, tables: &Tables<'_>) -> Results {
    Flow::new(ir, db, tables, false).analyze()
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
    /// Each function's declared type, under its rigids, if it's a function type
    declared: Vec<Option<Function>>,
    /// The declarations whose rigids states may hold, which every solver assumes
    scope: Vec<DeclId>,
    /// Each block's place in the queue: its reverse postorder index
    rank: Vec<u32>,
    /// Whether a block is the target of an edge that retreats in that order
    widens: Vec<bool>,
    /// Each variable's index among its owner's
    slots: Vec<usize>,
    /// Each checked module's unit, by name
    modules: HashMap<&'u str, UnitId>,
    contexts: Contexts,
    states: HashMap<(BlockId, CtxId), State>,
    queue: BTreeSet<(u32, BlockId, CtxId)>,
    /// Take the queue's last entry rather than its first, to show that the result
    /// doesn't depend on the order
    reversed: bool,
    widenings: HashMap<(BlockId, CtxId, VarId), Widening>,
    /// The joined type of each captured variable, and of each result a non-local
    /// return gives a value
    joined: HashMap<VarId, (TypeId, Widening)>,
    /// The blocks that read each joined type
    readers: HashMap<VarId, BTreeSet<(BlockId, CtxId)>>,
    rules: HashMap<(Site, CtxId), Conclusion>,
    /// Filled by the final pass, which leaves states alone
    results: Option<Results>,
}

impl<'a, 'u> Flow<'a, 'u> {
    fn new(ir: &'a Ir, db: &'a Database, tables: &'a Tables<'u>, reversed: bool) -> Self {
        let (rank, widens) = order(ir);
        let mut slots = vec![0; ir.var_count()];
        let mut scope = Vec::new();
        for (_, func) in ir.funcs() {
            for (slot, &var) in func.vars.iter().enumerate() {
                slots[var.index()] = slot;
            }
            if let FuncKind::Decl(decl) = func.kind {
                let key = (decl, tables.primary_sig(decl));
                for binder in tables.groups.get(&key).into_iter().flatten() {
                    let owner = tables.sig_decls[&(binder.decl, binder.sig)];
                    if !scope.contains(&owner) {
                        scope.push(owner);
                    }
                }
            }
        }
        let unit = (ir.funcs())
            .find_map(|(_, func)| match func.kind {
                FuncKind::Module(unit) => Some(unit),
                FuncKind::Decl(_) => None,
            })
            .expect("a unit's graph has its module function");
        let declared = (ir.funcs())
            .map(|(_, func)| match func.kind {
                FuncKind::Decl(decl) => declared_function(db, tables, decl),
                FuncKind::Module(_) => None,
            })
            .collect();
        let modules = (tables.units.iter().enumerate())
            .filter_map(|(index, unit)| match unit.compiler.mode {
                crate::Mode::Module { name } => Some((name, UnitId::from_index(index))),
                crate::Mode::Script | crate::Mode::Repl => None,
            })
            .collect();
        Self {
            ir,
            db,
            tables,
            unit,
            declared,
            scope,
            rank,
            widens,
            slots,
            modules,
            contexts: Contexts::new(),
            states: HashMap::new(),
            queue: BTreeSet::new(),
            reversed,
            widenings: HashMap::new(),
            joined: HashMap::new(),
            readers: HashMap::new(),
            rules: HashMap::new(),
            results: None,
        }
    }

    /// Iterate to a fixed point in rounds, then record what each reference and
    /// binding saw, and report
    fn analyze(mut self) -> Results {
        let bottom = self.db.bottom();
        for (_, func) in self.ir.funcs() {
            let vars = (func.vars.iter())
                .map(|&var| Fact {
                    ty: bottom,
                    unassigned: !self.ir.var(var).bottom,
                })
                .collect();
            let state = State {
                vars,
                stack: Vec::new(),
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
            // Upstream first: in each function, the undecided rules of its earliest
            // block with any. A later rule in that block runs after its inputs.
            let mut earliest: HashMap<FuncId, u32> = HashMap::new();
            for rule in self.rules.values() {
                if !rule.decided && !rule.frozen {
                    let rank = self.rank[rule.block.index()];
                    let func = self.ir.block(rule.block).func;
                    let entry = earliest.entry(func).or_insert(rank);
                    *entry = (*entry).min(rank);
                }
            }
            if earliest.is_empty() {
                break;
            }
            let mut frozen = Vec::new();
            for (&(_, ctx), rule) in &mut self.rules {
                let func = self.ir.block(rule.block).func;
                if !rule.decided
                    && !rule.frozen
                    && earliest.get(&func) == Some(&self.rank[rule.block.index()])
                {
                    rule.frozen = true;
                    frozen.push((rule.block, ctx));
                }
            }
            for (block, ctx) in frozen {
                self.enqueue(block, ctx);
            }
        }
        self.results = Some(Results::default());
        let mut keys: Vec<_> = self.states.keys().copied().collect();
        keys.sort_by_key(|&(block, ctx)| (self.rank[block.index()], block, ctx));
        for (block, ctx) in keys {
            self.run(block, ctx);
        }
        let mut results = self.results.take().expect("recorded by the final pass");
        results.problems.sort_by_key(|problem| {
            let span = crate::source::Diagnose::span(problem);
            (span.start, span.end)
        });
        results
            .unresolved
            .sort_by_key(|&(span, _)| (span.start, span.end));
        results
    }

    /// A solver that holds the region's rigids as its assumptions
    fn solver(&self) -> Solver<'a> {
        let mut solver = Solver::new(self.db);
        for &decl in &self.scope {
            solver.assume(decl);
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
            self.solver().lub(a, b)
        }
    }

    /// Relate two closed types
    fn relate(&self, actual: TypeId, expected: TypeId) -> Outcome {
        let mut solver = self.solver();
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
        if crate::source::Diagnose::span(&problem) != Span::INVALID
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
        let outcome = self.relate(ty, annotation);
        match outcome.status {
            Status::Proven => {}
            Status::Contradicted => {
                let found = self.tables.render_type(self.db, ty);
                let annotation = self.tables.render_type(self.db, annotation);
                self.problem(Problem::Annotation {
                    span,
                    found,
                    annotation,
                    result,
                });
            }
            Status::Unresolved => {
                let residual = (outcome.diagnostics.iter())
                    .find_map(|diagnostic| match diagnostic.issue {
                        super::solver::Issue::Residual(residual) => Some(residual),
                        super::solver::Issue::Contradiction(_) => None,
                    })
                    .unwrap_or(Residual::Unsupported);
                self.undecided(span, residual);
            }
        }
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
        let result = self.declared[data.owner.index()].as_ref()?.result;
        (result != self.db.unknown()).then_some(result)
    }

    /// The ambient channels a function's calls pass, when it declares them
    fn channels(&self, func: FuncId) -> (Option<TypeId>, Option<TypeId>) {
        let Some(declared) = &self.declared[func.index()] else {
            return (None, None);
        };
        let known = |ty: Option<TypeId>| ty.filter(|&ty| ty != self.db.unknown());
        (known(declared.input), known(declared.output))
    }

    fn observing(&self) -> bool {
        self.results.is_some()
    }

    fn enqueue(&mut self, block: BlockId, ctx: CtxId) {
        if !self.observing() {
            self.queue.insert((self.rank[block.index()], block, ctx));
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

    /// Join a state into a block's, queueing the block if it grew
    fn merge(&mut self, block: BlockId, ctx: CtxId, state: State) {
        if self.observing() {
            return;
        }
        let key = (block, ctx);
        let Some(old) = self.states.get(&key) else {
            self.states.insert(key, state);
            self.enqueue(block, ctx);
            return;
        };
        let old = old.clone();
        assert_eq!(
            old.stack.len(),
            state.stack.len(),
            "b{} is entered at two stack depths",
            block.index()
        );
        let func = self.ir.func(self.ir.block(block).func);
        let mut vars = Vec::with_capacity(old.vars.len());
        for (slot, (old, new)) in old.vars.iter().zip(&state.vars).enumerate() {
            let ty = if old.ty == new.ty {
                old.ty
            } else if self.widens[block.index()] {
                self.widen(key, func.vars[slot], old.ty, new.ty)
            } else {
                self.lub(old.ty, new.ty)
            };
            vars.push(Fact {
                ty,
                unassigned: old.unassigned || new.unassigned,
            });
        }
        let stack = (old.stack.iter().zip(&state.stack))
            .map(|(&old, &new)| self.lub(old, new))
            .collect();
        let joined = State { vars, stack };
        if joined != old {
            self.states.insert(key, joined);
            self.enqueue(block, ctx);
        }
    }

    /// Join at a widening point, widening once the join has grown too often. An
    /// annotated local widens to its annotation.
    fn widen(
        &mut self,
        (block, ctx): (BlockId, CtxId),
        var: VarId,
        old: TypeId,
        new: TypeId,
    ) -> TypeId {
        let solver = self.solver();
        let joined = solver.lub(old, new);
        let widening = self.widenings.entry((block, ctx, var)).or_default();
        let widened = widening.join(&solver, old, new);
        match self.ir.var(var).annotation {
            Some(annotation) if widened != joined => annotation,
            _ => widened,
        }
    }

    /// Join a value into a variable's joined type, queueing its readers if it grew
    fn join(&mut self, var: VarId, ty: TypeId) {
        if self.observing() {
            return;
        }
        let solver = self.solver();
        let bottom = self.db.bottom();
        let (old, widening) = self
            .joined
            .entry(var)
            .or_insert((bottom, Widening::default()));
        let new = widening.join(&solver, *old, ty);
        if new == *old {
            return;
        }
        *old = new;
        for (block, ctx) in self.readers.get(&var).cloned().into_iter().flatten() {
            self.enqueue(block, ctx);
        }
    }

    /// A variable's joined type, making the block depend on it
    fn joined(&mut self, var: VarId, at: At) -> TypeId {
        self.readers
            .entry(var)
            .or_default()
            .insert((at.block, at.ctx));
        self.joined
            .get(&var)
            .map_or(self.db.bottom(), |&(ty, _)| ty)
    }

    fn run(&mut self, block: BlockId, ctx: CtxId) {
        let Some(mut state) = self.states.get(&(block, ctx)).cloned() else {
            return;
        };
        let ir = self.ir;
        let data = ir.block(block);
        let func = ir.func(data.func);
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
            let fact = &mut state.vars[self.slots[func.result.index()]];
            fact.ty = self.lub(fact.ty, returned);
        }
        for (index, step) in data.steps.iter().enumerate() {
            if throws(step) {
                self.raise(at.ctx, data.handler, &state, self.db.unknown());
            }
            if !self.step(at, &mut state, index, step) {
                return;
            }
            if calls(step) {
                self.forget(at, &mut state);
            }
        }
        self.terminal(at, state);
    }

    /// Join a state into a handler, with the exception alone on the stack
    fn raise(&mut self, ctx: CtxId, handler: Option<BlockId>, state: &State, exception: TypeId) {
        let Some(handler) = handler else {
            return;
        };
        let state = State {
            vars: state.vars.clone(),
            stack: vec![exception],
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
        state.stack.split_off(at).into()
    }

    /// Apply the step at `index`. Returns whether its end is reachable.
    fn step(&mut self, at: At, state: &mut State, index: usize, step: &Step) -> bool {
        match step {
            Step::Let { pattern, value } => {
                let mut operands = Self::operands(state, holes(value));
                let expected = match *pattern {
                    Pattern::Bind(var) => self.ir.var(var).annotation,
                    Pattern::Unpack(_) => None,
                };
                let ty = self.expect(at, state, &mut operands, value, expected);
                let site = Site::Pattern(at.block, Some(index));
                self.bind(at, state, pattern, ty, site, value.span, true);
            }
            Step::Assign { target, value } => {
                let count = match target {
                    Target::Var(_) => 0,
                    Target::Field { object, .. } => holes(object),
                    Target::Index { object, index, .. } => holes(object) + holes(index),
                };
                let mut operands = Self::operands(state, count + holes(value));
                match *target {
                    Target::Var(var) => {
                        let expected =
                            (self.ir.var(var).annotation).or_else(|| self.result_annotation(var));
                        let ty = self.expect(at, state, &mut operands, value, expected);
                        self.assign(at, state, var, ty, value.span);
                    }
                    Target::Field { ref object, .. } => {
                        self.eval(at, state, &mut operands, object);
                        self.eval(at, state, &mut operands, value);
                    }
                    Target::Index {
                        ref object,
                        ref index,
                        ..
                    } => {
                        self.eval(at, state, &mut operands, object);
                        self.eval(at, state, &mut operands, index);
                        self.eval(at, state, &mut operands, value);
                    }
                }
            }
            Step::Default { var, value } => {
                let mut operands = Self::operands(state, holes(value));
                let ty = self.eval(at, state, &mut operands, value);
                let ty = self.settle(*var, ty);
                if self.ir.var(*var).owner == at.func {
                    let fact = state.vars[self.slots[var.index()]];
                    state.vars[self.slots[var.index()]] = Fact {
                        ty: self.lub(fact.ty, ty),
                        unassigned: fact.unassigned,
                    };
                }
                if self.ir.var(*var).captured {
                    self.join(*var, ty);
                }
            }
            Step::Eval(value) => {
                let mut operands = Self::operands(state, holes(value));
                self.eval(at, state, &mut operands, value);
            }
            Step::Push(value) => {
                let mut operands = Self::operands(state, holes(value));
                let ty = self.eval(at, state, &mut operands, value);
                state.stack.push(ty);
            }
            Step::Dup => {
                let top = *state.stack.last().expect("a value to duplicate");
                state.stack.push(top);
            }
            Step::Pop => {
                state.stack.pop().expect("a value to discard");
            }
            Step::Assume(assume) => return self.assume(at, state, assume),
        }
        true
    }

    /// The type an assignment stores: a literal assigned to a declared local decays
    /// to its class, when that fits the local's annotation
    fn settle(&self, var: VarId, ty: TypeId) -> TypeId {
        let var = self.ir.var(var);
        if !matches!(var.origin, Origin::Source(_)) {
            return ty;
        }
        let decayed = self.db.decay(ty);
        match var.annotation {
            _ if decayed == ty => ty,
            Some(annotation) if !self.below(decayed, annotation) => ty,
            _ => decayed,
        }
    }

    /// Assign a variable, strongly where its owner is analyzed, checking the value
    /// at `span` against its annotation
    fn assign(&mut self, at: At, state: &mut State, var: VarId, ty: TypeId, span: Span) {
        let ty = self.settle(var, ty);
        self.check(var, ty, span);
        let data = self.ir.var(var);
        if data.owner == at.func {
            state.vars[self.slots[var.index()]] = Fact {
                ty,
                unassigned: false,
            };
        }
        if data.captured {
            self.join(var, ty);
        }
    }

    /// Bind a pattern to a value at `span`, recording each binding. Unpacking is
    /// the rule at `site`, diagnosed if `strict`: a pattern that is a test isn't.
    #[expect(clippy::too_many_arguments, reason = "a pattern's context")]
    fn bind(
        &mut self,
        at: At,
        state: &mut State,
        pattern: &Pattern,
        ty: TypeId,
        site: Site,
        span: Span,
        strict: bool,
    ) {
        match pattern {
            &Pattern::Bind(var) => self.binding(at, state, var, ty, span),
            Pattern::Unpack(items) => {
                let blame = strict.then_some(span);
                let types = self.unpack(at, site, items, ty, blame);
                for (item, ty) in items.iter().zip(types) {
                    if let Some(var) = item.var {
                        let span = match self.ir.var(var).origin {
                            Origin::Source(span) => span,
                            _ => Span::INVALID,
                        };
                        self.binding(at, state, var, ty, span);
                    }
                }
            }
        }
    }

    /// Bind a variable, checking the value at `span` against its annotation
    fn binding(&mut self, at: At, state: &mut State, var: VarId, ty: TypeId, span: Span) {
        self.assign(at, state, var, ty, span);
        if let Origin::Source(span) = self.ir.var(var).origin {
            let fact = self.read(at, state, var);
            self.record(span, fact);
        }
    }

    /// Bind a function's parameters at its entry. A def's come from its signature,
    /// and a `do` block's from its signature variables, or else their annotations.
    fn bind_params(&mut self, at: At, state: &mut State) {
        let func = self.ir.func(at.func);
        if !matches!(func.kind, FuncKind::Decl(_)) {
            return;
        }
        let Pattern::Unpack(items) = &func.params else {
            unreachable!("parameters are a pattern of items")
        };
        let unknown = self.db.unknown();
        let types: Vec<TypeId> = match &func.signature {
            Some(signature) => (items.iter().zip(&signature.params))
                .map(|(item, &slot)| match slot {
                    Some(var) => self.joined(var, at),
                    None => (item.var)
                        .and_then(|var| self.ir.var(var).annotation)
                        .unwrap_or(unknown),
                })
                .collect(),
            None => self.declared_params(at.func, items.len()),
        };
        for (item, ty) in items.iter().zip(types) {
            if let Some(var) = item.var {
                self.binding(at, state, var, ty, Span::INVALID);
            }
        }
    }

    /// The types a def's or method's signature gives its parameters, under its
    /// rigids; `Unknown` for a rest, or for every one if they don't line up
    fn declared_params(&self, func: FuncId, count: usize) -> Vec<TypeId> {
        let unknown = self.db.unknown();
        let Some(function) = &self.declared[func.index()] else {
            return vec![unknown; count];
        };
        let Type::Schema(items) = self.db.ty(function.params) else {
            return vec![unknown; count];
        };
        if items.len() != count {
            return vec![unknown; count];
        }
        (items.iter())
            .map(|item| match item.element {
                Element::Positional(ty) | Element::Keyed { value: ty, .. } => ty,
                Element::Include(_) => unknown,
            })
            .collect()
    }

    /// What a variable holds: its fact where its owner is analyzed, and its joined
    /// type elsewhere
    fn read(&mut self, at: At, state: &State, var: VarId) -> Fact {
        if self.ir.var(var).owner == at.func {
            return state.vars[self.slots[var.index()]];
        }
        Fact {
            ty: self.joined(var, at),
            unassigned: false,
        }
    }

    /// Revert the owner's flagged variables to their joined types, after a step
    /// that may have run a closure assigning them
    fn forget(&mut self, at: At, state: &mut State) {
        let func = self.ir.func(at.func);
        for (slot, &var) in func.vars.iter().enumerate() {
            if self.ir.var(var).flagged {
                let joined = self.joined(var, at);
                state.vars[slot].ty = joined;
            }
        }
    }

    /// Narrow a variable its function owns. Returns whether anything is left.
    fn assume(&mut self, at: At, state: &mut State, assume: &Assume) -> bool {
        if self.ir.var(assume.var).owner != at.func {
            // Captured state isn't narrowed
            return true;
        }
        let mut none = VecDeque::new();
        let target = match &assume.against {
            Against::Class(class) => {
                let ty = self.eval(at, state, &mut none, class);
                self.class_of(ty).map(NarrowTarget::Class)
            }
            Against::Value(value) => {
                let ty = self.eval(at, state, &mut none, value);
                matches!(self.db.ty(ty), Type::Literal(_)).then_some(NarrowTarget::Literal(ty))
            }
            &Against::Type(ty) => match self.db.ty(ty) {
                Type::Literal(_) => Some(NarrowTarget::Literal(ty)),
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
        let fact = &mut state.vars[self.slots[assume.var.index()]];
        let bottom = self.db.bottom();
        if fact.ty == bottom {
            return true;
        }
        let narrowed = self
            .solver()
            .narrow(fact.ty, assume.relation, assume.negated, target);
        fact.ty = narrowed;
        narrowed != bottom
    }

    fn terminal(&mut self, at: At, mut state: State) {
        let data = self.ir.block(at.block);
        let unknown = self.db.unknown();
        match &data.terminal {
            &Terminal::Branch(target) => self.flow(at.ctx, target, state),
            Terminal::If { cond, then, else_ } => {
                if has_rule(cond) {
                    self.raise(at.ctx, data.handler, &state, unknown);
                }
                let mut operands = Self::operands(&mut state, holes(cond));
                self.eval(at, &mut state, &mut operands, cond);
                self.flow(at.ctx, *then, state.clone());
                self.flow(at.ctx, *else_, state);
            }
            Terminal::Unpack {
                pattern,
                value,
                then,
                else_,
            } => {
                let mut operands = Self::operands(&mut state, holes(value));
                self.raise(at.ctx, data.handler, &state, unknown);
                let ty = self.eval(at, &mut state, &mut operands, value);
                let mut bound = state.clone();
                let site = Site::Pattern(at.block, None);
                self.bind(at, &mut bound, pattern, ty, site, value.span, false);
                self.flow(at.ctx, *then, bound);
                self.flow(at.ctx, *else_, state);
            }
            Terminal::Catch { clauses, otherwise } => {
                let count = clauses.iter().map(|(class, _)| holes(class)).sum();
                let mut operands = Self::operands(&mut state, count);
                if clauses.iter().any(|(class, _)| has_rule(class)) {
                    self.raise(at.ctx, data.handler, &state, unknown);
                }
                let solver = self.solver();
                let mut rest = *state.stack.last().expect("the exception");
                for (class, clause) in clauses {
                    let ty = self.eval(at, &mut state, &mut operands, class);
                    let caught = match self.class_of(ty) {
                        Some(class) => {
                            let target = NarrowTarget::Class(class);
                            let upper = super::cfg::Relation::Upper;
                            let caught = solver.narrow(rest, upper, false, target);
                            rest = solver.narrow(rest, upper, true, target);
                            caught
                        }
                        None => rest,
                    };
                    if caught != self.db.bottom() {
                        let mut entered = state.clone();
                        *entered.stack.last_mut().expect("the exception") = caught;
                        self.flow(at.ctx, *clause, entered);
                    }
                }
                *state.stack.last_mut().expect("the exception") = rest;
                self.flow(at.ctx, *otherwise, state);
            }
            Terminal::Next {
                iter,
                pattern,
                body,
                exit,
                span,
            } => {
                self.raise(at.ctx, data.handler, &state, unknown);
                let iterable = self.read(at, &state, *iter).ty;
                let item = self.next(at, iterable, *span);
                let mut bound = state.clone();
                let site = Site::Pattern(at.block, None);
                self.bind(at, &mut bound, pattern, item, site, *span, true);
                self.flow(at.ctx, *body, bound);
                self.flow(at.ctx, *exit, state);
            }
            Terminal::Throw(value) => {
                if has_rule(value) {
                    self.raise(at.ctx, data.handler, &state, unknown);
                }
                let mut operands = Self::operands(&mut state, holes(value));
                let ty = self.eval(at, &mut state, &mut operands, value);
                self.raise(at.ctx, data.handler, &state, ty);
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
                    Tag::Rethrow => self.raise(outer, data.handler, &state, unknown),
                }
            }
            Terminal::Guard { next, targets } => {
                for &target in targets {
                    let mut jumped = state.clone();
                    jumped.stack.clear();
                    self.flow(at.ctx, target, jumped);
                }
                self.flow(at.ctx, *next, state);
            }
            Terminal::ReturnFrom { func, value } => {
                if has_rule(value) {
                    self.raise(at.ctx, data.handler, &state, unknown);
                }
                let result = self.ir.func(*func).result;
                let expected = self.result_annotation(result);
                let mut operands = Self::operands(&mut state, holes(value));
                let ty = self.expect(at, &mut state, &mut operands, value, expected);
                self.check(result, ty, value.span);
                self.join(result, ty);
            }
            Terminal::Return | Terminal::Escape | Terminal::Unreachable => {}
        }
    }

    /// The declaration a designated role of `std` or `strand` is, if it's checked
    fn designated(&self, role: Designated) -> Option<DeclId> {
        (self.tables.designated.iter())
            .find(|&(_, &designated)| designated == role)
            .map(|(&decl, _)| decl)
    }

    /// A designated class's instance type, or `Unknown` if it isn't checked
    fn designated_type(&self, role: Designated) -> TypeId {
        self.designated(role)
            .map_or(self.db.unknown(), |decl| self.db.intern(Type::Decl(decl)))
    }

    fn intrinsic(&self, intrinsic: Intrinsic) -> TypeId {
        self.db.intrinsic(intrinsic).unwrap_or(self.db.unknown())
    }

    fn literal(&self, literal: &Literal) -> TypeId {
        self.db.intern(Type::Literal(literal.clone()))
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

/// How many operand holes an expression has
fn holes(expr: &Expr) -> usize {
    let mut count = 0;
    expr.walk(&mut |expr| count += usize::from(matches!(expr.kind, ExprKind::Operand)));
    count
}

fn has_rule(expr: &Expr) -> bool {
    let mut found = false;
    expr.walk(&mut |expr| found |= expr.rule().is_some());
    found
}

/// Whether a step can throw: it has a rule, which may call or fail, or it unpacks
fn throws(step: &Step) -> bool {
    match step {
        Step::Let { pattern, value } => matches!(pattern, Pattern::Unpack(_)) || has_rule(value),
        Step::Assign { target, value } => !matches!(target, Target::Var(_)) || has_rule(value),
        Step::Default { value, .. } | Step::Eval(value) | Step::Push(value) => has_rule(value),
        Step::Dup | Step::Pop | Step::Assume(_) => false,
    }
}

/// Whether a step can call arbitrary code, which may run a closure
fn calls(step: &Step) -> bool {
    match step {
        Step::Assign { target, .. } if !matches!(target, Target::Var(_)) => true,
        step => throws(step),
    }
}

/// Each block's place in the queue, its reverse postorder index over every edge
/// from each function's entry in turn, and whether an edge to it retreats in that
/// order, which makes it a widening point. Blocks no edge reaches come last.
fn order(ir: &Ir) -> (Vec<u32>, Vec<bool>) {
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
    let mut widens = vec![false; count];
    for (block, _) in ir.blocks() {
        for target in successors(block) {
            if rank[target.index()] <= rank[block.index()] {
                widens[target.index()] = true;
            }
        }
    }
    (rank, widens)
}
