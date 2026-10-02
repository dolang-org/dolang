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

use std::collections::{HashSet, VecDeque};

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
            CallArgument, Contradiction, InferVarId, Issue, ObligationId, Outcome, PatternShape,
            Provenance, Solver, Status, Step as Derivation, Term,
        },
        r#type::{
            Argument, BoundRef, Database, DeclId, Element, Function, Intrinsic, Kind, Literal,
            Multiplicity, Rest, SchemaItem, SymbolId, Type, TypeId, UnionMember, Variance,
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
    /// The variables standing for the arguments held back, by index into
    /// [`Values::held`]
    held: Vec<(usize, Term)>,
    /// Whether a `do` block's result is left to a variable even once it's known,
    /// so that choosing an overload doesn't depend on it
    blind: bool,
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
        let (ty, holes) = match (self.blind, &lambda.blinded) {
            (true, Some((ty, holes))) => (*ty, holes),
            _ => (lambda.ty, &lambda.holes),
        };
        let group = (holes.iter())
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
        self.solver.view(ty, environment)
    }

    /// A fresh variable standing for an argument held back
    fn held(&mut self, index: usize) -> Term {
        let term = self.solver.infer();
        self.held.push((index, term));
        term
    }

    /// A call's arguments, or a collection's items, as often as each is passed,
    /// with the span each is diagnosed at: its own, its group's, or `fallback`
    fn arguments(
        &mut self,
        values: &Values<'_>,
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
                    Value::Pair(key, ty) => {
                        joined.keyed(rule, rule.db.regular(key), Part::Closed(ty))
                    }
                    Value::Lambda(key, ref lambda, _) => {
                        let term = Part::Term(rule.lambda(lambda));
                        match key {
                            Some(key) => joined.keyed(rule, symbol(key), term),
                            None => joined.positional.add(rule, term),
                        }
                    }
                    Value::Held(key, index, _) => {
                        let term = Part::Term(rule.held(index));
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
                // A key passed is exact, as a keyword's is
                Value::Pair(key, ty) => {
                    let key = rule.closed(rule.db.regular(key));
                    let argument = CallArgument::Pair(key, rule.closed(ty));
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
                Value::Held(key, index, span) => {
                    let term = rule.held(index);
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
    /// An argument held back until the call's parameters give it an expectation,
    /// positional or keyed, by its index into [`Values::held`]
    Held(Option<SymbolId>, usize, Span),
}

/// An argument held back, with the operands its holes pop
struct Held<'e> {
    expr: &'e Expr,
    operands: VecDeque<TypeId>,
}

/// A `do` block passed to a rule, which types it: its function type, with a hole
/// at depth 0 for each item its signature leaves to the rule
pub(super) struct Lambda {
    func: FuncId,
    ty: TypeId,
    holes: Vec<Hole>,
    /// Its type and holes with a hole for its result, once that's known
    blinded: Option<(TypeId, Vec<Hole>)>,
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
struct Values<'e> {
    values: Vec<Placed>,
    /// Each outermost `for`'s span
    groups: Vec<Span>,
    /// Whether a value isn't produced, so neither is the whole
    never: bool,
    /// Whether it has a comprehension
    comprehension: bool,
    /// The arguments held back, in order
    held: Vec<Held<'e>>,
}

impl Values<'_> {
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
    /// Whether an item whose parameter isn't known yet is held back, to be
    /// expected to be what the call's pre-solve gives it
    hold: bool,
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

/// Hold an item back, if it takes an expectation that its position doesn't give
/// yet: a collection literal or a call. Its operands are set aside with it. One in
/// an `if` outside every `for` isn't, since the `if`'s branches are compared as
/// they're gathered. Returns its index into [`Values::held`].
fn hold<'e>(
    gathering: &Gathering<'_>,
    place: Place,
    expected: Option<TypeId>,
    value: &'e Expr,
    operands: &mut VecDeque<TypeId>,
    values: &mut Values<'e>,
) -> Option<usize> {
    let takes = matches!(
        value.kind,
        ExprKind::Collection { .. } | ExprKind::Call { .. }
    );
    if !gathering.hold
        || expected.is_some()
        || !takes
        || (place.comprehension && place.group.is_none())
    {
        return None;
    }
    let operands = operands.drain(..super::holes(value)).collect();
    values.held.push(Held {
        expr: value,
        operands,
    });
    Some(values.held.len() - 1)
}

/// A call's own arguments, what its result is expected to be, and where it is
#[derive(Clone, Copy)]
pub(super) struct Call<'e> {
    pub(super) args: &'e [Item],
    pub(super) expected: Option<TypeId>,
    pub(super) span: Span,
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
                held: Vec::new(),
                blind: false,
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
                let roots = outputs(&results, &[]);
                default_all(&mut solver, self.db, true, Some(&roots))
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
        for (var, ty) in self::passed(&mut solver, self.db, &results, &passed, &pending) {
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
                            inner: None,
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
        match contradiction {
            Contradiction::Conflict => return Problem::Conflict(span),
            Contradiction::Unadmitted(key) => {
                let key = self.tables.render_type(self.db, key);
                return Problem::Unadmitted { span, key };
            }
            _ => {}
        }
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
                // A parameter whose type is left unsolved shows the bound of it
                // that the argument doesn't fit
                let expected = self
                    .render_term(solver, relation.expected)
                    .or_else(|| match steps.get(params + 2) {
                        Some(Derivation::BoundPropagation) => {
                            let bound = solver.obligation(path[params + 3]).relation;
                            self.render_term(solver, bound.expected)
                        }
                        _ => None,
                    });
                // What fails may lie deeper than the argument's own relation, as
                // in a binder's bound its type solves. A literal's class only
                // restates the literal.
                let mut end = path.len();
                while end > params + 3 && matches!(steps[end - 2], Derivation::IntrinsicBacking(_))
                {
                    end -= 1;
                }
                let deepest = solver.obligation(path[end - 1]).relation;
                let inner = (end > params + 3)
                    .then(|| {
                        let part = self.render_term(solver, deepest.actual)?;
                        let bound = self.render_term(solver, deepest.expected)?;
                        Some((part, bound))
                    })
                    .flatten()
                    .filter(|(part, bound)| {
                        (Some(part), Some(bound)) != (Some(&found), expected.as_ref())
                    });
                Problem::Argument {
                    span: arg,
                    found,
                    expected,
                    inner,
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
    /// analysis gives it, is a variable that keeps the call undecided. An argument
    /// that takes an expectation its generic callee's signature alone doesn't give
    /// is held back until the call's pre-solve does (see [`Flow::expectations`]).
    /// A comprehension's arguments are passed as often as it says: see
    /// [`Flow::gather`]. A class object is called as its constructor (see
    /// [`Flow::construct`]). A callee that isn't a function or a union of them
    /// gives the dynamic type.
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
        let callee_type = self.eval(at, state, operands, callee);
        let call = Call {
            args,
            expected,
            span: expr.span,
        };
        if let Some(class) = self.class_of(callee_type) {
            return self.construct(at, state, operands, callee_type, class, call);
        }
        if let Some((overloads, implementation)) = self.overloaded(callee_type) {
            return self.call_overloaded(
                at,
                state,
                operands,
                &overloads,
                Some(implementation),
                &[],
                call,
            );
        }
        self.call_with(at, state, operands, callee_type, &[], call)
    }

    /// A call of `callee`, passing `receivers` before the call's own arguments, as
    /// a method call passes its receiver
    pub(super) fn call_with(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        callee_type: TypeId,
        receivers: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> TypeId {
        let mut params = self.params(callee_type).unwrap_or_default();
        let skipped = receivers.len().min(params.positional.len());
        params.positional.drain(..skipped);
        let generic = matches!(self.db.ty(callee_type), Type::Quantified { .. });
        let mut values = self.values(at, state, operands, call.args, Some(&params), true, generic);
        received(&mut values, receivers, self.db.bottom());
        self.finish_call(at, state, callee_type, values, call)
    }

    /// A call of an overloaded function, passing `receivers` as
    /// [`Self::call_with`] does. The call takes the one overload whose pre-solve
    /// isn't contradicted, if exactly one is, and otherwise `implementation`, or is
    /// dynamic without one. Every argument that takes an expectation is held back
    /// until it's chosen. The expected result doesn't choose, so a call that
    /// doesn't give what's expected is reported against its overload.
    ///
    /// A stopgap for resolving overloads with union calls in the solver: the
    /// pre-solve sees a `do` block's result only as a variable, even once it's
    /// known, so an overload isn't rejected by what a block gives it.
    #[expect(clippy::too_many_arguments, reason = "a call's parts")]
    pub(super) fn call_overloaded(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        overloads: &[TypeId],
        implementation: Option<TypeId>,
        receivers: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> TypeId {
        let mut values = self.values(at, state, operands, call.args, None, true, true);
        received(&mut values, receivers, self.db.bottom());
        let mut chosen = None;
        if !values.never {
            let (input, output) = self.channels(at);
            let mut survivors = (overloads.iter()).filter(|&&overload| {
                let (_, _, contradicted) =
                    self.presolve(overload, &values, input, output, None, call.span, true);
                !contradicted
            });
            if let (Some(&survivor), None) = (survivors.next(), survivors.next()) {
                chosen = Some(survivor);
            }
        }
        let callee = chosen.or(implementation).unwrap_or(self.db.unknown());
        self.finish_call(at, state, callee, values, call)
    }

    /// Finish a call of `callee` with its arguments evaluated: release the ones
    /// held back, with what the call's pre-solve expects of them, and check the
    /// call
    fn finish_call(
        &mut self,
        at: At,
        state: &mut State,
        callee_type: TypeId,
        mut values: Values<'_>,
        call: Call<'_>,
    ) -> TypeId {
        let Call { expected, span, .. } = call;
        let bottom = self.db.bottom();
        let unknown = self.db.unknown();
        let (input, output) = self.channels(at);
        let callable = self.callable(callee_type);
        let spread = self.designated(Designated::Spread);
        if !values.held.is_empty() {
            let expectations = match callee_type != bottom && !values.never && callable {
                true => self.expectations(callee_type, &values, input, output, expected, span),
                false => vec![None; values.held.len()],
            };
            self.release(at, state, &mut values, &expectations);
        }
        if callee_type == bottom || values.never {
            return bottom;
        }
        if !callable {
            self.untyped(at, &values);
            return unknown;
        }
        self.conclude(at, expected, |rule| {
            vec![call_constraint(
                rule,
                &values,
                spread,
                span,
                callee_type,
                input,
                output,
            )]
        })[0]
    }

    /// Whether a callee is a function, or a union of them, as a variable assigned
    /// several closures holds
    fn callable(&self, callee: TypeId) -> bool {
        let function = |ty: TypeId| {
            let ty = match self.db.ty(ty) {
                Type::Quantified { body, .. } => *body,
                _ => ty,
            };
            matches!(self.db.ty(ty), Type::Function(_))
        };
        match self.db.ty(callee) {
            Type::Union(members) => members.iter().all(|member| match *member {
                UnionMember::Type(ty) => function(ty),
                _ => false,
            }),
            _ => function(callee),
        }
    }

    /// The expectations a call's pre-solve gives the arguments held back, by index
    /// into [`Values::held`]. It solves the call with a fresh variable for each held
    /// argument, and never makes a variable dynamic. A variable the held arguments
    /// can't raise takes its least solution (see [`Solver::raised`]): one they
    /// can raise would make the expectation too narrow. A held argument's
    /// expectation is its parameter, if what's forced or chosen solves it.
    fn expectations(
        &self,
        callee: TypeId,
        values: &Values<'_>,
        input: Option<TypeId>,
        output: Option<TypeId>,
        expected: Option<TypeId>,
        span: Span,
    ) -> Vec<Option<TypeId>> {
        let attempt = |seed| self.presolve(callee, values, input, output, seed, span, false);
        let mut expectations = vec![None; values.held.len()];
        let (mut solver, mut held, contradicted) = attempt(expected);
        if contradicted {
            let contradicted;
            (solver, held, contradicted) = attempt(None);
            if contradicted {
                return expectations;
            }
        }
        let terms: Vec<Term> = held.iter().map(|&(_, term)| term).collect();
        let raised = (solver.raised(&terms)).unwrap_or_else(|_| solver.unresolved().collect());
        default_where(&mut solver, |id| !raised.contains(&id), None);
        for (index, term) in held {
            expectations[index] = expectation(&solver, term);
        }
        expectations
    }

    /// Solve a call of `callee` with a fresh variable for each argument held back,
    /// and `expected`, if given, as an upper bound on its result. Returns the
    /// solver, each held argument's variable by its index into [`Values::held`],
    /// and whether anything was contradicted.
    ///
    /// With `choosing`, it decides whether an overload takes the call: a fresh
    /// variable also stands for each `do` block's result, and what's left
    /// unsolved is defaulted, as the call's own solve would, so that an overload
    /// isn't taken only because a key it can't select by is still a variable.
    #[expect(clippy::too_many_arguments, reason = "a call's parts")]
    fn presolve(
        &self,
        callee: TypeId,
        values: &Values<'_>,
        input: Option<TypeId>,
        output: Option<TypeId>,
        expected: Option<TypeId>,
        span: Span,
        choosing: bool,
    ) -> (Solver<'a>, Vec<(usize, Term)>, bool) {
        let spread = self.designated(Designated::Spread);
        let mut solver = self.solver();
        let mut rule = Rule {
            solver: &mut solver,
            db: self.db,
            checks: Vec::new(),
            passed: Vec::new(),
            pending: Vec::new(),
            held: Vec::new(),
            blind: choosing,
        };
        let result = call_constraint(&mut rule, values, spread, span, callee, input, output);
        let held = rule.held;
        if let Some(expected) = expected {
            solver.constrain(result, solver.closed(expected), Provenance::default());
        }
        let contradicted = |outcomes: &[Outcome]| {
            (outcomes.iter()).any(|outcome| outcome.status == Status::Contradicted)
        };
        let mut rejected = contradicted(&solver.solve());
        if choosing && !rejected {
            rejected = contradicted(&default_where(&mut solver, |_| true, None));
        }
        (solver, held, rejected)
    }

    /// Evaluate the arguments held back, each expecting what `expectations` gives
    /// it, in place of their placeholders
    fn release(
        &mut self,
        at: At,
        state: &mut State,
        values: &mut Values<'_>,
        expectations: &[Option<TypeId>],
    ) {
        let bottom = self.db.bottom();
        let types: Vec<TypeId> = (std::mem::take(&mut values.held).into_iter())
            .zip(expectations)
            .map(|(mut held, &expected)| {
                self.expect(at, state, &mut held.operands, held.expr, expected)
            })
            .collect();
        let mut never = false;
        values.values.retain_mut(|placed| {
            let Value::Held(key, index, span) = placed.value else {
                return true;
            };
            let ty = types[index];
            if ty == bottom {
                // One in a comprehension occurs zero times
                never |= placed.group.is_none();
                return false;
            }
            placed.value = match key {
                Some(key) => Value::Key(key, ty, span),
                None => Value::Pos(ty, span),
            };
            true
        });
        values.never |= never;
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
        let params = db.intern(Type::Schema(schema.into()));
        let function = |result| {
            db.intern(Type::Function(Function {
                params,
                result,
                input,
                output,
            }))
        };
        let mut blinded = None;
        let result = match signature.result {
            Some(var) => match self.joined(var, at) {
                ty if ty == db.bottom() => hole(&mut holes, Hole::Result),
                ty => {
                    let mut holes = holes.clone();
                    let result = hole(&mut holes, Hole::Result);
                    blinded = Some((function(result), holes));
                    ty
                }
            },
            None => declared.result,
        };
        let ty = function(result);
        let unknown = db.unknown();
        for (item, &var) in pattern.iter().zip(&signature.params) {
            if let (PatternKey::Rest(_), Some(var)) = (&item.key, var) {
                self.join(var, unknown);
            }
        }
        Some(Lambda {
            func,
            ty,
            holes,
            blinded,
        })
    }

    /// The parameter types a callee's signature alone gives, which don't mention its
    /// binders. Any other parameter's type is only known once the call is solved,
    /// so an argument that takes an expectation there is held back.
    fn params(&self, callee: TypeId) -> Option<Params> {
        let ty = match self.db.ty(callee) {
            // Its binders stay references, so a parameter mentioning one isn't fixed
            Type::Quantified { body, .. } => *body,
            _ => callee,
        };
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
    /// among them is left for the rule to type. With `hold`, an item that takes an
    /// expectation, but whose position `params` doesn't know, is held back (see
    /// [`Flow::expectations`]).
    #[expect(clippy::too_many_arguments, reason = "how items are gathered")]
    fn values<'e>(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &'e [Item],
        params: Option<&Params>,
        contextual: bool,
        hold: bool,
    ) -> Values<'e> {
        let mut values = Values::default();
        let mut gathering = Gathering {
            params,
            contextual,
            hold,
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
    fn gather<'e>(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &'e [Item],
        gathering: &mut Gathering<'_>,
        place: Place,
        values: &mut Values<'e>,
        out: &mut Vec<Placed>,
    ) {
        let bottom = self.db.bottom();
        let produced = |values: &mut Values<'_>, ty: TypeId| {
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
                    if let Some(index) = hold(gathering, place, expected, value, operands, values) {
                        out.push(placed(Value::Held(None, index, value.span)));
                        continue;
                    }
                    let ty = self.expect(at, state, operands, value, expected);
                    if produced(values, ty) {
                        out.push(placed(Value::Pos(ty, value.span)));
                    }
                }
                &Item::Key(key, ref value) => {
                    let expected = gathering.params.and_then(|params| {
                        (params.keyed.iter()).find_map(|&(name, ty)| (name == key).then_some(ty))
                    });
                    if let Some(index) = hold(gathering, place, expected, value, operands, values) {
                        out.push(placed(Value::Held(Some(key), index, value.span)));
                        continue;
                    }
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
    fn untyped(&mut self, at: At, values: &Values<'_>) {
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
            Collection::Tuple => Designated::Intrinsic(Intrinsic::Tuple),
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
            false,
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
        let int = self.intrinsic(Intrinsic::Int);
        // A dict expected to be a `BaseDict[S]` is a `Dict[S]`, which only a fresh
        // dict can be
        let expected_dict = match kind {
            Collection::Dict => expected.and_then(|ty| {
                self.applied(class, ty).or_else(|| {
                    let base = self.designated(Designated::BaseDict)?;
                    let (_, schema) = self.applied(base, ty)?;
                    let dict = self.db.intern(Type::Apply {
                        base: self.db.intern(Type::Decl(class)),
                        args: vec![Argument::Positional(schema)].into(),
                        kind: Kind::Type,
                    });
                    Some((dict, schema))
                })
            }),
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
                        Value::Held(..) => unreachable!("only a call holds arguments back"),
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
                // A key written in a dict literal is a term, which decays as its value would
                let symbol = |name| rule.db.intern(Type::Fresh(Literal::Sym(name)));
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
                        Value::Held(..) => unreachable!("only a call holds arguments back"),
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
                            Value::Held(..) => unreachable!("only a call holds arguments back"),
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
            _ => None,
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

    /// A value stored where its type must be `ty`, as a field is
    pub(super) fn store(&mut self, at: At, value: TypeId, ty: TypeId, span: Span) {
        if value == self.db.bottom() {
            return;
        }
        self.conclude(at, None, |rule| {
            rule.constrain(rule.closed(value), rule.closed(ty), Check::Expected(span));
            Vec::new()
        });
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
        let int = self.db.intrinsic(Intrinsic::Int);
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
    /// `Unpack[S]`, diagnosed at `span`; `None` if the pattern can't match it.
    /// The pattern is walked against `S` (see [`Solver::unpack_pattern`]), giving
    /// a rest `Unpack[tail]`. Where `S` can't be found, every item is optional and
    /// the pattern admits any others, giving a const key or a rest `Unknown`.
    pub(super) fn unpack(
        &mut self,
        at: At,
        state: &mut State,
        items: &[PatternItem],
        value: TypeId,
        span: Span,
    ) -> Option<Vec<TypeId>> {
        let unknown = self.db.unknown();
        if value == self.db.bottom() {
            return Some(vec![value; items.len()]);
        }
        let Some(unpack) = self.designated(Designated::Unpack) else {
            return Some(vec![unknown; items.len()]);
        };
        let check = Check::Fits(span, Misfit::Unpackable);
        let mut pattern = PatternShape {
            positional: Vec::new(),
            keyed: Vec::new(),
            rests: Vec::new(),
        };
        for item in items {
            match item.key {
                PatternKey::Pos => pattern.positional.push(item.default),
                PatternKey::Key(key) => {
                    let key = self.db.intern(Type::Literal(Literal::Sym(key)));
                    pattern.keyed.push((key, item.default));
                }
                PatternKey::ConstKey(ref key) => {
                    // A constant, without operands
                    let key = self.eval(at, state, &mut VecDeque::new(), key);
                    pattern.keyed.push((self.db.regular(key), item.default));
                }
                PatternKey::Rest(kind) => pattern.rests.push(kind),
            }
        }
        if let Some(unpacked) = self.solver().unpack_pattern(value, unpack, &pattern) {
            self.conclude(at, None, |rule| {
                let target = rule.term(|holes| {
                    let unknown = holes.db.unknown();
                    let schema = holes.schema(vec![
                        self::item(Multiplicity::Repeated, Element::Positional(unknown)),
                        self::item(
                            Multiplicity::Repeated,
                            Element::Keyed {
                                key: unknown,
                                value: unknown,
                            },
                        ),
                    ]);
                    holes.apply(unpack, vec![schema, unknown])
                });
                rule.constrain(rule.closed(value), target, check.clone());
                Vec::new()
            });
            if !unpacked.possible {
                return None;
            }
            let (positional, keyed) = unpacked.slots.split_at(pattern.positional.len());
            let (mut positional, mut keyed) = (positional.iter(), keyed.iter());
            let mut rests = unpacked.rests.iter();
            let types = (items.iter())
                .map(|item| {
                    let slot = match item.key {
                        PatternKey::Pos => positional.next(),
                        PatternKey::Key(_) | PatternKey::ConstKey(_) => keyed.next(),
                        PatternKey::Rest(_) => rests.next(),
                    };
                    *slot.expect("a type for each item")
                })
                .collect();
            return Some(types);
        }
        let types = self.conclude(at, None, |rule| {
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
                holes.apply(unpack, vec![schema, unknown])
            });
            rule.constrain(rule.closed(value), target, check.clone());
            (vars.into_iter())
                .map(|var| var.unwrap_or(rule.closed(unknown)))
                .collect()
        });
        Some(types)
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
    results: &[Term],
    passed: &[(VarId, Term)],
    pending: &[Term],
) -> Vec<(VarId, TypeId)> {
    if passed.is_empty() {
        return Vec::new();
    }
    for &term in pending {
        solver.constrain(solver.closed(db.bottom()), term, Provenance::default());
    }
    let parameters: Vec<Term> = passed.iter().map(|&(_, term)| term).collect();
    let roots = outputs(results, &parameters);
    default_all(solver, db, false, Some(&roots));
    (passed.iter())
        .filter_map(|&(var, term)| solver.reify(term).ok().map(|ty| (var, ty)))
        .collect()
}

/// What a rule produces, for [`Solver::locked`]: its results, and the
/// parameters of the `do` blocks it passes values, which are inputs of their
/// function types
fn outputs(results: &[Term], parameters: &[Term]) -> Vec<(Term, Variance)> {
    let results = results.iter().map(|&term| (term, Variance::Covariant));
    let parameters = parameters
        .iter()
        .map(|&term| (term, Variance::Contravariant));
    results.chain(parameters).collect()
}

/// Solve, defaulting every unsolved variable whose lower bounds are solved, and,
/// if `bare`, then any variable without lower bounds to its binder's default, or
/// without one, to the dynamic type of its kind, until nothing more can be
/// defaulted. Literals decay as
/// [`default_where`] decays them.
fn default_all(
    solver: &mut Solver<'_>,
    db: &Database,
    bare: bool,
    roots: Option<&[(Term, Variance)]>,
) -> Vec<Outcome> {
    let mut outcomes = default_where(solver, |_| true, roots);
    if !bare {
        return outcomes;
    }
    // Each round solves at least one variable; the limit only guards the solver
    for _ in 0..64 {
        let bare: Vec<_> = (solver.unresolved())
            .filter(|&id| solver.bounds(id).lower().next().is_none())
            .collect();
        if bare.is_empty() {
            break;
        }
        for id in bare {
            let default = (solver.fallback(id))
                .unwrap_or_else(|| solver.closed(db.unknown_of(solver.variable_kind(id))));
            solver.constrain(default, Term::Infer(id), Provenance::default());
        }
        outcomes = default_where(solver, |_| true, roots);
    }
    outcomes
}

/// Solve, defaulting every unsolved variable that `keep` admits whose lower
/// bounds are solved, until nothing more can be defaulted. A default decays its
/// literals only if a literal would lock in: if the variable is among what
/// [`Solver::locked`] finds from `roots`, the rule's outputs, or always without
/// them.
fn default_where(
    solver: &mut Solver<'_>,
    keep: impl Fn(InferVarId) -> bool,
    roots: Option<&[(Term, Variance)]>,
) -> Vec<Outcome> {
    let mut outcomes = solver.solve();
    // Each round solves at least one variable; the limit only guards the solver
    for _ in 0..64 {
        let unsolved: Vec<_> = solver.unresolved().filter(|&id| keep(id)).collect();
        // Defaults add bounds, so what's locked is found again each round
        let locked = roots.map(|roots| solver.locked(roots));
        let mut progress = false;
        for id in unsolved {
            let decay = match &locked {
                Some(Ok(locked)) => locked.contains(&id),
                Some(Err(_)) | None => true,
            };
            progress |= solver.default_with(id, decay).is_ok();
        }
        if !progress {
            break;
        }
        outcomes = solver.solve();
    }
    outcomes
}

/// Pass `receivers` before a call's own arguments. A receiver that never has a
/// value means the call never happens.
fn received(values: &mut Values<'_>, receivers: &[(TypeId, Span)], bottom: TypeId) {
    values.never |= receivers.iter().any(|&(ty, _)| ty == bottom);
    values.values.splice(
        0..0,
        receivers.iter().map(|&(ty, span)| Placed {
            value: Value::Pos(ty, span),
            multiplicity: Multiplicity::Required,
            group: None,
        }),
    );
}

/// The constraint of a call: its callee below the function type its arguments
/// call it as. Returns the variable standing for its result.
fn call_constraint(
    rule: &mut Rule<'_, '_>,
    values: &Values<'_>,
    spread: Option<DeclId>,
    span: Span,
    callee: TypeId,
    input: Option<TypeId>,
    output: Option<TypeId>,
) -> Term {
    let (arguments, spans) = rule.arguments(values, spread, span);
    let result = rule.solver.infer();
    let call = rule.solver.call_items(
        &arguments,
        result,
        input.map(|ty| rule.closed(ty)),
        output.map(|ty| rule.closed(ty)),
    );
    rule.constrain(rule.closed(callee), call, Check::Call { span, args: spans });
    result
}

/// What a held argument is expected to be: its parameter, if it's solved. The
/// variable of a `for`'s joined items stands between them.
fn expectation(solver: &Solver<'_>, held: Term) -> Option<TypeId> {
    let mut found = None;
    let mut pending = vec![held];
    let mut seen = HashSet::new();
    while let Some(term) = pending.pop() {
        let Term::Infer(id) = term else {
            unreachable!("a variable stands for a held argument")
        };
        if !seen.insert(id) {
            continue;
        }
        for upper in solver.bounds(id).upper() {
            let ty = match (upper, solver.reify(upper)) {
                (_, Ok(ty)) => ty,
                (Term::Infer(_), Err(_)) => {
                    pending.push(upper);
                    continue;
                }
                (_, Err(_)) => return None,
            };
            match found {
                Some(other) if other != ty => return None,
                _ => found = Some(ty),
            }
        }
    }
    found
}
