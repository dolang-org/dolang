//! Checking rules. Each run of a rule solves its constraints with a fresh solver,
//! and only reified types leave it.
//!
//! A rule contributes its results only once it's decided: solved without
//! contradiction, with every result solved without defaulting. Until then it
//! contributes bottom, which adds nothing and never has to be retracted. When the
//! queue empties, the earliest block of each function whose latest run left a rule
//! undecided runs again, once, defaulting the unsolved variables of each rule it
//! leaves undecided, upstream first; a variable with no lower bounds becomes
//! dynamic. The final pass defaults every rule it leaves undecided. A rule's
//! results are its latest run's: the analysis converges because the joins at block
//! entries and accumulators widen.
//!
//! A value of bottom type is never produced, so a rule with such an input doesn't
//! run: its results are bottom. An item of a comprehension is the exception, since
//! bottom only says that it occurs zero times.

use std::collections::VecDeque;

use super::{
    At, Flow, State,
    problem::{Misfit, Problem},
};
use crate::{
    source::Span,
    typeck::{
        cfg::{Collection, Expr, ExprKind, FuncId, Item, Pattern, PatternItem, PatternKey, VarId},
        elab::Designated,
        solver::{
            CallArgument, Contradiction, Issue, ObligationId, Outcome, Provenance, Solver, Status,
            Step as Derivation, Term,
        },
        r#type::{
            Argument, BoundRef, Database, DeclId, Element, Function, Kind, Literal, Multiplicity,
            Rest, SchemaItem, SymbolId, Type, TypeId, UnionMember,
        },
    },
};

/// What a constraint of a rule checks, to diagnose it by
#[derive(Clone)]
enum Check {
    /// A callee against the call, by the call's span and each argument's
    Call { span: Span, args: Vec<Span> },
    /// A value that must be something
    Fits(Span, Misfit),
    /// A value that must be what's expected of it
    Expected(Span),
    /// Nothing that can fail
    Quiet,
}

/// A rule's constraints, as they are made
struct Rule<'s, 'a> {
    solver: &'s mut Solver<'a>,
    db: &'a Database,
    checks: Vec<Check>,
    /// The variables standing for what a call passes the `do` blocks it's given,
    /// by the signature variable each joins into
    passed: Vec<(VarId, Term)>,
    /// The variables standing for the results of `do` blocks not yet known
    pending: Vec<Term>,
}

impl<'a> Rule<'_, 'a> {
    fn constrain(&mut self, actual: Term, expected: Term, check: Check) {
        self.solver
            .constrain(actual, expected, Provenance::default());
        self.checks.push(check);
    }

    fn closed(&self, ty: TypeId) -> Term {
        self.solver.closed(ty)
    }

    /// A `do` block's function type, with a fresh variable in each hole
    fn lambda(&mut self, lambda: &Lambda) -> Term {
        let group = (lambda.holes.iter())
            .map(|&hole| {
                let term = self.solver.infer();
                match hole {
                    Hole::Passed(var) => self.passed.push((var, term)),
                    Hole::Result => self.pending.push(term),
                }
                term
            })
            .collect();
        let environment = self
            .solver
            .environment(self.solver.empty_environment(), group);
        self.solver.view(lambda.ty, environment)
    }

    /// A call's arguments, or a collection's items, as often as each is passed,
    /// with the span each is diagnosed at: its own, its group's, or `fallback`
    fn arguments(
        &mut self,
        values: &Values,
        spread: Option<DeclId>,
        fallback: Span,
    ) -> (Vec<(Multiplicity, CallArgument)>, Vec<Span>) {
        let rule = self;
        // A spread's schema is the least its value spreads as, solved before the
        // rest, which can't show that an unsolved schema supplies what's expected
        let spreads: Vec<Term> = (values.values.iter())
            .filter_map(|placed| match placed.value {
                Value::Spread(ty, span) => {
                    let schema = rule.solver.infer_kind(Kind::Schema, Rest::All);
                    spread_into(rule, spread, ty, schema, span);
                    Some(schema)
                }
                _ => None,
            })
            .collect();
        if !spreads.is_empty() {
            rule.solver.solve();
        }
        let mut solved = Vec::new();
        for schema in spreads {
            if let Term::Infer(id) = schema {
                let _ = rule.solver.default(id);
            }
            let reified = rule.solver.reify(schema).ok();
            solved.push((reified.map_or(schema, |ty| rule.closed(ty)), reified));
        }
        let mut spreads = solved.into_iter();
        let unknown_schema = rule.db.unknown_of(Kind::Schema);
        let mut groups: Vec<Option<Joined>> = (values.groups.iter()).map(|_| None).collect();
        // Each argument, or a group whose arguments go where its first item is
        let mut entries = Vec::new();
        let symbol = |key| rule.db.intern(Type::Literal(Literal::Sym(key)));
        for placed in &values.values {
            let multiplicity = placed.multiplicity;
            // A spread gives its schema's items, unless it's passed once, so
            // that none is an inclusion the solver can't repeat or leave out
            let mut flat = Vec::new();
            if let Value::Spread(_, span) = placed.value {
                let (term, schema) = spreads.next().expect("a schema for each spread");
                if multiplicity == Multiplicity::Required {
                    let argument = CallArgument::Spread(term);
                    entries.push(Entry::Argument(multiplicity, argument, span));
                    continue;
                }
                let schema = schema.unwrap_or(unknown_schema);
                atoms(rule.db, schema, multiplicity, &mut flat);
            }
            if let Some(group) = placed.group {
                let joined = group_entry(&mut groups, &mut entries, group);
                match placed.value {
                    Value::Pos(ty, _) => joined.positional.add(rule, Part::Closed(ty)),
                    Value::Key(key, ty, _) => joined.keyed(rule, symbol(key), Part::Closed(ty)),
                    Value::Pair(key, ty) => joined.keyed(rule, key, Part::Closed(ty)),
                    Value::Lambda(key, ref lambda, _) => {
                        let term = Part::Term(rule.lambda(lambda));
                        match key {
                            Some(key) => joined.keyed(rule, symbol(key), term),
                            None => joined.positional.add(rule, term),
                        }
                    }
                    Value::Spread(..) => {
                        for (_, atom) in flat {
                            joined.atom(rule, atom);
                        }
                    }
                }
                continue;
            }
            let arguments = match placed.value {
                Value::Pos(ty, span) => {
                    vec![(
                        multiplicity,
                        CallArgument::Positional(rule.closed(ty)),
                        span,
                    )]
                }
                Value::Key(key, ty, span) => {
                    vec![(
                        multiplicity,
                        CallArgument::Keyword(key, rule.closed(ty)),
                        span,
                    )]
                }
                Value::Pair(key, ty) => {
                    let argument = CallArgument::Pair(rule.closed(key), rule.closed(ty));
                    vec![(multiplicity, argument, fallback)]
                }
                Value::Lambda(key, ref lambda, span) => {
                    let term = rule.lambda(lambda);
                    let argument = match key {
                        Some(key) => CallArgument::Keyword(key, term),
                        None => CallArgument::Positional(term),
                    };
                    vec![(multiplicity, argument, span)]
                }
                Value::Spread(_, span) => (flat.into_iter())
                    .map(|(multiplicity, atom)| {
                        let argument = match atom {
                            Atom::Positional(ty) => CallArgument::Positional(rule.closed(ty)),
                            Atom::Keyed(key, ty) => {
                                CallArgument::Pair(rule.closed(key), rule.closed(ty))
                            }
                            Atom::Unknown => CallArgument::Spread(rule.closed(unknown_schema)),
                        };
                        (multiplicity, argument, span)
                    })
                    .collect(),
            };
            for (multiplicity, argument, span) in arguments {
                entries.push(Entry::Argument(multiplicity, argument, span));
            }
        }
        let mut arguments = Vec::new();
        let mut spans = Vec::new();
        for entry in entries {
            match entry {
                Entry::Argument(multiplicity, argument, span) => {
                    arguments.push((multiplicity, argument));
                    spans.push(span);
                }
                Entry::Group(group) => {
                    let joined = groups[group].as_ref().expect("a group's items");
                    for argument in joined.arguments(rule) {
                        arguments.push((Multiplicity::Repeated, argument));
                        spans.push(values.groups[group]);
                    }
                }
            }
        }
        (arguments, spans)
    }

    /// A term for the type `build` makes, whose holes the terms it passes fill
    fn term(&mut self, build: impl FnOnce(&mut Holes<'a>) -> TypeId) -> Term {
        let mut holes = Holes {
            db: self.db,
            group: Vec::new(),
        };
        let ty = build(&mut holes);
        let environment = self
            .solver
            .environment(self.solver.empty_environment(), holes.group);
        self.solver.view(ty, environment)
    }
}

/// Builds a type with holes: bound references at depth 0 into a group of terms
struct Holes<'a> {
    db: &'a Database,
    group: Vec<Term>,
}

impl Holes<'_> {
    fn hole(&mut self, term: Term, kind: Kind) -> TypeId {
        self.group.push(term);
        self.db.intern(Type::Bound {
            reference: BoundRef::new(0, self.group.len() - 1),
            kind,
        })
    }

    /// A class applied to one argument for each of its binders
    fn apply(&self, class: DeclId, args: Vec<TypeId>) -> TypeId {
        self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(class)),
            args: args.into_iter().map(Argument::Positional).collect(),
            kind: Kind::Type,
        })
    }

    fn schema(&self, items: Vec<SchemaItem>) -> TypeId {
        self.db.intern(Type::Schema(items.into()))
    }
}

fn item(multiplicity: Multiplicity, element: Element) -> SchemaItem {
    SchemaItem {
        multiplicity,
        element,
    }
}

/// An evaluated item of an argument list or collection
enum Value {
    Pos(TypeId, Span),
    Key(SymbolId, TypeId, Span),
    Pair(TypeId, TypeId),
    Spread(TypeId, Span),
    /// A `do` block that the rule types, positional or keyed
    Lambda(Option<SymbolId>, Lambda, Span),
}

/// A `do` block passed to a rule, which types it: its function type, with a hole
/// at depth 0 for each item its signature leaves to the rule
pub(super) struct Lambda {
    func: FuncId,
    ty: TypeId,
    holes: Vec<Hole>,
}

#[derive(Clone, Copy)]
enum Hole {
    /// A parameter's or channel's type, joined into its signature variable
    Passed(VarId),
    /// The block's result, until it's known
    Result,
}

/// An evaluated item, and how often it occurs
struct Placed {
    value: Value,
    multiplicity: Multiplicity,
    /// The outermost `for` it's in, by index into [`Values::groups`]. The items of
    /// a `for` join into one repeated item of each kind.
    group: Option<usize>,
}

/// The evaluated items of an argument list or collection
#[derive(Default)]
struct Values {
    values: Vec<Placed>,
    /// Each outermost `for`'s span
    groups: Vec<Span>,
    /// Whether a value isn't produced, so neither is the whole
    never: bool,
    /// Whether it has a comprehension
    comprehension: bool,
}

impl Values {
    fn lambdas(&self) -> impl Iterator<Item = &Lambda> {
        (self.values.iter()).filter_map(|placed| match &placed.value {
            Value::Lambda(_, lambda, _) => Some(lambda),
            _ => None,
        })
    }
}

/// Where items are gathered: how often they occur, relative to the `if` branch
/// they're in, unless they're in a `for`
#[derive(Clone, Copy)]
struct Place {
    multiplicity: Multiplicity,
    group: Option<usize>,
    /// Whether they're in a comprehension, where a value that isn't produced
    /// occurs zero times
    comprehension: bool,
}

/// What items are gathered against
struct Gathering<'p> {
    /// What each positional or keyed item is expected to be
    params: Option<&'p Params>,
    /// Whether a `do` block among them is left for the rule to type
    contextual: bool,
    /// The next positional item's position, or, unless `exact`, the least it
    /// can be
    position: usize,
    exact: bool,
}

impl Gathering<'_> {
    /// What the next positional item is expected to be. Past the parameters that
    /// take one item each, every item is expected to be the rest's, wherever
    /// exactly it lands.
    fn expected(&self) -> Option<TypeId> {
        let params = self.params?;
        match params.positional.get(self.position) {
            Some(&ty) if self.exact => ty,
            Some(_) => None,
            None => params.rest,
        }
    }

    /// Count a positional item. One in a comprehension may not occur.
    fn advance(&mut self, place: Place) {
        if !place.comprehension {
            self.position += 1;
        }
    }
}

/// A `for`'s items in a call, each kind joined into one repeated item
#[derive(Default)]
struct Joined {
    positional: Slot,
    /// By literal key
    keyed: Vec<(TypeId, Slot)>,
    /// Every other key, and its values
    pair: Option<(TypeId, Slot)>,
    /// Whether it spreads a schema whose items aren't known
    unknown: bool,
}

/// Values joined into one: their closed types' join, so that a mismatch names it,
/// and the terms of `do` blocks, which only a variable can join
#[derive(Default)]
struct Slot {
    closed: Option<TypeId>,
    terms: Vec<Term>,
}

/// A value joined into a [`Slot`]
#[derive(Clone, Copy)]
enum Part {
    Closed(TypeId),
    Term(Term),
}

impl Slot {
    fn add(&mut self, rule: &Rule<'_, '_>, part: Part) {
        match part {
            Part::Closed(ty) => {
                self.closed = Some(match self.closed {
                    Some(joined) => rule.solver.lub(joined, ty),
                    None => ty,
                });
            }
            Part::Term(term) => self.terms.push(term),
        }
    }

    /// Its values' join, if any value reaches it
    fn term(&self, rule: &mut Rule<'_, '_>) -> Option<Term> {
        let closed = self.closed.map(|ty| rule.closed(ty));
        if self.terms.is_empty() {
            return closed;
        }
        let var = rule.solver.infer();
        for &term in closed.iter().chain(&self.terms) {
            rule.constrain(term, var, Check::Quiet);
        }
        Some(var)
    }
}

impl Joined {
    fn keyed(&mut self, rule: &Rule<'_, '_>, key: TypeId, value: Part) {
        if let Type::Literal(_) = rule.db.ty(key) {
            let index = match self.keyed.iter().position(|&(other, _)| other == key) {
                Some(index) => index,
                None => {
                    self.keyed.push((key, Slot::default()));
                    self.keyed.len() - 1
                }
            };
            self.keyed[index].1.add(rule, value);
            return;
        }
        let (keys, values) = self.pair.get_or_insert_with(|| (key, Slot::default()));
        *keys = rule.solver.lub(*keys, key);
        values.add(rule, value);
    }

    fn atom(&mut self, rule: &Rule<'_, '_>, atom: Atom) {
        match atom {
            Atom::Positional(ty) => self.positional.add(rule, Part::Closed(ty)),
            Atom::Keyed(key, value) => self.keyed(rule, key, Part::Closed(value)),
            Atom::Unknown => self.unknown = true,
        }
    }

    /// Its repeated arguments
    fn arguments(&self, rule: &mut Rule<'_, '_>) -> Vec<CallArgument> {
        let mut arguments: Vec<_> = (self.positional.term(rule))
            .map(CallArgument::Positional)
            .into_iter()
            .collect();
        for (key, slot) in &self.keyed {
            let value = slot.term(rule).expect("a value for each key");
            arguments.push(CallArgument::Pair(rule.closed(*key), value));
        }
        if let Some((key, slot)) = &self.pair {
            let value = slot.term(rule).expect("a value for each key");
            arguments.push(CallArgument::Pair(rule.closed(*key), value));
        }
        if self.unknown {
            let unknown = rule.db.unknown_of(Kind::Schema);
            arguments.push(CallArgument::Spread(rule.closed(unknown)));
        }
        arguments
    }
}

/// An argument of a call, or where a group's go
enum Entry {
    Argument(Multiplicity, CallArgument, Span),
    Group(usize),
}

/// A group's joined items, entered where its first item is
fn group_entry<'g>(
    groups: &'g mut [Option<Joined>],
    entries: &mut Vec<Entry>,
    group: usize,
) -> &'g mut Joined {
    groups[group].get_or_insert_with(|| {
        entries.push(Entry::Group(group));
        Joined::default()
    })
}

/// An item of a solved schema, flattened
#[derive(Clone, Copy)]
enum Atom {
    Positional(TypeId),
    Keyed(TypeId, TypeId),
    /// A schema whose items aren't known
    Unknown,
}

/// A solved schema's items included `multiplicity` times, flattened, so that none
/// is an inclusion that the solver can't repeat or make optional
fn atoms(
    db: &Database,
    schema: TypeId,
    multiplicity: Multiplicity,
    atoms: &mut Vec<(Multiplicity, Atom)>,
) {
    let Type::Schema(items) = db.ty(schema) else {
        atoms.push((multiplicity, Atom::Unknown));
        return;
    };
    for item in items.iter() {
        let multiplicity = multiplicity.compose(item.multiplicity);
        match item.element {
            Element::Positional(ty) => atoms.push((multiplicity, Atom::Positional(ty))),
            Element::Keyed { key, value } => atoms.push((multiplicity, Atom::Keyed(key, value))),
            Element::Include(schema) => self::atoms(db, schema, multiplicity, atoms),
        }
    }
}

/// What a call can pass its arguments against, when the callee's parameters are
/// known from its signature alone
#[derive(Default)]
struct Params {
    positional: Vec<Option<TypeId>>,
    /// The type of every positional argument past `positional`
    rest: Option<TypeId>,
    keyed: Vec<(SymbolId, TypeId)>,
}

impl<'a> Flow<'a, '_> {
    /// Run a rule: solve the constraints `build` makes and conclude its results in
    /// this context, replacing what its earlier runs concluded. `expected` is
    /// pre-seeded as an upper bound on the first result, unless that contradicts:
    /// then the check against the expectation reports it instead. A rule left
    /// undecided is solved again with defaulting if its block defaults, or in the
    /// final pass; otherwise it marks its block undecided.
    fn conclude(
        &mut self,
        at: At,
        expected: Option<TypeId>,
        build: impl Fn(&mut Rule<'_, 'a>) -> Vec<Term>,
    ) -> Vec<TypeId> {
        let run = |seed: bool, default: bool| {
            let mut solver = self.solver();
            let mut rule = Rule {
                solver: &mut solver,
                db: self.db,
                checks: Vec::new(),
                passed: Vec::new(),
                pending: Vec::new(),
            };
            let results = build(&mut rule);
            let checks = rule.checks;
            let lambdas = (rule.passed, rule.pending);
            let seeded = match expected {
                Some(expected) if seed => {
                    solver.constrain(results[0], solver.closed(expected), Provenance::default());
                    true
                }
                _ => false,
            };
            let outcomes = if default {
                default_all(&mut solver, self.db, true)
            } else {
                solver.solve()
            };
            let rejected = seeded && outcomes[checks.len()].status == Status::Contradicted;
            (solver, results, checks, lambdas, outcomes, rejected)
        };
        let attempt = |default: bool| {
            let (solver, results, checks, lambdas, outcomes, rejected) = run(true, default);
            if rejected {
                let (solver, results, checks, lambdas, outcomes, _) = run(false, default);
                return (solver, results, checks, lambdas, outcomes);
            }
            (solver, results, checks, lambdas, outcomes)
        };
        let (mut solver, mut results, mut checks, mut lambdas, mut outcomes) = attempt(false);
        let contradicted =
            (outcomes[..checks.len()].iter()).any(|outcome| outcome.status == Status::Contradicted);
        // A check left unresolved may be decided by defaulting
        let unresolved =
            (outcomes[..checks.len()].iter()).any(|outcome| outcome.status == Status::Unresolved);
        let reified: Option<Vec<TypeId>> = (results.iter())
            .map(|&term| solver.reify(term).ok())
            .collect();
        let values: Vec<TypeId> = match reified {
            Some(values) if !contradicted && !unresolved => values,
            _ if self.defaulting || self.observing() => {
                (solver, results, checks, lambdas, outcomes) = attempt(true);
                (results.iter())
                    .map(|&term| solver.reify(term).unwrap_or(self.db.unknown()))
                    .collect()
            }
            _ => {
                self.unsettled.insert((at.block, at.ctx));
                vec![self.db.bottom(); results.len()]
            }
        };
        if self.observing() {
            self.blame(&solver, &checks, &outcomes[..checks.len()]);
            return values;
        }
        let (passed, pending) = lambdas;
        for (var, ty) in self::passed(&mut solver, self.db, &passed, &pending) {
            self.join(var, ty);
        }
        values
    }

    /// Diagnose the checks a rule's final run contradicted, and record the ones it
    /// couldn't decide
    fn blame(&mut self, solver: &Solver<'_>, checks: &[Check], outcomes: &[Outcome]) {
        for (check, outcome) in checks.iter().zip(outcomes) {
            let span = match *check {
                Check::Call { span, .. } | Check::Fits(span, _) | Check::Expected(span) => span,
                Check::Quiet => continue,
            };
            match outcome.status {
                Status::Proven => {}
                Status::Unresolved => self.undecided(span, super::residual(outcome)),
                Status::Contradicted => {
                    let relation = solver.obligation(root(outcome)).relation;
                    // The callee, or the value that doesn't fit
                    let actual = self.render_term(solver, relation.actual);
                    let problems: Vec<Problem> = match check {
                        Check::Call { span, args } => (outcome.diagnostics.iter())
                            .filter_map(|diagnostic| {
                                let Issue::Contradiction(contradiction) = diagnostic.issue else {
                                    return None;
                                };
                                Some(self.call_problem(
                                    solver,
                                    *span,
                                    args,
                                    &diagnostic.path,
                                    contradiction,
                                    &actual,
                                ))
                            })
                            .collect(),
                        &Check::Fits(span, misfit) => vec![Problem::Misfit {
                            span,
                            found: actual.clone().unwrap_or_else(|| "?".to_owned()),
                            misfit,
                        }],
                        &Check::Expected(span) => vec![Problem::Argument {
                            span,
                            found: actual.clone().unwrap_or_else(|| "?".to_owned()),
                            expected: self.render_term(solver, relation.expected),
                        }],
                        Check::Quiet => Vec::new(),
                    };
                    for problem in problems {
                        self.problem(problem);
                    }
                }
            }
        }
    }

    /// What a contradiction under a call's constraint says, through the path of
    /// derivations to it
    fn call_problem(
        &self,
        solver: &Solver<'_>,
        span: Span,
        args: &[Span],
        path: &[ObligationId],
        contradiction: Contradiction,
        callee: &Option<String>,
    ) -> Problem {
        let steps: Vec<Derivation> = (path.windows(2))
            .filter_map(|pair| {
                (solver.obligation(pair[0]).dependencies.iter())
                    .find(|dependency| dependency.obligation == pair[1])
                    .map(|dependency| dependency.step.clone())
            })
            .collect();
        let fallback = Problem::Call {
            span,
            callee: callee.clone().unwrap_or_else(|| "?".to_owned()),
        };
        if steps.len() + 1 != path.len() {
            return fallback;
        }
        let Some(params) = steps
            .iter()
            .position(|step| *step == Derivation::Parameters)
        else {
            return fallback;
        };
        match steps.get(params + 1) {
            Some(&(Derivation::Item(index) | Derivation::Key(index))) => {
                let Some(&arg) = args.get(index) else {
                    return fallback;
                };
                let relation = solver.obligation(path[params + 2]).relation;
                let Some(found) = self.render_term(solver, relation.actual) else {
                    return fallback;
                };
                Problem::Argument {
                    span: arg,
                    found,
                    expected: self.render_term(solver, relation.expected),
                }
            }
            None => match contradiction {
                Contradiction::Excess(index) => {
                    Problem::ExtraArgument(args.get(index).copied().unwrap_or(span))
                }
                Contradiction::Missing(_) => Problem::MissingArgument(span),
                _ => fallback,
            },
            Some(_) => fallback,
        }
    }

    /// A term's type as a diagnostic shows it, if it's solved
    fn render_term(&self, solver: &Solver<'_>, term: Term) -> Option<String> {
        let ty = solver.reify(term).ok()?;
        Some(self.tables.render_type(self.db, ty))
    }

    /// A call: its callee's type below the function type its arguments call it as.
    /// A `do` block among them enters as its function type, with a fresh variable
    /// for each parameter and channel its signature leaves open. What the callee
    /// passes them joins into its signature, and its result, until the block's
    /// analysis gives it, is a variable that keeps the call undecided. A
    /// comprehension's arguments are passed as often as it says: see
    /// [`Flow::gather`]. A callee that isn't a function or a union of them gives
    /// the dynamic type.
    pub(super) fn call(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Call { callee, args, .. } = &expr.kind else {
            unreachable!("a call")
        };
        let bottom = self.db.bottom();
        let unknown = self.db.unknown();
        let callee_type = self.eval(at, state, operands, callee);
        let (input, output) = self.channels(at);
        let params = self.params(callee_type, input, output).unwrap_or_default();
        let values = self.values(at, state, operands, args, Some(&params), true);
        if callee_type == bottom || values.never {
            return bottom;
        }
        let function = |ty: TypeId| {
            let ty = match self.db.ty(ty) {
                Type::Quantified { body, .. } => *body,
                _ => ty,
            };
            matches!(self.db.ty(ty), Type::Function(_))
        };
        // A union of functions, as a variable assigned several closures holds
        let callable = match self.db.ty(callee_type) {
            Type::Union(members) => members.iter().all(|member| match *member {
                UnionMember::Type(ty) => function(ty),
                UnionMember::Expand(_) => false,
            }),
            _ => function(callee_type),
        };
        if !callable {
            self.untyped(at, &values);
            return unknown;
        }
        let spread = self.designated(Designated::Spread);
        let span = expr.span;
        self.conclude(at, expected, |rule| {
            let (arguments, spans) = rule.arguments(&values, spread, span);
            let result = rule.solver.infer();
            let call = rule.solver.call_items(
                &arguments,
                result,
                input.map(|ty| rule.closed(ty)),
                output.map(|ty| rule.closed(ty)),
            );
            rule.constrain(
                rule.closed(callee_type),
                call,
                Check::Call { span, args: spans },
            );
            vec![result]
        })[0]
    }

    /// A `do` block as a call's argument: its declared function type, with a hole
    /// for each parameter and channel its signature leaves to the call, and for
    /// its result until that's known. A rest's element type isn't left to the
    /// call, so it's dynamic. `None` if the block has no signature, or its declared
    /// type doesn't line up with its parameters.
    fn contextual(&mut self, at: At, func: FuncId) -> Option<Lambda> {
        let (db, ir) = (self.db, self.ir);
        let data = ir.func(func);
        let signature = data.signature.as_ref()?;
        let declared = self.declared[func.index()].clone()?;
        let (Pattern::Unpack(pattern), Type::Schema(items)) =
            (&data.params, db.ty(declared.params))
        else {
            return None;
        };
        let mut holes = Vec::new();
        let hole = |holes: &mut Vec<Hole>, filled| {
            holes.push(filled);
            db.intern(Type::Bound {
                reference: BoundRef::new(0, holes.len() - 1),
                kind: Kind::Type,
            })
        };
        // A rest can give several schema items, and every other parameter one
        let mut singles = (pattern.iter().zip(&signature.params))
            .filter(|(item, _)| !matches!(item.key, PatternKey::Rest(_)))
            .map(|(_, &var)| var);
        let mut schema = Vec::with_capacity(items.len());
        for item in items.iter() {
            let rest = item.multiplicity == Multiplicity::Repeated
                || matches!(item.element, Element::Include(_));
            let passed = match rest {
                true => None,
                false => singles.next()?,
            };
            let element = match (passed, &item.element) {
                (None, element) => element.clone(),
                (Some(var), Element::Positional(_)) => {
                    Element::Positional(hole(&mut holes, Hole::Passed(var)))
                }
                (Some(var), &Element::Keyed { key, .. }) => Element::Keyed {
                    key,
                    value: hole(&mut holes, Hole::Passed(var)),
                },
                (Some(_), Element::Include(_)) => unreachable!("not a rest"),
            };
            schema.push(self::item(item.multiplicity, element));
        }
        if singles.next().is_some() {
            return None;
        }
        let mut channel = |var: Option<VarId>, declared: Option<TypeId>| match var {
            Some(var) => Some(hole(&mut holes, Hole::Passed(var))),
            None => declared,
        };
        let input = channel(signature.input, declared.input);
        let output = channel(signature.output, declared.output);
        let result = match signature.result {
            Some(var) => match self.joined(var, at) {
                ty if ty == db.bottom() => hole(&mut holes, Hole::Result),
                ty => ty,
            },
            None => declared.result,
        };
        let ty = db.intern(Type::Function(Function {
            params: db.intern(Type::Schema(schema.into())),
            result,
            input,
            output,
        }));
        let unknown = db.unknown();
        for (item, &var) in pattern.iter().zip(&signature.params) {
            if let (PatternKey::Rest(_), Some(var)) = (&item.key, var) {
                self.join(var, unknown);
            }
        }
        Some(Lambda { func, ty, holes })
    }

    /// The parameter types a callee's signature alone gives, which don't mention its
    /// binders. Any other parameter's type is only known once the call is solved.
    /// The binders a callee takes as its channels are the caller's `input` and
    /// `output`, as the call passes them, so a callback sharing them is known;
    /// other binders wait for expectations from the call's solve (#801).
    fn params(
        &self,
        callee: TypeId,
        input: Option<TypeId>,
        output: Option<TypeId>,
    ) -> Option<Params> {
        let db = self.db;
        let mut ty = callee;
        if let Type::Quantified { binders, body } = db.ty(ty) {
            let Type::Function(function) = db.ty(*body) else {
                return None;
            };
            let channel = |channel: Option<TypeId>, slot: usize| {
                matches!(
                    channel.map(|ty| db.ty(ty)),
                    Some(&Type::Bound { reference, .. })
                        if reference.depth == 0 && usize::from(reference.slot) == slot
                )
            };
            // Each other binder stays a reference to its own slot, so it's not fixed
            let args: Vec<TypeId> = (binders.iter().enumerate())
                .map(|(slot, binder)| {
                    let caller = if channel(function.input, slot) {
                        input
                    } else if channel(function.output, slot) {
                        output
                    } else {
                        None
                    };
                    caller.unwrap_or_else(|| {
                        db.intern(Type::Bound {
                            reference: BoundRef::new(0, slot),
                            kind: binder.kind,
                        })
                    })
                })
                .collect();
            ty = db.substitute(*body, &args);
        }
        let Type::Function(function) = self.db.ty(ty) else {
            return None;
        };
        let Type::Schema(items) = self.db.ty(function.params) else {
            return None;
        };
        let fixed = |ty: TypeId| {
            let mut free = true;
            self.db.walk(ty, |node, depth| {
                if let Type::Bound { reference, .. } = *self.db.ty(node) {
                    free &= u32::from(reference.depth) < depth;
                }
            });
            free.then_some(ty)
        };
        let mut params = Params::default();
        let mut open = true;
        for item in items.iter() {
            match (item.multiplicity, &item.element) {
                (Multiplicity::Repeated, &Element::Positional(ty)) if open => {
                    params.rest = fixed(ty);
                    open = false;
                }
                (_, &Element::Positional(ty)) if open => params.positional.push(fixed(ty)),
                (_, Element::Positional(_) | Element::Include(_)) => open = false,
                (_, &Element::Keyed { key, value }) => {
                    if let (&Type::Literal(Literal::Sym(key)), Some(value)) =
                        (self.db.ty(key), fixed(value))
                    {
                        params.keyed.push((key, value));
                    }
                }
            }
        }
        Some(params)
    }

    /// Evaluate the items of an argument list or collection in order, each
    /// expecting what `params` gives its position. With `contextual`, a `do` block
    /// among them is left for the rule to type.
    fn values(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &[Item],
        params: Option<&Params>,
        contextual: bool,
    ) -> Values {
        let mut values = Values::default();
        let mut gathering = Gathering {
            params,
            contextual,
            position: 0,
            exact: true,
        };
        let place = Place {
            multiplicity: Multiplicity::Required,
            group: None,
            comprehension: false,
        };
        let mut placed = Vec::new();
        self.gather(
            at,
            state,
            operands,
            items,
            &mut gathering,
            place,
            &mut values,
            &mut placed,
        );
        values.values = placed;
        values
    }

    /// Gather items into `out`, as they occur at `place`. An `if` outside every
    /// `for` gives its branches' items once each, if they're alike, and otherwise
    /// makes them optional.
    #[expect(clippy::too_many_arguments, reason = "one recursion's state")]
    fn gather(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &[Item],
        gathering: &mut Gathering<'_>,
        place: Place,
        values: &mut Values,
        out: &mut Vec<Placed>,
    ) {
        let bottom = self.db.bottom();
        let produced = |values: &mut Values, ty: TypeId| {
            let never = ty == bottom;
            values.never |= never && !place.comprehension;
            !never
        };
        let placed = |value| Placed {
            value,
            multiplicity: place.multiplicity,
            group: place.group,
        };
        for item in items {
            // A rule types a `do` block it's given
            let lambda = match item {
                Item::Pos(value) | Item::Key(_, value) if gathering.contextual => {
                    match value.kind {
                        ExprKind::Lambda(func) => self.contextual(at, func),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(lambda) = lambda {
                let (key, value) = match item {
                    Item::Pos(value) => {
                        gathering.advance(place);
                        (None, value)
                    }
                    &Item::Key(key, ref value) => (Some(key), value),
                    _ => unreachable!("a positional or keyed item"),
                };
                out.push(placed(Value::Lambda(key, lambda, value.span)));
                continue;
            }
            match item {
                Item::Pos(value) => {
                    let expected = gathering.expected();
                    gathering.advance(place);
                    let ty = self.expect(at, state, operands, value, expected);
                    if produced(values, ty) {
                        out.push(placed(Value::Pos(ty, value.span)));
                    }
                }
                &Item::Key(key, ref value) => {
                    let expected = gathering.params.and_then(|params| {
                        (params.keyed.iter()).find_map(|&(name, ty)| (name == key).then_some(ty))
                    });
                    let ty = self.expect(at, state, operands, value, expected);
                    if produced(values, ty) {
                        out.push(placed(Value::Key(key, ty, value.span)));
                    }
                }
                Item::Pair(key, value) => {
                    let key_type = self.eval(at, state, operands, key);
                    let ty = self.eval(at, state, operands, value);
                    if produced(values, key_type) && produced(values, ty) {
                        out.push(placed(Value::Pair(key_type, ty)));
                    }
                }
                Item::Spread(value) => {
                    gathering.exact = false;
                    let ty = self.eval(at, state, operands, value);
                    if produced(values, ty) {
                        out.push(placed(Value::Spread(ty, value.span)));
                    }
                }
                &Item::For { ref items, span } => {
                    values.comprehension = true;
                    gathering.exact = false;
                    let group = place.group.unwrap_or_else(|| {
                        values.groups.push(span);
                        values.groups.len() - 1
                    });
                    let inner = Place {
                        multiplicity: Multiplicity::Repeated,
                        group: Some(group),
                        comprehension: true,
                    };
                    self.gather(at, state, operands, items, gathering, inner, values, out);
                }
                &Item::If {
                    ref then,
                    ref else_,
                    span,
                } => {
                    values.comprehension = true;
                    gathering.exact = false;
                    if place.group.is_some() {
                        let inner = Place {
                            comprehension: true,
                            ..place
                        };
                        for items in [then, else_] {
                            self.gather(at, state, operands, items, gathering, inner, values, out);
                        }
                        continue;
                    }
                    let inner = Place {
                        multiplicity: Multiplicity::Required,
                        group: None,
                        comprehension: true,
                    };
                    let mut branches = [Vec::new(), Vec::new()];
                    for (items, branch) in [then, else_].into_iter().zip(&mut branches) {
                        self.gather(at, state, operands, items, gathering, inner, values, branch);
                    }
                    let [then, else_] = branches;
                    let arms = match self.alike(&then, &else_, span) {
                        Some(arms) => arms,
                        None => (then.into_iter().chain(else_))
                            .map(|placed| Placed {
                                multiplicity: Multiplicity::Optional.compose(placed.multiplicity),
                                ..placed
                            })
                            .collect(),
                    };
                    out.extend(arms.into_iter().map(|arm| Placed {
                        multiplicity: place.multiplicity.compose(arm.multiplicity),
                        ..arm
                    }));
                }
            }
        }
    }

    /// An `if`'s branches as one set of required items, if they're alike: each
    /// branch's items are required, and pairwise of the same kind and key. A joined
    /// value is diagnosed at the `if`.
    fn alike(&self, then: &[Placed], else_: &[Placed], span: Span) -> Option<Vec<Placed>> {
        if then.len() != else_.len() {
            return None;
        }
        (then.iter().zip(else_))
            .map(|(a, b)| {
                let required = Multiplicity::Required;
                if a.multiplicity != required || b.multiplicity != required {
                    return None;
                }
                let value = match (&a.value, &b.value) {
                    (&Value::Pos(x, _), &Value::Pos(y, _)) => Value::Pos(self.lub(x, y), span),
                    (&Value::Key(k, x, _), &Value::Key(l, y, _)) if k == l => {
                        Value::Key(k, self.lub(x, y), span)
                    }
                    (&Value::Pair(k, x), &Value::Pair(l, y)) => {
                        Value::Pair(self.lub(k, l), self.lub(x, y))
                    }
                    _ => return None,
                };
                Some(Placed {
                    value,
                    multiplicity: required,
                    group: None,
                })
            })
            .collect()
    }

    /// Instantiate each `do` block among values that no rule types
    fn untyped(&mut self, at: At, values: &Values) {
        for lambda in values.lambdas() {
            self.lambda(at, lambda.func);
        }
    }

    /// A collection literal of a designated class. An array joins its items into
    /// its element type, whether a comprehension repeats them or not, and so does a
    /// dict, unless something is expected of it: then its schema is its items'. A
    /// tuple
    /// or record takes its items' schema; neither has a vertical form, so neither
    /// holds a comprehension. With an expected type, the collection types the `do`
    /// blocks among its items.
    pub(super) fn collection(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Collection { kind, items, .. } = &expr.kind else {
            unreachable!("a collection")
        };
        let unknown = self.db.unknown();
        let role = match kind {
            Collection::Array => Designated::Array,
            Collection::Dict => Designated::Dict,
            Collection::Tuple => Designated::Tuple,
            Collection::Record => Designated::Record,
        };
        let class = self.designated(role);
        // An array expected to be `Array[E]` expects each item to be `E`
        let params = match kind {
            Collection::Array => expected.and_then(|ty| self.applied(class?, ty)),
            _ => None,
        }
        .map(|(_, element)| Params {
            rest: Some(element),
            ..Params::default()
        });
        let values = self.values(
            at,
            state,
            operands,
            items,
            params.as_ref(),
            expected.is_some(),
        );
        if values.never {
            return self.db.bottom();
        }
        let class = class.filter(|_| {
            !values.comprehension || matches!(kind, Collection::Array | Collection::Dict)
        });
        let Some(class) = class else {
            self.untyped(at, &values);
            return unknown;
        };
        let spread = self.designated(Designated::Spread);
        let int = self.intrinsic(crate::typeck::r#type::Intrinsic::Int);
        let expected_dict = match kind {
            Collection::Dict => expected.and_then(|ty| self.applied(class, ty)),
            _ => None,
        };
        let result = match (kind, expected_dict) {
            (Collection::Array, _) => self.conclude(at, expected, |rule| {
                let element = rule.solver.infer();
                for placed in &values.values {
                    match placed.value {
                        Value::Pos(ty, _) => rule.constrain(rule.closed(ty), element, Check::Quiet),
                        Value::Lambda(key, ref lambda, _) => {
                            let term = rule.lambda(lambda);
                            let element = match key {
                                Some(_) => rule.closed(unknown),
                                None => element,
                            };
                            rule.constrain(term, element, Check::Quiet);
                        }
                        Value::Spread(ty, span) => {
                            let schema = rule.term(|holes| {
                                let element = holes.hole(element, Kind::Type);
                                holes.schema(vec![item(
                                    Multiplicity::Repeated,
                                    Element::Positional(element),
                                )])
                            });
                            spread_into(rule, spread, ty, schema, span);
                        }
                        Value::Key(..) | Value::Pair(..) => {
                            rule.constrain(rule.closed(unknown), element, Check::Quiet);
                        }
                    }
                }
                vec![rule.term(|holes| {
                    let element = holes.hole(element, Kind::Type);
                    holes.apply(class, vec![element])
                })]
            }),
            // What's expected of a dict checks its items one by one, where joining
            // them would lose which is where
            (Collection::Dict, Some((ty, schema))) => self.conclude(at, expected, |rule| {
                let (arguments, _) = rule.arguments(&values, spread, expr.span);
                let exact = rule.solver.arguments_schema(&arguments);
                rule.constrain(exact, rule.closed(schema), Check::Expected(expr.span));
                vec![rule.closed(ty)]
            }),
            (Collection::Dict, None) => self.conclude(at, expected, |rule| {
                let keys = rule.solver.infer();
                let entries = rule.solver.infer();
                let entry = |rule: &mut Rule<'_, 'a>, key: TypeId, value: Term| {
                    rule.constrain(rule.closed(key), keys, Check::Quiet);
                    rule.constrain(value, entries, Check::Quiet);
                };
                let symbol = |name| rule.db.intern(Type::Literal(Literal::Sym(name)));
                for placed in &values.values {
                    match placed.value {
                        Value::Pos(ty, _) => entry(rule, int, rule.closed(ty)),
                        Value::Key(name, ty, _) => entry(rule, symbol(name), rule.closed(ty)),
                        Value::Pair(key_type, ty) => entry(rule, key_type, rule.closed(ty)),
                        Value::Lambda(key, ref lambda, _) => {
                            let term = rule.lambda(lambda);
                            entry(rule, key.map_or(int, symbol), term);
                        }
                        Value::Spread(ty, span) => {
                            let schema = rule.term(|holes| {
                                let key = holes.hole(keys, Kind::Type);
                                let value = holes.hole(entries, Kind::Type);
                                holes.schema(vec![item(
                                    Multiplicity::Repeated,
                                    Element::Keyed { key, value },
                                )])
                            });
                            spread_into(rule, spread, ty, schema, span);
                        }
                    }
                }
                vec![rule.term(|holes| {
                    let key = holes.hole(keys, Kind::Type);
                    let value = holes.hole(entries, Kind::Type);
                    let schema = holes.schema(vec![item(
                        Multiplicity::Repeated,
                        Element::Keyed { key, value },
                    )]);
                    holes.apply(class, vec![schema])
                })]
            }),
            (Collection::Tuple | Collection::Record, _) => {
                let lanes = match kind {
                    Collection::Tuple => Rest::Positional,
                    _ => Rest::All,
                };
                self.conclude(at, expected, |rule| {
                    let mut elements = Vec::new();
                    for placed in &values.values {
                        match placed.value {
                            Value::Lambda(key, ref lambda, _) => {
                                elements.push((key, rule.lambda(lambda), Kind::Type));
                            }
                            Value::Pos(ty, _) => {
                                let var = rule.solver.infer();
                                rule.constrain(rule.closed(ty), var, Check::Quiet);
                                elements.push((None, var, Kind::Type));
                            }
                            Value::Key(key, ty, _) => {
                                let var = rule.solver.infer();
                                rule.constrain(rule.closed(ty), var, Check::Quiet);
                                elements.push((Some(key), var, Kind::Type));
                            }
                            Value::Spread(ty, span) => {
                                let schema = rule.solver.infer_kind(Kind::Schema, lanes);
                                spread_into(rule, spread, ty, schema, span);
                                elements.push((None, schema, Kind::Schema));
                            }
                            Value::Pair(..) => unreachable!("a tuple or record has no pairs"),
                        }
                    }
                    vec![rule.term(|holes| {
                        let items = (elements.iter())
                            .map(|&(key, var, kind)| {
                                let hole = holes.hole(var, kind);
                                let element = match (key, kind) {
                                    (_, Kind::Schema) => Element::Include(hole),
                                    (Some(key), _) => Element::Keyed {
                                        key: holes.db.intern(Type::Literal(Literal::Sym(key))),
                                        value: hole,
                                    },
                                    (None, _) => Element::Positional(hole),
                                };
                                item(Multiplicity::Required, element)
                            })
                            .collect();
                        let schema = holes.schema(items);
                        holes.apply(class, vec![schema])
                    })]
                })
            }
        };
        result[0]
    }

    /// The one application of `class` to a single argument that `expected` is, or
    /// that is a member of it: the application, and its argument
    fn applied(&self, class: DeclId, expected: TypeId) -> Option<(TypeId, TypeId)> {
        let db = self.db;
        let applied = |ty: TypeId| match db.ty(ty) {
            Type::Apply { base, args, .. } if *db.ty(*base) == Type::Decl(class) => {
                match args[..] {
                    [Argument::Positional(arg)] => Some((ty, arg)),
                    _ => None,
                }
            }
            _ => None,
        };
        let Type::Union(members) = db.ty(expected) else {
            return applied(expected);
        };
        let mut found = (members.iter()).filter_map(|member| match *member {
            UnionMember::Type(ty) => applied(ty),
            UnionMember::Expand(_) => None,
        });
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// Values that must each fit a type, giving a known result. Without the type,
    /// a value is unchecked.
    fn fits(
        &mut self,
        at: At,
        values: &[(TypeId, Span)],
        ty: Option<TypeId>,
        misfit: Misfit,
        result: TypeId,
    ) -> TypeId {
        let bottom = self.db.bottom();
        if values.iter().any(|&(value, _)| value == bottom) {
            return bottom;
        }
        let Some(ty) = ty else {
            return result;
        };
        self.conclude(at, None, |rule| {
            for &(value, span) in values {
                rule.constrain(
                    rule.closed(value),
                    rule.closed(ty),
                    Check::Fits(span, misfit),
                );
            }
            vec![rule.closed(result)]
        })[0]
    }

    /// A binary string: each interpolated part must be binary
    pub(super) fn bin_concat(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        parts: &[Expr],
    ) -> TypeId {
        let values: Vec<_> = (parts.iter())
            .map(|part| (self.eval(at, state, operands, part), part.span))
            .collect();
        let bin = self.designated(Designated::Bin);
        let result = self.designated_type(Designated::Bin);
        let bin = bin.map(|decl| self.db.intern(Type::Decl(decl)));
        self.fits(at, &values, bin, Misfit::Binary, result)
    }

    /// An interpolation or a parameter hole: its specification's width and precision
    /// must be `Int`s
    pub(super) fn fmt(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
    ) -> TypeId {
        let (value, spec, role) = match &expr.kind {
            ExprKind::FmtValue { value, spec, .. } => (Some(&**value), spec, Designated::FmtValue),
            ExprKind::FmtParam { spec, .. } => (None, spec, Designated::FmtParam),
            _ => unreachable!("an interpolation or parameter hole"),
        };
        let bottom = self.db.bottom();
        let mut never = false;
        if let Some(value) = value {
            never |= self.eval(at, state, operands, value) == bottom;
        }
        let values: Vec<_> = ([&spec.width, &spec.precision].into_iter().flatten())
            .map(|part| (self.eval(at, state, operands, part), part.span))
            .collect();
        if never {
            return bottom;
        }
        let int = self.db.intrinsic(crate::typeck::r#type::Intrinsic::Int);
        let result = self.designated_type(role);
        self.fits(at, &values, int, Misfit::Int, result)
    }

    /// The type of the items a `for` iterates: its iteratee must be a
    /// `BaseIterable[T]`, giving `T`
    pub(super) fn next(&mut self, at: At, iterable: TypeId, span: Span) -> TypeId {
        if iterable == self.db.bottom() {
            return iterable;
        }
        let Some(base) = self.designated(Designated::BaseIterable) else {
            return self.db.unknown();
        };
        self.conclude(at, None, |rule| {
            let element = rule.solver.infer();
            let target = rule.term(|holes| {
                let element = holes.hole(element, Kind::Type);
                holes.apply(base, vec![element])
            });
            rule.constrain(
                rule.closed(iterable),
                target,
                Check::Fits(span, Misfit::Iterable),
            );
            vec![element]
        })[0]
    }

    /// The types a pattern's items unpack from a value, which must be an
    /// `Unpack[S]`. Since unpacking checks the items' count as it runs, every item
    /// is optional and the pattern admits any others; a rest's type isn't found yet.
    /// Diagnosed at `span`, if given.
    pub(super) fn unpack(
        &mut self,
        at: At,
        items: &[PatternItem],
        value: TypeId,
        span: Option<Span>,
    ) -> Vec<TypeId> {
        let unknown = self.db.unknown();
        if value == self.db.bottom() {
            return vec![value; items.len()];
        }
        let Some(unpack) = self.designated(Designated::Unpack) else {
            return vec![unknown; items.len()];
        };
        let check = match span {
            Some(span) => Check::Fits(span, Misfit::Unpackable),
            None => Check::Quiet,
        };
        self.conclude(at, None, |rule| {
            let vars: Vec<Option<Term>> = (items.iter())
                .map(|item| match item.key {
                    PatternKey::Pos | PatternKey::Key(_) => Some(rule.solver.infer()),
                    PatternKey::ConstKey(_) | PatternKey::Rest(_) => None,
                })
                .collect();
            let target = rule.term(|holes| {
                let mut schema: Vec<SchemaItem> = (items.iter().zip(&vars))
                    .filter_map(|(item, var)| {
                        let hole = holes.hole((*var)?, Kind::Type);
                        let element = match item.key {
                            PatternKey::Key(key) => Element::Keyed {
                                key: holes.db.intern(Type::Literal(Literal::Sym(key))),
                                value: hole,
                            },
                            _ => Element::Positional(hole),
                        };
                        Some(self::item(Multiplicity::Optional, element))
                    })
                    .collect();
                let unknown = holes.db.unknown();
                schema.push(self::item(
                    Multiplicity::Repeated,
                    Element::Positional(unknown),
                ));
                schema.push(self::item(
                    Multiplicity::Repeated,
                    Element::Keyed {
                        key: unknown,
                        value: unknown,
                    },
                ));
                let schema = holes.schema(schema);
                holes.apply(unpack, vec![schema])
            });
            rule.constrain(rule.closed(value), target, check.clone());
            (vars.into_iter())
                .map(|var| var.unwrap_or(rule.closed(unknown)))
                .collect()
        })
    }
}

/// Constrain a spread value to spread as `schema`
fn spread_into(
    rule: &mut Rule<'_, '_>,
    spread: Option<DeclId>,
    ty: TypeId,
    schema: Term,
    span: Span,
) {
    let Some(spread) = spread else {
        let unknown = rule.db.unknown_of(Kind::Schema);
        rule.constrain(rule.closed(unknown), schema, Check::Quiet);
        return;
    };
    let target = rule.term(|holes| {
        let schema = holes.hole(schema, Kind::Schema);
        holes.apply(spread, vec![schema])
    });
    rule.constrain(
        rule.closed(ty),
        target,
        Check::Fits(span, Misfit::Spreadable),
    );
}

/// The obligation of a contradicted constraint: the root of its diagnostics' paths
fn root(outcome: &Outcome) -> ObligationId {
    outcome
        .diagnostics
        .iter()
        .find_map(|diagnostic| diagnostic.path.first().copied())
        .expect("a contradicted constraint has a diagnostic")
}

/// What a rule passes the `do` blocks it's given: each variable's solution, once
/// everything that defaulting can solve is. It runs after the rule is concluded,
/// so its defaults decide nothing. A block's result that isn't known yet is taken
/// to be bottom here, so that a callee's binder it also bounds, as `T` in
/// `fold[T] init@T f@((T, T) -> T)`, is solved from the other bounds.
fn passed(
    solver: &mut Solver<'_>,
    db: &Database,
    passed: &[(VarId, Term)],
    pending: &[Term],
) -> Vec<(VarId, TypeId)> {
    if passed.is_empty() {
        return Vec::new();
    }
    for &term in pending {
        solver.constrain(solver.closed(db.bottom()), term, Provenance::default());
    }
    default_all(solver, db, false);
    (passed.iter())
        .filter_map(|&(var, term)| solver.reify(term).ok().map(|ty| (var, ty)))
        .collect()
}

/// Solve, defaulting every unsolved variable whose lower bounds are solved, and,
/// if `bare`, then any variable without lower bounds to the dynamic type of its
/// kind, until nothing more can be defaulted
fn default_all(solver: &mut Solver<'_>, db: &Database, bare: bool) -> Vec<Outcome> {
    let mut outcomes = solver.solve();
    // Each round solves at least one variable; the limit only guards the solver
    for _ in 0..64 {
        let unsolved: Vec<_> = solver.unresolved().collect();
        if unsolved.is_empty() {
            break;
        }
        let mut progress = false;
        for &id in &unsolved {
            progress |= solver.default(id).is_ok();
        }
        if !progress {
            if !bare {
                break;
            }
            let bare: Vec<_> = (unsolved.into_iter())
                .filter(|&id| solver.bounds(id).lower().next().is_none())
                .collect();
            if bare.is_empty() {
                break;
            }
            for id in bare {
                let unknown = db.unknown_of(solver.variable_kind(id));
                solver.constrain(
                    solver.closed(unknown),
                    Term::Infer(id),
                    Provenance::default(),
                );
            }
        }
        outcomes = solver.solve();
    }
    outcomes
}
