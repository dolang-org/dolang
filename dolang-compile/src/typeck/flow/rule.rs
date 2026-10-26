//! Checking rules. Each run of a rule solves its constraints with a fresh solver,
//! and only reified types leave it.
//!
//! A rule contributes its results only once it's decided: solved without
//! contradiction, with every result solved without defaulting. Until then it
//! contributes bottom, which adds nothing and never has to be retracted. When the
//! queue empties, the earliest block of each function whose latest run left a rule
//! undecided runs again, once, defaulting the unsolved variables of each rule it
//! leaves undecided, upstream first; a variable with no lower bounds becomes
//! dynamic, and a `do` block's result that isn't known yet is bottom. The final
//! pass defaults every rule it leaves undecided. A rule's results are its latest
//! run's: the analysis converges because the joins at block entries and
//! accumulators widen.
//!
//! A value of bottom type is never produced, so a rule with such an input doesn't
//! run: its results are bottom. An item of a comprehension is the exception, since
//! bottom only says that it occurs zero times.
//!
//! An argument held back, as a collection literal that a generic callee's
//! parameter would expect something of, is evaluated as far as it can be when
//! it's met, and finished as a sub-rule of its call's rule: made in a fork of
//! the rule's solver once what the rule makes is solved, toward what that
//! expects of it with the call's binders still open, and taken over by the rule
//! (see [`Flow::release`]). Its checks are the rule's, and only the rule
//! defaults, but in a run that defaults, each sub-rule first defaults its own
//! variables, as a rule of its own would.
//!
//! A call that gives bottom ends the path of the step or terminal making it (see
//! [`Flow::diverge`]), unless defaulting chose it (see [`Solver::chosen`]): that
//! bottom only picks the least type the call's result can have. It's decided
//! never to return only if its rule is decided and doesn't take a `do` block's
//! result that isn't known yet to be bottom. A union's alternatives end it only
//! if they all do.

use std::{collections::VecDeque, ops::ControlFlow};

use dolang_util::mono::MonoVec;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    At, Detail, Flow, Role, Seen, State,
    member::CallTarget,
    problem::{Misfit, Problem},
    state::Group,
};
use crate::typeck::report::Report;
use crate::{
    source::Span,
    typeck::{
        cfg::{
            Collection, Expr, ExprKind, FuncId, Item, Member, Pattern, PatternItem, PatternKey,
            VarId,
        },
        elab::Designated,
        solver::{
            Call as SolverCall, CallArgument, Contradiction, Gas, InferVarId, Issue, Node,
            ObligationId, Outcome, PatternShape, Provenance, Reason, Relation, Residual, Solver,
            Status, Step as Derivation, Term, Unpacked,
        },
        r#type::{
            Argument, BoundRef, Database, DeclId, Element, Function, Intrinsic, Kind, Literal,
            Multiplicity, Rest, SchemaItem, Shown, Style, SymbolId, Type, TypeId, Variance,
        },
    },
};

/// What a constraint of a rule checks, to diagnose it by
#[derive(Clone)]
enum Check {
    /// A callee against the call, by the call's span and each argument's, and
    /// the callee's name, if it has one
    Call {
        span: Span,
        args: Vec<Span>,
        callee: Option<String>,
    },
    /// A value that must be something
    Fits(Span, Misfit),
    /// A value that must be what's expected of it
    Expected(Span),
    /// An empty dict's schema, which must admit no items. Left undecided, the
    /// dict's type is one nothing determines, which its rule reports.
    Empty(Span),
    /// Nothing that can fail
    Quiet,
}

/// A rule's constraints, as they are made
struct Rule<'s, 'a, 'p> {
    solver: &'s mut Solver<'a>,
    db: &'a Database,
    /// The budget of the run it's made for, which its sub-rules share
    gas: &'a Gas,
    checks: Vec<Check>,
    /// The variables standing for what a call passes the `do` blocks it's given,
    /// by the signature variable each joins into
    passed: Vec<(VarId, Term)>,
    /// The variables standing for the results of `do` blocks not yet known
    pending: Vec<Term>,
    /// The arguments held back, to be released once what's made so far is
    /// solved (see [`Flow::release`])
    held: Vec<Held<'p>>,
    /// The calls held back, by the variable standing for each one's result and
    /// its span, and whether it may occur zero times: a result that's bottom
    /// ends the path, and unless it may not occur, means the whole isn't
    /// produced
    calls: Vec<(Term, Span, bool)>,
    /// Whether it's made for a final pass that logs facts
    logging: bool,
    /// What the final pass logs once it's solved: the `do` blocks and
    /// arguments held back that it types, and the calls written in source that
    /// it checks
    logged: Vec<Pending>,
    /// Whether what it makes is held back in a `for`, so it may occur zero
    /// times
    repeated: bool,
    /// Whether it's made for a run that defaults, so each sub-rule defaults
    /// its own variables, as a rule of its own would
    defaulting: bool,
    /// Whether its sub-rules are seeded: bounded by what's expected of them
    /// (see [`Flow::sub_rule`])
    seed: bool,
    /// Whether a sub-rule was seeded
    seeded: bool,
    /// The arguments held back that an earlier make of the run found exactly,
    /// passed closed (see [`Flow::decide`])
    exact: &'s [Found],
    /// The arguments held back that it finds exactly: solved, or with only
    /// solved types below them, without a check of their own left undecided
    /// or a `do` block
    found: Vec<Found>,
    /// Whether a `do` block's result is left to a variable even once it's known,
    /// so that choosing a fresh dict's schema doesn't depend on it
    blind: bool,
}

/// An argument held back that a make of a rule found exactly (see
/// [`Flow::decide`])
struct Found {
    prepared: *const Prepared,
    ty: TypeId,
    /// What it logs of itself and of the arguments held back in it, as they
    /// were found, which passing it closed leaves untyped
    logged: Vec<(Span, Role, Seen)>,
}

/// What a rule's final run logs once it's solved
#[derive(Clone, Copy)]
enum Pending {
    /// A `do` block's or an argument held back's value, by the variable
    /// standing for it
    Value(Term, Span),
    /// A call's callee, by its constraint's obligation
    Callee(ObligationId, Span),
    /// What an argument found exactly logs
    Seen(Span, Role, Seen),
}

impl Pending {
    /// What it logs, as `solver` solved it, with `reify` giving a term's type,
    /// or `None` while that's not known
    fn resolve(
        self,
        solver: &Solver<'_>,
        reify: impl Fn(Term) -> Option<TypeId>,
    ) -> Option<(Span, Role, Seen)> {
        match self {
            Pending::Value(term, span) => Some((span, Role::Value, Seen::ty(reify(term)?))),
            Pending::Callee(obligation, span) => {
                let (callee, overload) = solver.called(obligation)?;
                let mut seen = Seen::ty(reify(callee)?);
                if let Some(index) = overload {
                    let index = u32::try_from(index).expect("an overload's index fits");
                    seen.detail = Detail::Overload(index);
                }
                Some((span, Role::Callee, seen))
            }
            Pending::Seen(span, role, seen) => Some((span, role, seen)),
        }
    }
}

/// An argument held back, with the variable standing for it
#[derive(Clone, Copy)]
struct Held<'p> {
    prepared: &'p Prepared,
    term: Term,
    /// Whether it's in a `for`, so it may occur zero times
    repeated: bool,
}

impl<'s, 'a, 'p> Rule<'s, 'a, 'p> {
    fn new(solver: &'s mut Solver<'a>, db: &'a Database, gas: &'a Gas) -> Self {
        Rule {
            solver,
            db,
            gas,
            checks: Vec::new(),
            passed: Vec::new(),
            pending: Vec::new(),
            held: Vec::new(),
            calls: Vec::new(),
            logging: false,
            logged: Vec::new(),
            repeated: false,
            defaulting: false,
            seed: true,
            seeded: false,
            exact: &[],
            found: Vec::new(),
            blind: false,
        }
    }

    fn constrain(&mut self, actual: Term, expected: Term, check: Check) {
        self.solver
            .constrain(actual, expected, Provenance::default());
        self.checks.push(check);
    }

    /// Constrain `schema` to admit the empty schema, once it's solved
    fn constrain_empty(&mut self, schema: Term, check: Check) {
        self.solver.constrain_empty(schema, Provenance::default());
        self.checks.push(check);
    }

    fn closed(&self, ty: TypeId) -> Term {
        self.solver.closed(ty)
    }

    /// A `do` block's function type, with a fresh variable in each hole, its
    /// value at `span`
    fn lambda(&mut self, lambda: &Lambda, span: Span) -> Term {
        let (ty, holes) = match (self.blind, &lambda.blinded) {
            (true, Some((ty, holes))) => (*ty, holes),
            _ => (lambda.ty, &lambda.holes),
        };
        let group: Vec<Term> = (holes.iter())
            .map(|&hole| {
                let term = self.solver.infer();
                match hole {
                    Hole::Passed(var) => self.passed.push((var, term)),
                    Hole::Result => self.pending.push(term),
                }
                term
            })
            .collect();
        let empty = self.solver.empty_environment();
        // Its known result doesn't choose among alternatives either
        let twin = match (self.blind, &lambda.blinded) {
            (false, Some((blinded, _))) => {
                let mut twin = group.clone();
                twin.push(self.solver.infer());
                let environment = self.solver.environment(empty, twin);
                Some(self.solver.view(*blinded, environment))
            }
            _ => None,
        };
        let environment = self.solver.environment(empty, group);
        let term = self.solver.view(ty, environment);
        if let Some(twin) = twin {
            self.solver.blind(term, twin);
        }
        if self.logging {
            self.logged.push(Pending::Value(term, span));
        }
        term
    }

    /// A fresh variable standing for an argument held back, above what it's
    /// known to be already
    fn hold(&mut self, prepared: &'p Prepared, repeated: bool) -> Term {
        if let Some(found) = self
            .exact
            .iter()
            .find(|found| std::ptr::eq(found.prepared, prepared))
        {
            let term = self.closed(found.ty);
            if self.logging {
                self.logged
                    .extend(prepared.span().map(|span| Pending::Value(term, span)));
                let logged = found.logged.iter();
                let logged = logged.map(|&(span, role, seen)| Pending::Seen(span, role, seen));
                self.logged.extend(logged);
            }
            return term;
        }
        let term = self.solver.infer();
        if self.logging {
            let value = prepared.span().map(|span| Pending::Value(term, span));
            self.logged.extend(value);
        }
        self.held.push(Held {
            prepared,
            term,
            repeated: repeated || self.repeated,
        });
        if let Some(preview) = prepared.preview() {
            self.constrain(self.closed(preview), term, Check::Quiet);
        }
        term
    }

    /// A call's arguments, or a collection's items, as often as each is passed,
    /// with the span each is diagnosed at: its own, its group's, or `fallback`
    fn arguments(
        &mut self,
        values: &'p Values,
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
                    Value::Lambda(key, ref lambda, span) => {
                        let term = Part::Term(rule.lambda(lambda, span));
                        match key {
                            Some(key) => joined.keyed(rule, symbol(key), term),
                            None => joined.positional.add(rule, term),
                        }
                    }
                    Value::Held(at, index, _) => {
                        let term = Part::Term(rule.hold(&values.held[index], true));
                        match at {
                            HeldAt::Positional => joined.positional.add(rule, term),
                            HeldAt::Keyword(key) => joined.keyed(rule, symbol(key), term),
                            HeldAt::Pair(key) => joined.keyed(rule, rule.db.regular(key), term),
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
                    let term = rule.lambda(lambda, span);
                    let argument = match key {
                        Some(key) => CallArgument::Keyword(key, term),
                        None => CallArgument::Positional(term),
                    };
                    vec![(multiplicity, argument, span)]
                }
                Value::Held(at, index, span) => {
                    let term = rule.hold(&values.held[index], false);
                    let argument = match at {
                        HeldAt::Positional => CallArgument::Positional(term),
                        HeldAt::Keyword(key) => CallArgument::Keyword(key, term),
                        HeldAt::Pair(key) => {
                            CallArgument::Pair(rule.closed(rule.db.regular(key)), term)
                        }
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

/// A run of a rule (see [`Flow::decide`])
struct Run<'g> {
    solver: Solver<'g>,
    results: Vec<Term>,
    checks: Vec<Check>,
    passed: Vec<(VarId, Term)>,
    pending: Vec<Term>,
    calls: Vec<(Term, Span, bool)>,
    logged: Vec<Pending>,
    outcomes: Vec<Outcome>,
    /// How many of the checks are the rule's own, ahead of what's expected of
    /// it and its sub-rules'
    own: usize,
    /// Whether what's expected of it is contradicted
    rejected: bool,
    /// Whether a sub-rule was seeded
    seeded: bool,
}

impl Run<'_> {
    /// Whether any of the first `count` checks has `status`
    fn any(&self, count: usize, status: Status) -> bool {
        (self.outcomes[..count].iter()).any(|outcome| outcome.status == status)
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
    /// by its index into [`Values::held`]
    Held(HeldAt, usize, Span),
}

/// Where an argument held back goes
#[derive(Clone, Copy)]
enum HeldAt {
    Positional,
    Keyword(SymbolId),
    /// Under an evaluated key
    Pair(TypeId),
}

/// An argument held back, evaluated as far as it can be before the call's
/// parameters give it an expectation. It's finished as a sub-rule of the
/// call's (see [`Flow::release`]).
enum Prepared {
    /// A call of a function, and the instance it gives, if it's a constructor
    /// with unchecked arguments. A range calls its constructor.
    Call {
        call: PreparedCall,
        instance: Option<TypeId>,
    },
    Collection(PreparedCollection),
    /// A template, with its values and hole shape known
    Fmt(TypeId),
}

impl Prepared {
    /// Its span, if its value is left to the rule it's held back in; a template's
    /// is known when it's evaluated
    fn span(&self) -> Option<Span> {
        match self {
            Prepared::Call { call, .. } => Some(call.span),
            Prepared::Collection(collection) => Some(collection.span),
            Prepared::Fmt(_) => None,
        }
    }

    /// Its type, if it's known before its expectation is
    fn preview(&self) -> Option<TypeId> {
        match *self {
            Prepared::Fmt(ty) => Some(ty),
            Prepared::Call { .. } | Prepared::Collection(_) => None,
        }
    }
}

/// A call of `callee`, its arguments evaluated
pub(super) struct PreparedCall {
    callee: TypeId,
    values: Values,
    member: Option<Member>,
    span: Span,
    /// Whether it's written in source, so the final pass logs its callee
    logs: bool,
    /// The ambient channels it passes
    channels: (Option<TypeId>, Option<TypeId>),
}

impl PreparedCall {
    fn call(&self, expected: Option<TypeId>) -> Call<'static> {
        Call {
            args: &[],
            expected,
            span: self.span,
            callee: self.member,
            logs: self.logs,
        }
    }
}

/// A collection literal, its items evaluated
struct PreparedCollection {
    kind: Collection,
    class: Option<DeclId>,
    values: Values,
    span: Span,
}

/// What's expected of a fresh collection, and what that lets it be
struct Fresh {
    expected: Option<TypeId>,
    /// An array's type and element type (see [`Flow::fresh_expected`])
    array: Option<(TypeId, TypeId)>,
    /// A dict's candidates (see [`Flow::fresh_candidates`])
    candidates: Vec<(TypeId, TypeId)>,
}

/// A schema a fresh dict can take: closed, as an application of its class and
/// the argument, or an alternative of its expectation open in what isn't solved
/// yet
#[derive(Clone, Copy, PartialEq, Eq)]
enum Candidate {
    Closed(TypeId, TypeId),
    Open(Term),
}

/// What a collection's rule builds
#[derive(Clone, Copy)]
enum Shape {
    Array,
    /// A dict whose schema is chosen: its type, and the schema
    Chosen(Term, Term),
    /// A dict with the lanes its items can fill, positional then keyed
    Dict(bool, bool),
    /// An empty dict, whose schema is left to what it's in, so long as it admits
    /// no items
    Empty,
    /// A tuple or record, with the lanes its spreads can fill
    Items(Rest),
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
struct Values {
    values: Vec<Placed>,
    /// Each outermost `for`'s span
    groups: Vec<Span>,
    /// Whether a value isn't produced, so neither is the whole
    never: bool,
    /// Whether it has a comprehension
    comprehension: bool,
    /// The arguments held back, in order
    held: Vec<Prepared>,
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
    /// Whether an item whose parameter isn't known yet is held back, to be
    /// released toward what the call's rule expects of it (see
    /// [`Flow::release`])
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
    fn add(&mut self, rule: &Rule<'_, '_, '_>, part: Part) {
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
    fn term(&self, rule: &mut Rule<'_, '_, '_>) -> Option<Term> {
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
    fn keyed(&mut self, rule: &Rule<'_, '_, '_>, key: TypeId, value: Part) {
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

    fn atom(&mut self, rule: &Rule<'_, '_, '_>, atom: Atom) {
        match atom {
            Atom::Positional(ty) => self.positional.add(rule, Part::Closed(ty)),
            Atom::Keyed(key, value) => self.keyed(rule, key, Part::Closed(value)),
            Atom::Unknown => self.unknown = true,
        }
    }

    /// Its repeated arguments
    fn arguments(&self, rule: &mut Rule<'_, '_, '_>) -> Vec<CallArgument> {
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

/// A `do` block's declared type, with what `fill` gives each parameter and
/// channel its signature leaves to the calls it's passed to, in order. A rest's
/// element type isn't left to them. `None` if the declared type doesn't line up
/// with its parameters.
pub(super) fn passed_signature(
    db: &Database,
    data: &super::super::cfg::Func,
    declared: &Function,
    mut fill: impl FnMut(VarId) -> TypeId,
) -> Option<Function> {
    let signature = data.signature.as_ref()?;
    let (Pattern::Unpack { items: pattern, .. }, Type::Schema(items)) =
        (&data.params, db.ty(declared.params))
    else {
        return None;
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
            (Some(var), Element::Positional(_)) => Element::Positional(fill(var)),
            (Some(var), &Element::Keyed { key, .. }) => Element::Keyed {
                key,
                value: fill(var),
            },
            (Some(_), Element::Include(_)) => unreachable!("not a rest"),
        };
        schema.push(self::item(item.multiplicity, element));
    }
    if singles.next().is_some() {
        return None;
    }
    let mut channel = |var: Option<VarId>, declared: Option<TypeId>| match var {
        Some(var) => Some(fill(var)),
        None => declared,
    };
    let input = channel(signature.input, declared.input);
    let output = channel(signature.output, declared.output);
    Some(Function {
        params: db.intern(Type::Schema(schema.into())),
        input,
        output,
        ..declared.clone()
    })
}

/// Whether to hold an item back: if it takes an expectation that its position
/// doesn't give yet, as a collection literal, range, call or template does. One
/// in an `if` outside every `for` isn't, since the `if`'s branches are compared
/// as they're gathered.
fn hold(gathering: &Gathering<'_>, place: Place, expected: Option<TypeId>, value: &Expr) -> bool {
    let takes = matches!(
        value.kind,
        ExprKind::Collection { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Fmt(_)
    );
    gathering.hold && expected.is_none() && takes && !(place.comprehension && place.group.is_none())
}

/// A call's own arguments, what its result is expected to be, and where it is,
/// with the member it calls, if any, to name it by
#[derive(Clone, Copy)]
pub(super) struct Call<'e> {
    pub(super) args: &'e [Item],
    pub(super) expected: Option<TypeId>,
    pub(super) span: Span,
    pub(super) callee: Option<Member>,
    /// Whether it's written in source, so the final pass logs its callee and
    /// the members it reaches
    pub(super) logs: bool,
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

/// What a run of a rule concludes (see [`Flow::decide`])
struct Decision {
    /// Its results
    values: Vec<TypeId>,
    /// Whether they're decided
    decided: bool,
    /// Whether each is a bottom that defaulting chose
    chosen: Vec<bool>,
}

impl<'a> Flow<'a, '_> {
    /// Run a rule: solve the constraints `build` makes and conclude its results in
    /// this context, replacing what its earlier runs concluded. Once the rule
    /// is made, `expected` bounds its first result above. A rule left
    /// undecided is solved again with defaulting if its block defaults, or in the
    /// final pass; otherwise it marks its block undecided. Defaulting takes a `do`
    /// block's result that isn't known yet to be bottom: the block never completes,
    /// until its result grows and runs the rule again.
    fn conclude<'p>(
        &mut self,
        at: At,
        span: Span,
        expected: Option<TypeId>,
        build: impl Fn(&Self, &mut Rule<'_, '_, 'p>) -> Vec<Term>,
    ) -> Vec<TypeId> {
        self.decide(at, span, expected, build).values
    }

    /// Run a rule as [`Flow::conclude`] does, also saying whether its results are
    /// decided: solved, or defaulted without an unresolved check and without
    /// taking a `do` block's result that isn't known yet to be bottom. A decided
    /// bottom is a value that's never produced, where an undecided one only
    /// stands in for what isn't known yet. It also says which results are a
    /// bottom that defaulting chose (see [`Solver::chosen`]).
    ///
    /// Each run solves what `build` makes before releasing the arguments it
    /// holds back, each as a sub-rule (see [`Flow::release`]). A run that
    /// defaults and is contradicted is made again with each sub-rule solved
    /// alone, and then, if `expected` or the rule's own checks are still
    /// contradicted, without `expected` as a bound. The last run's results are
    /// the ones concluded, and its checks the ones diagnosed, so a value that
    /// doesn't fit what's expected of it is reported where it's checked
    /// against that, rather than taking it as its type. A
    /// call held back whose result is bottom ends the path, and means the whole
    /// isn't produced unless it's in a `for`.
    fn decide<'p>(
        &mut self,
        at: At,
        span: Span,
        expected: Option<TypeId>,
        build: impl Fn(&Self, &mut Rule<'_, '_, 'p>) -> Vec<Term>,
    ) -> Decision {
        // Each run has a budget of its own
        let gases = MonoVec::new();
        // An argument held back that's found exactly is then passed as one that
        // isn't, closed, so it chooses only what that would: the rule is made
        // again with it
        let make = |default: bool, (expect, seed): (bool, bool)| {
            let mut exact = Vec::new();
            loop {
                gases.push(Gas::default());
                let gas = &gases[gases.len() - 1];
                let mut solver = self.solver(gas);
                let mut rule = Rule {
                    defaulting: default,
                    logging: self.logging && self.observing(),
                    seed,
                    exact: &exact,
                    ..Rule::new(&mut solver, self.db, gas)
                };
                let results = build(self, &mut rule);
                let own = rule.checks.len();
                let expect = expected.filter(|_| expect).map(|expected| {
                    let expected = rule.closed(expected);
                    rule.constrain(results[0], expected, Check::Quiet);
                    own
                });
                if !rule.held.is_empty() {
                    rule.solver.solve();
                    self.release(&mut rule);
                }
                let Rule {
                    checks,
                    passed,
                    pending,
                    calls,
                    logged,
                    found,
                    seeded,
                    ..
                } = rule;
                if exact.is_empty() && !found.is_empty() {
                    exact = found;
                    continue;
                }
                let outcomes = if default {
                    // The blocks it passes values to have run with them, so a
                    // result that isn't known yet is one that never completes
                    for &term in &pending {
                        let bottom = solver.closed(self.db.bottom());
                        solver.constrain(bottom, term, Provenance::default());
                    }
                    let roots = outputs(&results, &[]);
                    default_all(&mut solver, self.db, true, Some(&roots))
                } else {
                    solver.solve()
                };
                let rejected =
                    expect.is_some_and(|index| outcomes[index].status == Status::Contradicted);
                break Run {
                    solver,
                    results,
                    checks,
                    passed,
                    pending,
                    calls,
                    logged,
                    outcomes,
                    own,
                    rejected,
                    seeded,
                };
            }
        };
        // A contradicted run is made again with its sub-rules unseeded, and
        // then, if what's expected of it or its own checks are still
        // contradicted, without what's expected of it either
        let unseed = |run: &Run<'_>, seeds: &mut (bool, bool)| {
            if !run.any(run.checks.len(), Status::Contradicted) {
                return false;
            }
            match *seeds {
                (_, true) if run.seeded => seeds.1 = false,
                (true, _) if run.rejected || run.any(run.own, Status::Contradicted) => {
                    *seeds = (false, false)
                }
                _ => return false,
            }
            true
        };
        let mut seeds = (true, true);
        let mut run = make(false, seeds);
        let contradicted = run.any(run.checks.len(), Status::Contradicted);
        // A check left unresolved may be decided by defaulting
        let unresolved = run.any(run.checks.len(), Status::Unresolved);
        let reified: Option<Vec<TypeId>> = (run.results.iter())
            .map(|&term| run.solver.reify(term).ok())
            .collect();
        let (mut values, decided): (Vec<TypeId>, bool) = match reified {
            Some(values) if !contradicted && !unresolved => (values, true),
            _ if self.defaulting || self.observing() => {
                unseed(&run, &mut seeds);
                run = make(true, seeds);
                while unseed(&run, &mut seeds) {
                    run = make(true, seeds);
                }
                let reified: Vec<_> = (run.results.iter())
                    .map(|&term| run.solver.reify(term).ok())
                    .collect();
                // A strict unit's defaulting leaves what nothing determines
                // unresolved. A check it leaves unresolved is recorded instead.
                let decided = !run.any(run.checks.len(), Status::Unresolved);
                let (mut empty, mut others) = (false, false);
                for (check, outcome) in run.checks.iter().zip(&run.outcomes) {
                    match (check, outcome.status) {
                        (Check::Empty(_), Status::Unresolved) => empty = true,
                        (_, Status::Unresolved) => others = true,
                        _ => {}
                    }
                }
                let mut open = (run.results.iter().zip(&reified)).filter(|(_, ty)| ty.is_none());
                empty |= open.clone().any(|(&term, _)| run.solver.holds_fresh(term));
                if !others {
                    let what = match (empty, open.next().is_some()) {
                        (true, _) => Some("an empty collection nothing determines"),
                        (false, true) => Some("a type nothing determines"),
                        (false, false) => None,
                    };
                    if let Some(what) = what {
                        self.gap(span, what);
                    }
                }
                let values = (reified.into_iter())
                    .map(|ty| ty.unwrap_or(self.db.unknown()))
                    .collect();
                (values, decided && run.pending.is_empty())
            }
            _ => {
                self.unsettled.insert((at.block, at.ctx));
                (vec![self.db.bottom(); run.results.len()], false)
            }
        };
        let Run {
            mut solver,
            results,
            checks,
            passed,
            pending,
            calls,
            logged,
            outcomes,
            ..
        } = run;
        let bottom = self.db.bottom();
        let chosen = |solver: &Solver<'_>, term| solver.chosen(term).unwrap_or(false);
        for &(term, span, repeated) in &calls {
            if solver.reify(term).is_ok_and(|ty| ty == bottom) && !chosen(&solver, term) {
                self.diverge(span, decided);
                if !repeated {
                    values.fill(bottom);
                }
            }
        }
        let chosen = (values.iter().zip(&results))
            .map(|(&ty, &term)| ty == bottom && chosen(&solver, term))
            .collect();
        if self.observing() {
            self.blame(&solver, &checks, &outcomes[..checks.len()]);
            let unknown = self.db.unknown();
            let reify = |term| Some(solver.reify(term).unwrap_or(unknown));
            for pending in logged {
                if let Some((span, role, seen)) = pending.resolve(&solver, reify) {
                    self.log(span, role, seen);
                }
            }
        } else {
            for (var, ty) in self::passed(&mut solver, self.db, &results, &passed, &pending) {
                self.join(var, ty);
            }
        }
        Decision {
            values,
            decided,
            chosen,
        }
    }

    /// Diagnose the checks a rule's final run contradicted, and record the ones it
    /// couldn't decide
    fn blame(&mut self, solver: &Solver<'_>, checks: &[Check], outcomes: &[Outcome]) {
        for (check, outcome) in checks.iter().zip(outcomes) {
            let span = match *check {
                Check::Call { span, .. } | Check::Fits(span, _) | Check::Expected(span) => span,
                // Its rule reports it left undecided
                Check::Empty(_) if outcome.status == Status::Unresolved => continue,
                Check::Empty(span) => span,
                Check::Quiet => continue,
            };
            match outcome.status {
                Status::Proven => {}
                Status::Unresolved => match check {
                    Check::Call { callee, .. }
                        if let Some(survivors) = self.ambiguous(solver, outcome) =>
                    {
                        match survivors {
                            Some(survivors) => self.problem(Problem::AmbiguousCall {
                                span,
                                callee: callee.clone(),
                                survivors,
                            }),
                            // What would tell them apart may be unknown, so
                            // it's only undecided
                            None => self.undecided(span, Residual::Ambiguous),
                        }
                    }
                    _ => self.undecided(span, super::residual(outcome)),
                },
                Status::Contradicted => {
                    let relation = solver.relation(root(outcome));
                    // The callee, or the value that doesn't fit, as what's found
                    // of it if an argument held back in it is left unsolved
                    let checked = relation.checked();
                    let actual = (solver.reify(checked))
                        .or_else(|_| solver.reify_found(checked))
                        .ok();
                    let problems: Vec<Problem> = match check {
                        // An argument is reported once, with a note for each of
                        // its parts that doesn't fit
                        Check::Call { span, args, callee } => {
                            let mut problems = Vec::new();
                            for cause in solver.explain(outcome).iter() {
                                if let (Contradiction::NoOverload, [_]) =
                                    (cause.contradiction, &cause.path[..])
                                {
                                    let overloaded = root(outcome);
                                    let notes = self.rejections(solver, *span, args, overloaded);
                                    merge(
                                        &mut problems,
                                        Problem::NoOverload {
                                            span: *span,
                                            callee: callee.clone(),
                                            notes,
                                        },
                                    );
                                    continue;
                                }
                                let problem = self.call_problem(
                                    *span,
                                    args,
                                    &cause.path,
                                    cause.contradiction,
                                    &actual.map(|callee| self.shown(callee)),
                                );
                                merge(&mut problems, problem);
                            }
                            problems
                        }
                        &Check::Fits(span, misfit) => vec![Problem::Misfit {
                            span,
                            found: actual
                                .map_or(Shown::Type("?".to_owned()), |found| self.subject(found)),
                            misfit,
                        }],
                        &Check::Expected(span) | &Check::Empty(span) => {
                            let mut notes = Vec::new();
                            for cause in solver.explain(outcome).iter() {
                                let steps = steps(&cause.path);
                                if let Some(note) =
                                    self.cause(&cause.path, &steps, 0, cause.contradiction)
                                    && !notes.contains(&note)
                                {
                                    notes.push(note);
                                }
                            }
                            let expected = (relation.sides())
                                .and_then(|(_, expected)| solver.reify(expected).ok());
                            let (found, expected) = match (actual, expected) {
                                (Some(found), Some(expected)) => {
                                    let (found, expected) = self.pair(found, expected);
                                    (found, Some(expected))
                                }
                                (found, _) => (
                                    found.map_or(Shown::Type("?".to_owned()), |found| {
                                        self.subject(found)
                                    }),
                                    None,
                                ),
                            };
                            vec![Problem::Argument {
                                span,
                                found,
                                expected,
                                causes: notes,
                            }]
                        }
                        Check::Quiet => Vec::new(),
                    };
                    for problem in problems {
                        self.problem(problem);
                    }
                }
            }
        }
    }

    /// Why each overload of an overloaded callee was rejected by a call: the
    /// overload, and what its trial's contradictions say, as they would of a
    /// call of it alone
    fn rejections(
        &self,
        solver: &Solver<'_>,
        span: Span,
        args: &[Span],
        overloaded: ObligationId,
    ) -> Vec<String> {
        let mut notes = Vec::new();
        for rejection in solver.rejections(overloaded).iter() {
            let overload = (rejection.overload)
                .map(|ty| self.overload(solver, overloaded, rejection.index, ty));
            let mut problems = Vec::new();
            for cause in &rejection.contradictions {
                let problem =
                    self.call_problem(span, args, &cause.path, cause.contradiction, &overload);
                merge(&mut problems, problem);
            }
            let mut reasons = Vec::new();
            for problem in problems {
                let mut reason = String::new();
                let _ = problem.message(&mut reason);
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
            }
            let overload = overload.unwrap_or_else(|| "?".to_owned());
            notes.push(match &reasons[..] {
                [] => format!("`{overload}`"),
                _ => format!("`{overload}`: {}", reasons.join("; ")),
            });
        }
        notes
    }

    /// Whether a call's check is left unresolved only by an ambiguous choice of
    /// overload, and if so, the overloads left possible, unless what it's
    /// passed is partly dynamic. Several may then fit only for lack of what
    /// would tell them apart, so the call is only undecided.
    fn ambiguous(&self, solver: &Solver<'_>, outcome: &Outcome) -> Option<Option<Vec<String>>> {
        let mut overloaded = None;
        for diagnostic in &outcome.diagnostics {
            match (diagnostic.issue, &diagnostic.path[..]) {
                (Issue::Residual(Residual::Ambiguous), &[obligation]) => {
                    overloaded = Some(obligation)
                }
                _ => return None,
            }
        }
        let overloaded = overloaded?;
        let possible = solver.possible(overloaded);
        if possible.is_empty() {
            return None;
        }
        let Relation::Call(call) = solver.relation(overloaded) else {
            return Some(None);
        };
        // What chooses: the arguments' twin and the ambient channels
        let selection = [Some(solver.twin(call.arguments)), call.input, call.output];
        let gradual = (selection.into_iter().flatten())
            .any(|term| (solver.reify(term)).map_or(true, |ty| self.db.contains_unknown(ty)));
        if gradual {
            return Some(None);
        }
        let overloads = match solver.reify(call.callee).map(|ty| self.db.ty(ty)) {
            Ok(Type::Overloaded { overloads, .. }) => overloads.clone(),
            Ok(&Type::Decl(decl)) => (self.db.overloads(decl).iter())
                .map(|&overload| self.db.declaration(overload).ty)
                .collect(),
            _ => return Some(None),
        };
        let survivors = (possible.into_iter())
            .filter_map(|index| Some((index, *overloads.get(index)?)))
            .map(|(index, overload)| self.overload(solver, overloaded, index, overload))
            .collect();
        Some(Some(survivors))
    }

    /// An overload of the overloaded callee of `overloaded`'s judgment, by its
    /// index, as its declaration names it
    fn overload(
        &self,
        solver: &Solver<'_>,
        overloaded: ObligationId,
        index: usize,
        ty: TypeId,
    ) -> String {
        let callee = solver.relation(overloaded).checked();
        let function = match solver.reify(callee).map(|ty| self.db.ty(ty)) {
            Ok(&Type::Overloaded { function, .. }) => function,
            Ok(&Type::Decl(decl)) => Some(decl),
            _ => None,
        };
        match function.and_then(|function| self.db.overloads(function).get(index)) {
            Some(&decl) => (self.db).render_declared(decl, ty, &self.names(), Style::Reader),
            None => self.shown(ty),
        }
    }

    /// What a contradiction under a call's constraint says, through the path of
    /// derivations to it
    fn call_problem(
        &self,
        span: Span,
        args: &[Span],
        path: &[Node],
        contradiction: Contradiction,
        callee: &Option<String>,
    ) -> Problem {
        let steps = steps(path);
        match contradiction {
            Contradiction::Conflict => return Problem::Conflict(span),
            Contradiction::Unadmitted(key) => {
                let key = self.subject(key);
                return Problem::Unadmitted { span, key };
            }
            Contradiction::Required(key) => {
                let key = self.subject(key);
                return Problem::Required { span, key };
            }
            Contradiction::Unrepeatable(key) => {
                let key = self.subject(key);
                return Problem::Unrepeatable { span, key };
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
        let Some(arguments) = steps.iter().position(|step| *step == Derivation::Arguments) else {
            return fallback;
        };
        match steps.get(arguments + 1) {
            Some(&(Derivation::Item(index) | Derivation::Key(index))) => {
                let Some(&arg) = args.get(index) else {
                    return fallback;
                };
                let mut root = arguments + 2;
                // An argument held back that's left unsolved shows what it's
                // made of, as does one solved as what it's made of
                if let Some(Derivation::BoundPropagation | Derivation::Assignment) = steps.get(root)
                    && let Some((found, _)) = path[root].sides
                    && (found.is_none() || path[root].sides == path[root + 1].sides)
                {
                    root += 1;
                }
                // A write through a key selecting several items shows the item
                // the argument doesn't fit
                if let Some(Derivation::Selected(_)) = steps.get(root) {
                    root += 1;
                }
                // What's found of an argument held back that's left unsolved
                let Some((Some(found), expected)) = (path[root].sides)
                    .map(|(found, expected)| (found.or(path[root].found), expected))
                else {
                    return fallback;
                };
                // A parameter whose type is left unsolved shows the bound of it
                // that the argument doesn't fit
                let expected = expected.or_else(|| match steps.get(root) {
                    Some(Derivation::BoundPropagation) => path[root + 1].sides?.1,
                    _ => None,
                });
                let (found, expected) = match expected {
                    Some(expected) => {
                        let (found, expected) = self.pair(found, expected);
                        (found, Some(expected))
                    }
                    None => (self.subject(found), None),
                };
                // What fails may lie deeper than the argument's own relation, as
                // in a binder's bound its type solves
                let causes = (self.cause(path, &steps, root, contradiction))
                    .into_iter()
                    .collect();
                Problem::Argument {
                    span: arg,
                    found,
                    expected,
                    causes,
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

    /// Notes on a contradiction below the relation at `path[start]`, which its
    /// diagnostic shows: what doesn't fit what, then each type found around it,
    /// innermost first. Each step that reverses a relation, as into a
    /// contravariant position, swaps which of its sides is the found one. A
    /// literal's class only restates the literal, and a contradiction of the
    /// shown relation itself needs a note only to name an item.
    fn cause(
        &self,
        path: &[Node],
        steps: &[Derivation],
        start: usize,
        contradiction: Contradiction,
    ) -> Option<Vec<String>> {
        let mut end = path.len();
        while end > start + 1 && matches!(steps[end - 2], Derivation::IntrinsicBacking(_)) {
            end -= 1;
        }
        // Whether each obligation's expected side is the found one
        let reversed: Vec<bool> = (steps[start..end - 1].iter())
            .scan(false, |reversed, step| {
                *reversed ^= step.reverses();
                Some(*reversed)
            })
            .collect();
        let reversed = |index: usize| index > start && reversed[index - start - 1];
        // Below a check's root, only subtypes are derived
        let sides = |index: usize| path[index].sides;
        let found_side = |index: usize| {
            let (actual, expected) = sides(index)?;
            match reversed(index) {
                false => actual,
                true => expected,
            }
        };
        let (leaf_actual, leaf_expected) = sides(end - 1)?;
        let (actual, expected) = self.pair(leaf_actual?, leaf_expected?);
        let note = match (contradiction, reversed(end - 1)) {
            (Contradiction::Missing(item), flipped) => {
                match (
                    leaf_expected.and_then(|schema| self.schema_item(schema, item)),
                    flipped,
                ) {
                    (Some(item), false) => format!("{actual} may be missing {item}"),
                    (Some(item), true) => {
                        format!("{expected} requires {item}, which {actual} may be missing")
                    }
                    (None, _) => format!("{actual} may be missing an item of {expected}"),
                }
            }
            (Contradiction::Excess(item), _) => {
                match leaf_actual.and_then(|schema| self.schema_item(schema, item)) {
                    Some(item) => format!("{expected} doesn't admit {item}"),
                    None => format!("{expected} doesn't admit every item of {actual}"),
                }
            }
            _ if end == start + 1 => return None,
            (_, false) => format!("{actual} does not fit {expected}"),
            (_, true) => format!("{expected} doesn't accept {actual}"),
        };
        let mut notes = vec![note];
        let shown = found_side(start).map(|ty| self.shown(ty));
        let mut last = found_side(end - 1).map(|ty| self.shown(ty));
        for index in (start + 1..end - 1).rev() {
            // Only steps into a type's parts are where a reader can look
            if !matches!(
                steps[index - 1],
                Derivation::Argument { .. }
                    | Derivation::Parameters
                    | Derivation::Arguments
                    | Derivation::Return
                    | Derivation::Input
                    | Derivation::Output
                    | Derivation::UnionMember(_)
                    | Derivation::Item(_)
                    | Derivation::Key(_)
            ) {
                continue;
            }
            let Some(around) = found_side(index).map(|ty| self.shown(ty)) else {
                continue;
            };
            if Some(&around) == last.as_ref() || Some(&around) == shown.as_ref() {
                continue;
            }
            notes.push(format!("in `{around}`"));
            last = Some(around);
        }
        Some(notes)
    }

    /// A schema's top-level item as a diagnostic names it: quoted without the
    /// schema's braces, and called positional if it is
    fn schema_item(&self, schema: TypeId, index: usize) -> Option<String> {
        let Type::Schema(items) = self.db.ty(schema) else {
            return None;
        };
        let item = items.get(index)?;
        let positional = matches!(item.element, Element::Positional(_));
        let rendered = self.shown(self.db.intern(Type::Schema(vec![item.clone()].into())));
        let inner = rendered.strip_prefix('{')?.strip_suffix('}')?;
        Some(match positional {
            true => format!("positional `{inner}`"),
            false => format!("`{inner}`"),
        })
    }

    /// A call: its callee's type below the function type its arguments call it as.
    /// A `do` block among them enters as its function type, with a fresh variable
    /// for each parameter and channel its signature leaves open. What the callee
    /// passes them joins into its signature, and its result, until the block's
    /// analysis gives it, is a variable that keeps the call undecided. An argument
    /// that takes an expectation its generic callee's signature alone doesn't give
    /// is held back, and released in the call's rule (see [`Flow::release`]).
    /// A comprehension's arguments are passed as often as it says: see
    /// [`Flow::gather`]. A callee that isn't a function is called as
    /// [`Flow::call_value`] says.
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
            callee: None,
            logs: true,
        };
        self.call_value(at, state, operands, (callee_type, callee.span), &[], call)
    }

    /// A call of an evaluated callee, passing `leading` before the call's own
    /// arguments, as calling a member's value passes an operator's operands. How
    /// it reaches a signature is [`Flow::call_target`]'s. Each alternative of a
    /// union must take the call (see [`Flow::call_union`]).
    pub(super) fn call_value(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        callee: (TypeId, Span),
        leading: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> TypeId {
        let targets = self.value_targets(callee, leading, call.span);
        self.call_targets(at, state, operands, targets, call)
    }

    /// The targets calling `callee` reaches, passing `leading` before the call's
    /// own arguments: one for each alternative of a union
    pub(super) fn value_targets(
        &mut self,
        callee: (TypeId, Span),
        leading: &[(TypeId, Span)],
        span: Span,
    ) -> Vec<CallTarget> {
        let mut alternatives = Vec::new();
        let mut targets = if self.expand(callee.0, &mut alternatives) {
            (alternatives.into_iter())
                .map(|alternative| match alternative {
                    Some(ty) => self.call_target((ty, callee.1), span),
                    None => {
                        self.gap(span, "a projection in a union callee");
                        self.call_target((self.db.unknown(), callee.1), span)
                    }
                })
                .collect()
        } else {
            vec![self.call_target(callee, span)]
        };
        for target in &mut targets {
            target.receivers.extend_from_slice(leading);
        }
        targets
    }

    /// A call through each of `targets`, which must all take it. Several are
    /// called as [`Flow::call_union`] says.
    pub(super) fn call_targets(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        targets: Vec<CallTarget>,
        call: Call<'_>,
    ) -> TypeId {
        match <[CallTarget; 1]>::try_from(targets) {
            Ok([target]) => {
                let receivers = &target.receivers;
                let call = Call {
                    callee: target.member.or(call.callee),
                    ..call
                };
                let given =
                    self.call_signature(at, state, operands, &target.signature, receivers, call);
                target.gives(given, self.db.bottom())
            }
            Err(targets) => self.call_union(at, state, operands, &targets, call),
        }
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
        let prepared = self.prepare_call(at, state, operands, callee_type, receivers, call);
        self.finish_prepared(at, prepared, call.expected)
    }

    /// A call of `callee` as [`Flow::call_with`] makes it, with its arguments
    /// evaluated and its expectation not yet given
    pub(super) fn prepare_call(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        callee: TypeId,
        receivers: &[(TypeId, Span)],
        call: Call<'_>,
    ) -> PreparedCall {
        let mut params = self.params(callee).unwrap_or_default();
        let skipped = receivers.len().min(params.positional.len());
        params.positional.drain(..skipped);
        let generic =
            matches!(self.db.ty(callee), Type::Quantified { .. }) || self.overloaded(callee);
        let mut values = self.values(at, state, operands, call.args, Some(&params), true, generic);
        received(&mut values, receivers, self.db.bottom());
        PreparedCall {
            callee,
            values,
            member: call.callee,
            span: call.span,
            logs: call.logs,
            channels: self.channels(at),
        }
    }

    /// Finish a call [`Flow::prepare_call`] made, expecting `expected` of its
    /// result
    pub(super) fn finish_prepared(
        &mut self,
        at: At,
        prepared: PreparedCall,
        expected: Option<TypeId>,
    ) -> TypeId {
        let call = prepared.call(expected);
        self.finish_call(at, prepared.callee, &prepared.values, call)
    }

    /// A call of a union's alternatives, its arguments evaluated once. Each is
    /// solved separately, with the arguments held back released in its own rule,
    /// so a diagnostic's argument indexes are its own receivers', and the call
    /// gives what they give.
    fn call_union(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        targets: &[CallTarget],
        call: Call<'_>,
    ) -> TypeId {
        let (bottom, unknown) = (self.db.bottom(), self.db.unknown());
        let mut values = self.values(at, state, operands, call.args, None, true, true);
        values.never |=
            (targets.iter().flat_map(|target| &target.receivers)).any(|&(ty, _)| ty == bottom);
        if values.never {
            self.diverge(call.span, false);
            return bottom;
        }
        // An alternative that never returns ends the path only if they all do
        let diverged = self.diverged;
        let mut result = bottom;
        for target in targets {
            received(&mut values, &target.receivers, bottom);
            let callee = target.signature.callee(self.db).unwrap_or(unknown);
            let call = Call {
                callee: target.member.or(call.callee),
                ..call
            };
            let given = self.finish_call(at, callee, &values, call);
            values.values.drain(..target.receivers.len());
            let given = target.gives(given, bottom);
            trace!(
                "{}: alternative {} gives {}",
                self.tables.locate(self.unit, call.span),
                self.show(callee),
                self.show(given)
            );
            result = self.solver(&Gas::default()).lub(result, given);
        }
        if result != bottom {
            self.diverged = diverged;
        }
        result
    }

    /// Finish a call of `callee` with its arguments evaluated, and check it. The
    /// arguments held back are released in its rule (see [`Flow::release`]).
    fn finish_call(
        &mut self,
        at: At,
        callee_type: TypeId,
        values: &Values,
        call: Call<'_>,
    ) -> TypeId {
        let Call { expected, span, .. } = call;
        let name = self.call_name(call.callee, callee_type);
        let bottom = self.db.bottom();
        let (input, output) = self.channels(at);
        if callee_type == bottom || values.never {
            self.diverge(span, false);
            return bottom;
        }
        if !self.callable(callee_type) {
            self.unchecked(at, values, span);
            return self.db.unknown();
        }
        let spread = self.designated(Designated::Spread);
        let decision = self.decide(at, span, expected, |_, rule| {
            vec![call_constraint(
                rule,
                values,
                spread,
                (span, name.clone(), call.logs),
                callee_type,
                input,
                output,
            )]
        });
        let given = decision.values[0];
        if given == bottom && !decision.chosen[0] {
            self.diverge(span, decision.decided);
        }
        trace!(
            "{}: call of {} gives {}",
            self.tables.locate(self.unit, span),
            self.show(callee_type),
            self.show(given)
        );
        given
    }

    /// What a call's diagnostics name its callee: the member it calls, or the
    /// declaration
    fn call_name(&self, member: Option<Member>, callee: TypeId) -> Option<String> {
        match (member, self.db.ty(callee)) {
            (Some(member), _) => Some(self.member_name(member)),
            (None, Type::Decl(_)) => Some(self.shown(callee)),
            _ => None,
        }
    }

    /// Check what no rule will among values: instantiate each `do` block as
    /// anywhere else, and release the arguments held back without an expectation
    fn unchecked(&mut self, at: At, values: &Values, span: Span) {
        self.untyped(at, values);
        if !values.held.is_empty() {
            self.conclude(at, span, None, |_, rule| {
                for prepared in &values.held {
                    rule.hold(prepared, false);
                }
                Vec::new()
            });
        }
    }

    /// Whether a callee is a function, or an overloaded one, or a union of them,
    /// as a variable assigned several closures holds
    pub(super) fn callable(&self, callee: TypeId) -> bool {
        let function = |ty: TypeId| {
            if self.overloaded(ty) {
                return true;
            }
            let ty = match self.db.ty(ty) {
                Type::Quantified { body, .. } => *body,
                _ => ty,
            };
            matches!(self.db.ty(ty), Type::Function(_))
        };
        let mut alternatives = Vec::new();
        if self.expand(callee, &mut alternatives) {
            (alternatives.into_iter()).all(|alternative| alternative.is_some_and(function))
        } else {
            function(callee)
        }
    }

    /// Evaluate an item held back as far as it can be without its expectation.
    /// What's left to finish is held back; anything else is evaluated whole,
    /// giving its type: a call of a union's alternatives, or one its rule won't
    /// check, and what's never produced.
    fn prepare(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
    ) -> Result<Prepared, TypeId> {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => {
                let callee_type = self.eval(at, state, operands, callee);
                let targets = self.value_targets((callee_type, callee.span), &[], expr.span);
                let call = Call {
                    args,
                    expected: None,
                    span: expr.span,
                    callee: None,
                    logs: true,
                };
                match <[CallTarget; 1]>::try_from(targets) {
                    Ok([target]) => {
                        let callee =
                            (target.signature.callee(self.db)).unwrap_or(self.db.unknown());
                        let call = Call {
                            callee: target.member,
                            ..call
                        };
                        let receivers = &target.receivers;
                        let call = self.prepare_call(at, state, operands, callee, receivers, call);
                        self.held_call(at, call, target.instance())
                    }
                    Err(targets) => Err(self.call_union(at, state, operands, &targets, call)),
                }
            }
            ExprKind::Range { .. } => match self.prepare_range(at, state, operands, expr) {
                Ok(call) => self.held_call(at, call, None),
                Err(ty) => Err(ty),
            },
            ExprKind::Collection { .. } => {
                let collection = self.prepare_collection(at, state, operands, expr, None);
                match self.unchecked_collection(at, &collection) {
                    Some(ty) => Err(ty),
                    None => Ok(Prepared::Collection(collection)),
                }
            }
            ExprKind::Fmt(_) => match self.eval(at, state, operands, expr) {
                ty if ty == self.db.bottom() => Err(ty),
                ty => Ok(Prepared::Fmt(ty)),
            },
            _ => unreachable!("an item that takes an expectation"),
        }
    }

    /// A call held back, giving `instance` if it's a constructor with unchecked
    /// arguments, unless its rule won't check it: then it's finished now
    fn held_call(
        &mut self,
        at: At,
        call: PreparedCall,
        instance: Option<TypeId>,
    ) -> Result<Prepared, TypeId> {
        let bottom = self.db.bottom();
        if call.callee == bottom || call.values.never || !self.callable(call.callee) {
            let given = self.finish_prepared(at, call, None);
            return Err(match instance {
                Some(instance) if given != bottom => instance,
                _ => given,
            });
        }
        Ok(Prepared::Call { call, instance })
    }

    /// Release the arguments a rule holds back, in order. Each is a sub-rule,
    /// made in a fork of the rule's solver that the rule then takes over: its
    /// checks and `do` blocks are the rule's, and what's solved so far gives
    /// its expectation, which leaves the binders it names open (see
    /// [`expectation`]), and in a seeded run, it's bounded by that, as a fresh
    /// array is by the type that gives it. A template only takes the holes its expectation gives,
    /// once that's solved.
    fn release(&self, rule: &mut Rule<'_, '_, '_>) {
        let held = std::mem::take(&mut rule.held);
        let raisable: Vec<Term> = held.iter().map(|held| held.term).collect();
        for held in held {
            let expected = self.held_expectation(rule, held.term, &raisable);
            if let Prepared::Fmt(preview) = *held.prepared {
                let expected = expected.and_then(|expected| rule.solver.reify(expected).ok());
                let refined = self.contextual_fmt(preview, expected);
                if refined != preview {
                    rule.constrain(rule.closed(refined), held.term, Check::Quiet);
                }
                continue;
            }
            self.sub_rule(rule, held, expected);
        }
    }

    /// What an argument held back is expected to be: what's solved so far gives
    /// (see [`expectation`]), unless it's solved once every variable that the
    /// arguments held back, `raisable`, can't raise takes its least solution (see
    /// [`Solver::raised`]). Such a variable, as a key's type an item's parameter
    /// selects by, would otherwise wait on defaulting, though the arguments held
    /// back can't change it.
    fn held_expectation(
        &self,
        rule: &mut Rule<'_, '_, '_>,
        held: Term,
        raisable: &[Term],
    ) -> Option<Term> {
        let below = |actual, expected| self.below(actual, expected);
        let open = expectation(rule.solver, held, below);
        if open.is_some_and(|expected| rule.solver.reify(expected).is_ok()) {
            return open;
        }
        let solved = {
            let mut fork = rule.solver.fork(rule.gas);
            let raised = (fork.raised(raisable)).unwrap_or_else(|_| fork.unresolved().collect());
            default_where(&mut fork, |id| !raised.contains(&id), None);
            expectation(&fork, held, below).and_then(|expected| fork.reify(expected).ok())
        };
        solved.map(|ty| rule.closed(ty)).or(open)
    }

    /// Make the sub-rule of an argument held back, and have the rule take it
    /// over. In an unseeded run, it's solved alone and passed closed.
    fn sub_rule<'p>(&self, rule: &mut Rule<'_, '_, 'p>, held: Held<'p>, expected: Option<Term>) {
        let (db, gas) = (rule.db, rule.gas);
        let lengths = (rule.checks.len(), rule.passed.len(), rule.pending.len());
        let logged = rule.logged.len();
        let delta = {
            let mut fork = rule.solver.fork(gas);
            let mut sub = Rule {
                checks: std::mem::take(&mut rule.checks),
                passed: std::mem::take(&mut rule.passed),
                pending: std::mem::take(&mut rule.pending),
                calls: std::mem::take(&mut rule.calls),
                logging: rule.logging,
                logged: std::mem::take(&mut rule.logged),
                found: std::mem::take(&mut rule.found),
                repeated: held.repeated,
                defaulting: rule.defaulting,
                seed: rule.seed,
                seeded: rule.seeded,
                exact: rule.exact,
                blind: rule.blind,
                ..Rule::new(&mut fork, db, gas)
            };
            let (given, seeded) = self.held_term(&mut sub, held.prepared, expected);
            if sub.seed {
                if let Some(seeded) = seeded {
                    sub.constrain(given, seeded, Check::Quiet);
                }
                sub.constrain(given, held.term, Check::Quiet);
                sub.seeded = true;
            }
            sub.solver.solve();
            if !sub.held.is_empty() {
                self.release(&mut sub);
                sub.solver.solve();
            }
            // In a run that defaults, it's what its own defaults make it, as a
            // rule of its own would be. A `do` block's result that isn't known
            // yet is bottom, as a defaulting run takes it.
            if sub.defaulting {
                for index in lengths.2..sub.pending.len() {
                    let term = sub.pending[index];
                    sub.constrain(sub.closed(db.bottom()), term, Check::Quiet);
                }
                let roots = outputs(&[given], &[]);
                default_own(sub.solver, Some(&roots));
            }
            // Unseeded, it's solved alone, and what it's solved as is passed
            // closed, so what the argument doesn't fit is reported only against
            // the argument
            if !sub.seed {
                let given = sub.solver.reify(given).map_or(given, |ty| sub.closed(ty));
                sub.constrain(given, held.term, Check::Quiet);
            }
            // A value that's solved, or that only solved types are below, as a
            // call's of a function that isn't generic is, is the argument
            // exactly, as if it were passed closed, so it chooses nothing more
            // than that would. One holding an empty literal's open part isn't:
            // what it's in decides that part.
            let exact = (!sub.solver.holds_fresh(given))
                .then(|| {
                    sub.solver.reify(given).ok().or_else(|| match given {
                        Term::Infer(id)
                            if (sub.solver.bounds(id).lower())
                                .all(|lower| sub.solver.reify(lower).is_ok()) =>
                        {
                            sub.solver.reify_found(given).ok()
                        }
                        _ => None,
                    })
                })
                .flatten();
            if let Some(exact) = exact {
                sub.constrain(held.term, sub.closed(exact), Check::Quiet);
                // Found exactly unless what it checks of its own is left
                // undecided, or it passes a `do` block values
                let outcomes = sub.solver.solve();
                let decided =
                    (sub.checks[lengths.0..].iter().zip(&outcomes)).all(|(check, outcome)| {
                        match outcome.status {
                            Status::Proven => true,
                            Status::Unresolved => matches!(check, Check::Quiet),
                            Status::Contradicted => false,
                        }
                    });
                if decided
                    && exact != db.bottom()
                    && (sub.passed.len(), sub.pending.len()) == (lengths.1, lengths.2)
                {
                    let solver = &*sub.solver;
                    let reify = |term| {
                        solver
                            .reify(term)
                            .or_else(|_| solver.reify_found(term))
                            .ok()
                    };
                    let logged = sub.logged[logged..]
                        .iter()
                        .filter_map(|pending| pending.resolve(solver, reify))
                        .collect();
                    sub.found.push(Found {
                        prepared: held.prepared,
                        ty: exact,
                        logged,
                    });
                }
            }
            let Rule {
                checks,
                passed,
                pending,
                calls,
                logged,
                found,
                seeded,
                ..
            } = sub;
            (
                rule.checks,
                rule.passed,
                rule.pending,
                rule.calls,
                rule.logged,
                rule.found,
                rule.seeded,
            ) = (checks, passed, pending, calls, logged, found, seeded);
            fork.commit()
        };
        rule.solver.absorb(delta);
    }

    /// The term of an argument held back, toward `expected`, and what it
    /// must be besides
    fn held_term<'p>(
        &self,
        rule: &mut Rule<'_, '_, 'p>,
        prepared: &'p Prepared,
        expected: Option<Term>,
    ) -> (Term, Option<Term>) {
        match prepared {
            Prepared::Call { call, instance } => {
                let spread = self.designated(Designated::Spread);
                let name = self.call_name(call.member, call.callee);
                let (input, output) = call.channels;
                let result = call_constraint(
                    rule,
                    &call.values,
                    spread,
                    (call.span, name, call.logs),
                    call.callee,
                    input,
                    output,
                );
                rule.calls.push((result, call.span, rule.repeated));
                let given = instance.map_or(result, |instance| rule.closed(instance));
                (given, None)
            }
            Prepared::Collection(collection) => self.collection_term(rule, collection, expected),
            Prepared::Fmt(_) => unreachable!("a template is refined, not made"),
        }
    }

    /// A `do` block as a call's argument: its declared function type, with a hole
    /// for each parameter and channel its signature leaves to the call, and for
    /// its result until that's known. A rest's element type isn't left to the
    /// call. `None` if the block has no signature, or its declared type doesn't
    /// line up with its parameters.
    fn contextual(&mut self, at: At, func: FuncId) -> Option<Lambda> {
        let (db, ir) = (self.db, self.ir);
        let data = ir.func(func);
        let signature = data.signature.as_ref()?;
        let declared = self.declared[func.index()].clone()?;
        let mut holes = Vec::new();
        let hole = |holes: &mut Vec<Hole>, filled| {
            holes.push(filled);
            db.intern(Type::Bound {
                reference: BoundRef::new(0, holes.len() - 1),
                kind: Kind::Type,
            })
        };
        let Function {
            params,
            input,
            output,
            ..
        } = passed_signature(db, data, &declared, |var| {
            hole(&mut holes, Hole::Passed(var))
        })?;
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
            let bound = self.db.walk(ty, |node, depth| match *self.db.ty(node) {
                Type::Bound { reference, .. } if u32::from(reference.depth) >= depth => {
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            });
            bound.is_continue().then_some(ty)
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
    /// [`Flow::release`]).
    #[expect(clippy::too_many_arguments, reason = "how items are gathered")]
    fn values(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &[Item],
        params: Option<&Params>,
        contextual: bool,
        hold: bool,
    ) -> Values {
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
                    let ty = match hold(gathering, place, expected, value) {
                        true => match self.prepare(at, state, operands, value) {
                            Ok(prepared) => {
                                values.held.push(prepared);
                                let index = values.held.len() - 1;
                                let held = Value::Held(HeldAt::Positional, index, value.span);
                                out.push(placed(held));
                                continue;
                            }
                            Err(ty) => ty,
                        },
                        false => self.expect(at, state, operands, value, expected),
                    };
                    if produced(values, ty) {
                        out.push(placed(Value::Pos(ty, value.span)));
                    }
                }
                &Item::Key(key, ref value) => {
                    let expected = gathering.params.and_then(|params| {
                        (params.keyed.iter()).find_map(|&(name, ty)| (name == key).then_some(ty))
                    });
                    let ty = match hold(gathering, place, expected, value) {
                        true => match self.prepare(at, state, operands, value) {
                            Ok(prepared) => {
                                values.held.push(prepared);
                                let index = values.held.len() - 1;
                                let held = Value::Held(HeldAt::Keyword(key), index, value.span);
                                out.push(placed(held));
                                continue;
                            }
                            Err(ty) => ty,
                        },
                        false => self.expect(at, state, operands, value, expected),
                    };
                    if produced(values, ty) {
                        out.push(placed(Value::Key(key, ty, value.span)));
                    }
                }
                // The key is evaluated first, as its operands come first
                Item::Pair(key, value) => {
                    let key_type = self.eval(at, state, operands, key);
                    let key_produced = produced(values, key_type);
                    let ty = match key_produced && hold(gathering, place, None, value) {
                        true => match self.prepare(at, state, operands, value) {
                            Ok(prepared) => {
                                values.held.push(prepared);
                                let index = values.held.len() - 1;
                                let held = Value::Held(HeldAt::Pair(key_type), index, value.span);
                                out.push(placed(held));
                                continue;
                            }
                            Err(ty) => ty,
                        },
                        false => self.eval(at, state, operands, value),
                    };
                    if key_produced && produced(values, ty) {
                        out.push(placed(Value::Pair(key_type, ty)));
                    }
                }
                // What's spread is what the value's `(spread)` gives
                Item::Spread(value) => {
                    gathering.exact = false;
                    let mut ty = self.eval(at, state, operands, value);
                    if self.designated(Designated::Spread).is_some() {
                        ty = self.send_special(at, state, ty, "spread", value.span);
                    }
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
            self.lambda(at, lambda.func, Span::INVALID);
        }
    }

    /// A collection literal of a designated class. An array joins its items into
    /// its element type, whether a comprehension repeats them or not, and so does a
    /// dict, unless something is expected of it: then its schema is its items'. A
    /// union expects several schemas of a dict; it takes the one its items fit,
    /// or else the one its keys do, and its items held back take what that
    /// schema expects of them. A tuple or record takes its items' schema; neither
    /// has a vertical form, so neither holds a comprehension. With an expected
    /// type, the collection types the `do` blocks among its items.
    pub(super) fn collection(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Collection { kind, .. } = &expr.kind else {
            unreachable!("a collection")
        };
        let fresh = self.fresh(*kind, self.designated(collection_role(*kind)), expected);
        let prepared = self.prepare_collection(at, state, operands, expr, Some(&fresh));
        if let Some(ty) = self.unchecked_collection(at, &prepared) {
            return ty;
        }
        let seed = match prepared.kind {
            Collection::Array => fresh.array.map(|(ty, _)| ty).or(expected),
            _ => expected,
        };
        self.conclude(at, prepared.span, seed, |flow, rule| {
            let expected = expected.map(|ty| rule.closed(ty));
            vec![flow.collection_term(rule, &prepared, expected).0]
        })[0]
    }

    /// What a fresh collection of `class` can be to be `expected`. Only a fresh
    /// array can widen its invariant element type to an expected supertype, such
    /// as `BaseArray[E]` or `Iterable[E]`: it types its items with `E`, and
    /// solves as `Array[E]`. A dict expected to be a supertype of `Dict[S]`,
    /// such as `BaseDict[S]`, is a `Dict[S]`, which only a fresh dict can be.
    fn fresh(&self, kind: Collection, class: Option<DeclId>, expected: Option<TypeId>) -> Fresh {
        let (array, candidates) = match (kind, class, expected) {
            (Collection::Array, Some(class), Some(expected)) => {
                (self.fresh_expected(class, Kind::Type, expected), Vec::new())
            }
            (Collection::Dict, Some(class), Some(expected)) => {
                (None, self.fresh_candidates(class, Kind::Schema, expected))
            }
            _ => (None, Vec::new()),
        };
        Fresh {
            expected,
            array,
            candidates,
        }
    }

    /// A collection's items, evaluated against what `fresh` says is expected of
    /// them. A dict with candidates holds back the items that take an
    /// expectation until its schema is chosen, then gives them the schema's.
    /// Without `fresh`, the collection is itself held back: its items that take
    /// an expectation are held back too, and it types its `do` blocks.
    fn prepare_collection(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        fresh: Option<&Fresh>,
    ) -> PreparedCollection {
        let ExprKind::Collection { kind, items, .. } = &expr.kind else {
            unreachable!("a collection")
        };
        let params = (fresh.and_then(|fresh| fresh.array)).map(|(_, element)| Params {
            rest: Some(element),
            ..Params::default()
        });
        let (contextual, hold) = match fresh {
            Some(fresh) => (fresh.expected.is_some(), !fresh.candidates.is_empty()),
            None => (true, true),
        };
        let values = self.values(
            at,
            state,
            operands,
            items,
            params.as_ref(),
            contextual,
            hold,
        );
        PreparedCollection {
            kind: *kind,
            class: self.designated(collection_role(*kind)),
            values,
            span: expr.span,
        }
    }

    /// The type of a collection that no rule checks, if it's one: one that's
    /// never produced, a tuple or record with a comprehension, or one without
    /// its class
    fn unchecked_collection(&mut self, at: At, prepared: &PreparedCollection) -> Option<TypeId> {
        let PreparedCollection {
            kind,
            class,
            ref values,
            span,
        } = *prepared;
        if values.never {
            return Some(self.db.bottom());
        }
        if values.comprehension
            && !matches!(kind, Collection::Array | Collection::Dict)
            && class.is_some()
        {
            self.unchecked(at, values, span);
            return Some(self.gap(span, "a comprehension in a tuple or record"));
        }
        if class.is_none() {
            self.unchecked(at, values, span);
            return Some(self.db.unknown());
        }
        None
    }

    /// The term of a collection with its items evaluated, toward `expected`, and
    /// what it must be besides when it's held back: the type a fresh array
    /// solves as (see [`Flow::fresh`]). A dict with candidates takes the one
    /// [`Flow::choose`] does, and its items held back are released toward what
    /// that schema expects of them; otherwise it's built from the lanes its
    /// items can fill.
    fn collection_term<'p>(
        &self,
        rule: &mut Rule<'_, '_, 'p>,
        prepared: &'p PreparedCollection,
        expected: Option<Term>,
    ) -> (Term, Option<Term>) {
        let PreparedCollection {
            kind,
            class,
            ref values,
            span,
        } = *prepared;
        let class = class.expect("a collection of a designated class");
        let spread = self.designated(Designated::Spread);
        let mut seed = None;
        let shape = match kind {
            Collection::Array => {
                seed = (expected.and_then(|expected| rule.solver.reify(expected).ok()))
                    .and_then(|expected| self.fresh_expected(class, Kind::Type, expected))
                    .map(|(ty, _)| rule.closed(ty));
                Shape::Array
            }
            Collection::Dict => {
                let candidates = (expected.map(|expected| self.candidates(rule, class, expected)))
                    .unwrap_or_default();
                match self.choose(rule, class, &candidates, values, span) {
                    Some(candidate) => {
                        let (ty, schema) = self.candidate(rule, class, candidate);
                        Shape::Chosen(ty, schema)
                    }
                    None if values.values.is_empty() => Shape::Empty,
                    // A lane is present where an item can fill it, and an empty
                    // dict has the keyed lane
                    None => {
                        let (mut has_positional, mut has_keyed) = (false, false);
                        for placed in &values.values {
                            let (positional, keyed) = match placed.value {
                                Value::Pos(..)
                                | Value::Lambda(None, ..)
                                | Value::Held(HeldAt::Positional, ..) => (true, false),
                                Value::Key(..)
                                | Value::Pair(..)
                                | Value::Lambda(Some(_), ..)
                                | Value::Held(..) => (false, true),
                                Value::Spread(ty, _) => self.spread_lanes(spread, ty),
                            };
                            has_positional |= positional;
                            has_keyed |= keyed;
                        }
                        has_keyed |= !has_positional;
                        Shape::Dict(has_positional, has_keyed)
                    }
                }
            }
            Collection::Tuple => Shape::Items(Rest::Positional),
            Collection::Record => Shape::Items(Rest::All),
        };
        let term = collection_constraint(rule, values, class, shape, spread, span);
        (term, seed)
    }

    /// The candidates of a fresh dict of `class` expected to be `expected`: those
    /// of [`Flow::fresh_candidates`] if it's solved, and otherwise one for each
    /// alternative of it the class reaches with an argument that's expected of
    /// it, open in the binders the alternative names
    fn candidates(
        &self,
        rule: &mut Rule<'_, '_, '_>,
        class: DeclId,
        expected: Term,
    ) -> Vec<Candidate> {
        let closed = |ty: TypeId| {
            (self.fresh_candidates(class, Kind::Schema, ty).into_iter())
                .map(|(ty, schema)| Candidate::Closed(ty, schema))
        };
        if let Ok(ty) = rule.solver.reify(expected) {
            return closed(ty).collect();
        }
        // A variable says nothing of what the dict is
        let expected = rule.solver.resolve(expected).unwrap_or(expected);
        if let Term::Infer(_) = expected {
            return Vec::new();
        }
        let alternatives = (rule.solver.alternatives(expected)).unwrap_or_else(|| vec![expected]);
        let mut candidates = Vec::new();
        for alternative in alternatives {
            let found: Vec<Candidate> = match rule.solver.reify(alternative) {
                Ok(ty) => closed(ty).collect(),
                Err(_) if self.reaches(rule, class, alternative) => {
                    vec![Candidate::Open(alternative)]
                }
                Err(_) => Vec::new(),
            };
            for candidate in found {
                if !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
            }
        }
        candidates
    }

    /// Whether a fresh dict of `class` can be an open alternative, with an
    /// argument that's expected of it. One that admits whatever the class's
    /// bound does, as a binder's bound it's below may, says nothing of it.
    fn reaches(&self, rule: &mut Rule<'_, '_, '_>, class: DeclId, alternative: Term) -> bool {
        let mut fork = rule.solver.fork(rule.gas);
        let (argument, ty) = applied_term(&mut fork, rule.db, class, Kind::Schema);
        fork.constrain(ty, alternative, Provenance::default());
        if (fork.solve().iter()).any(|outcome| outcome.status == Status::Contradicted) {
            return false;
        }
        let Some(argument) = expectation(&fork, argument, |a, b| self.below(a, b)) else {
            return false;
        };
        let Ok(argument) = fork.reify(argument) else {
            return true;
        };
        let bound = match self.db.ty(self.db.declaration(class).ty) {
            Type::Quantified { binders, .. } => binders.first().and_then(|binder| binder.bound),
            _ => None,
        };
        !matches!(self.db.ty(argument), Type::Unknown(_))
            && !bound.is_some_and(|bound| self.below(bound, argument))
    }

    /// The candidate a fresh dict with `values` is: the only one, or else the one
    /// its items fit, or else the one its keys fit. A choice its values don't
    /// fit is then reported against it.
    fn choose<'p>(
        &self,
        rule: &mut Rule<'_, '_, 'p>,
        class: DeclId,
        candidates: &[Candidate],
        values: &'p Values,
        span: Span,
    ) -> Option<Candidate> {
        if let &[candidate] = candidates {
            return Some(candidate);
        }
        if candidates.is_empty() || values.never {
            return None;
        }
        // Two survivors are as many as it takes to choose none
        let survivors = |rule: &mut Rule<'_, '_, 'p>, keys_only: bool| {
            let mut found = Vec::new();
            for &candidate in candidates {
                if self.items_fit(rule, class, candidate, values, span, keys_only) {
                    found.push(candidate);
                    if found.len() == 2 {
                        break;
                    }
                }
            }
            found
        };
        let mut chosen = survivors(rule, false);
        if chosen.is_empty() {
            chosen = survivors(rule, true);
        }
        trace!(
            "{}: {} candidates, {}",
            self.tables.locate(self.unit, span),
            candidates.len(),
            match chosen[..] {
                [] => "none fits the dict".to_owned(),
                [Candidate::Closed(ty, _)] => format!("chose {}", self.show(ty)),
                [Candidate::Open(_)] => "chose an open candidate".to_owned(),
                _ => "several fit the dict".to_owned(),
            }
        );
        match chosen[..] {
            [candidate] => Some(candidate),
            _ => None,
        }
    }

    /// Whether a fresh dict's items, `values`, fit a candidate, in a fork of the
    /// rule's solver that's discarded. What it leaves unsolved of its own is
    /// defaulted, and with `keys_only`, each item's value is a fresh variable,
    /// so only its key can contradict.
    fn items_fit<'p>(
        &self,
        rule: &mut Rule<'_, '_, 'p>,
        class: DeclId,
        candidate: Candidate,
        values: &'p Values,
        span: Span,
        keys_only: bool,
    ) -> bool {
        let spread = self.designated(Designated::Spread);
        let (db, gas) = (rule.db, rule.gas);
        let mut fork = rule.solver.fork(gas);
        let mut probe = Rule::new(&mut fork, db, gas);
        probe.blind = true;
        let (_, schema) = self.candidate(&mut probe, class, candidate);
        let (mut arguments, _) = probe.arguments(values, spread, span);
        if keys_only {
            for (_, argument) in &mut arguments {
                match argument {
                    CallArgument::Positional(value)
                    | CallArgument::Keyword(_, value)
                    | CallArgument::Pair(_, value) => *value = probe.solver.infer(),
                    CallArgument::Spread(_) => {}
                }
            }
        }
        let exact = probe.solver.arguments_schema(&arguments);
        probe.constrain(exact, schema, Check::Quiet);
        let contradicted = |outcomes: &[Outcome]| {
            (outcomes.iter()).any(|outcome| outcome.status == Status::Contradicted)
        };
        let solver = probe.solver;
        !contradicted(&solver.solve()) && !contradicted(&default_own(solver, None))
    }

    /// A chosen candidate's type and schema. An open one's schema is what its
    /// alternative expects of the class's argument.
    fn candidate(
        &self,
        rule: &mut Rule<'_, '_, '_>,
        class: DeclId,
        candidate: Candidate,
    ) -> (Term, Term) {
        match candidate {
            Candidate::Closed(ty, schema) => (rule.closed(ty), rule.closed(schema)),
            Candidate::Open(alternative) => {
                let (argument, ty) = applied_term(rule.solver, rule.db, class, Kind::Schema);
                rule.constrain(ty, alternative, Check::Quiet);
                rule.solver.solve();
                let below = |actual, expected| self.below(actual, expected);
                let schema = expectation(rule.solver, argument, below).unwrap_or(argument);
                let ty = rule.term(|holes| {
                    let schema = holes.hole(schema, Kind::Schema);
                    holes.apply(class, vec![schema])
                });
                (ty, schema)
            }
        }
    }

    /// Which lanes of a dict a spread of `ty` can fill, positional then keyed:
    /// those of the schema it spreads, or both where that isn't known
    fn spread_lanes(&self, spread: Option<DeclId>, ty: TypeId) -> (bool, bool) {
        let Some(spread) = spread else {
            return (true, true);
        };
        let gas = Gas::default();
        let mut solver = self.solver(&gas);
        let schema = solver.infer_kind(Kind::Schema, Rest::All);
        let mut holes = Holes {
            db: self.db,
            group: Vec::new(),
        };
        let hole = holes.hole(schema, Kind::Schema);
        let target = holes.apply(spread, vec![hole]);
        let environment = solver.environment(solver.empty_environment(), holes.group);
        let target = solver.view(target, environment);
        solver.constrain(solver.closed(ty), target, Provenance::default());
        default_where(&mut solver, |_| true, None);
        match solver.reify(schema) {
            Ok(schema) => self.schema_lanes(schema),
            Err(_) => (true, true),
        }
    }

    /// Which lanes a schema has items in, positional then keyed; both where it
    /// isn't known
    fn schema_lanes(&self, schema: TypeId) -> (bool, bool) {
        let Type::Schema(items) = self.db.ty(schema) else {
            return (true, true);
        };
        let (mut positional, mut keyed) = (false, false);
        for item in items.iter() {
            match item.element {
                Element::Positional(_) => positional = true,
                Element::Keyed { .. } => keyed = true,
                Element::Include(inner) => {
                    let (inner_positional, inner_keyed) = self.schema_lanes(inner);
                    positional |= inner_positional;
                    keyed |= inner_keyed;
                }
            }
        }
        (positional, keyed)
    }

    /// What a fresh collection of `class` can be to be `expected`, one candidate for
    /// each alternative of it that the class reaches: `class` applied to the
    /// argument that alternative gives, and that argument, of `kind`. An
    /// alternative that gives no single argument gives no candidate.
    fn fresh_candidates(
        &self,
        class: DeclId,
        kind: Kind,
        expected: TypeId,
    ) -> Vec<(TypeId, TypeId)> {
        let argument = |expected: TypeId| {
            if let Type::Unknown(_) = self.db.ty(expected) {
                return None;
            }
            let gas = Gas::default();
            let mut solver = self.solver(&gas);
            let (argument, term) = applied_term(&mut solver, self.db, class, kind);
            solver.constrain(term, solver.closed(expected), Provenance::default());
            if (solver.solve().iter()).any(|outcome| outcome.status == Status::Contradicted) {
                return None;
            }
            (expectation(&solver, argument, |a, b| self.below(a, b)))
                .and_then(|argument| solver.reify(argument).ok())
                .filter(|&ty| !matches!(self.db.ty(ty), Type::Unknown(_)))
        };
        let mut alternatives = Vec::new();
        if !self.expand(expected, &mut alternatives) {
            alternatives = vec![Some(expected)];
        }
        let mut arguments: Vec<TypeId> = Vec::new();
        for argument in alternatives.into_iter().flatten().filter_map(argument) {
            if !arguments.contains(&argument) {
                arguments.push(argument);
            }
        }
        (arguments.into_iter())
            .map(|argument| {
                let applied = self.db.intern(Type::Apply {
                    base: self.db.intern(Type::Decl(class)),
                    args: vec![Argument::Positional(argument)].into(),
                    kind: Kind::Type,
                });
                (applied, argument)
            })
            .collect()
    }

    /// The one candidate of [`Flow::fresh_candidates`], if there's exactly one
    fn fresh_expected(
        &self,
        class: DeclId,
        kind: Kind,
        expected: TypeId,
    ) -> Option<(TypeId, TypeId)> {
        match self.fresh_candidates(class, kind, expected)[..] {
            [candidate] => Some(candidate),
            _ => None,
        }
    }

    /// The one application of `class` to a single argument that `expected` is, or
    /// that is a member of it, seen through aliases: the application, and its
    /// argument
    fn applied(&self, class: DeclId, expected: TypeId) -> Option<(TypeId, TypeId)> {
        let (ty, arguments) = self.application(class, expected)?;
        match arguments[..] {
            [arg] => Some((ty, arg)),
            _ => None,
        }
    }

    /// The one application of `class` that `expected` is, or that is a member of
    /// it, seen through aliases: the application, and its arguments
    fn application(&self, class: DeclId, expected: TypeId) -> Option<(TypeId, Vec<TypeId>)> {
        let gas = Gas::default();
        let solver = self.solver(&gas);
        let applied = |ty: TypeId| match solver.exposed_nominal(ty)? {
            (decl, arguments) if decl == class => Some((ty, arguments)),
            _ => None,
        };
        let mut alternatives = Vec::new();
        if !self.expand(expected, &mut alternatives) {
            return applied(expected);
        }
        let mut found = alternatives.into_iter().flatten().filter_map(applied);
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
        let span = values.first().map_or(Span::INVALID, |&(_, span)| span);
        self.conclude(at, span, None, |_, rule| {
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

    /// A `do` block a type is expected of, which types it as a call types the
    /// block it's given. Whoever expected it reports a block that doesn't fit.
    /// Without an expectation, or a signature to line it up with, it's
    /// instantiated as it is anywhere else.
    pub(super) fn expected_lambda(
        &mut self,
        at: At,
        func: FuncId,
        expected: Option<TypeId>,
        span: Span,
    ) -> TypeId {
        let Some((expected, lambda)) =
            expected.and_then(|expected| Some((expected, self.contextual(at, func)?)))
        else {
            return self.lambda(at, func, span);
        };
        self.conclude(at, span, None, |_, rule| {
            let term = rule.lambda(&lambda, span);
            rule.constrain(term, rule.closed(expected), Check::Quiet);
            vec![term]
        })[0]
    }

    /// A pattern item's default, joined with `present`, the item's type where
    /// it's present: a rule whose result lies above both, so that what the
    /// default leaves open takes the least choice, `present` if it fits there.
    /// Returns the default's own type, which joins `present` as the join does.
    pub(super) fn default_above(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        value: &Expr,
        present: TypeId,
    ) -> TypeId {
        let span = value.span;
        let held = matches!(
            value.kind,
            ExprKind::Collection { .. }
                | ExprKind::Range { .. }
                | ExprKind::Call { .. }
                | ExprKind::Fmt(_)
        );
        let mut values = Values::default();
        let placed = |value| Placed {
            value,
            multiplicity: Multiplicity::Required,
            group: None,
        };
        let prepared = match held {
            true => self.prepare(at, state, operands, value),
            false => Err(self.expect(at, state, operands, value, None)),
        };
        match prepared {
            Ok(prepared) => {
                values.held.push(prepared);
                values
                    .values
                    .push(placed(Value::Held(HeldAt::Positional, 0, span)));
            }
            Err(ty) if ty == self.db.bottom() => return ty,
            Err(ty) => values.values.push(placed(Value::Pos(ty, span))),
        }
        let spread = self.designated(Designated::Spread);
        self.conclude(at, span, None, |_, rule| {
            let (arguments, _) = rule.arguments(&values, spread, span);
            let [(_, CallArgument::Positional(found))] = arguments[..] else {
                unreachable!("one positional argument");
            };
            let joined = rule.solver.infer();
            rule.constrain(found, joined, Check::Quiet);
            rule.constrain(rule.closed(present), joined, Check::Quiet);
            vec![joined, found]
        })[1]
    }

    /// A value stored where its type must be `ty`, as a field is
    pub(super) fn store(&mut self, at: At, value: TypeId, ty: TypeId, span: Span) {
        if value == self.db.bottom() {
            return;
        }
        self.conclude(at, span, None, |_, rule| {
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
        let value_type = value.map(|value| self.eval(at, state, operands, value));
        let never = value_type == Some(bottom);
        let values: Vec<_> = ([&spec.width, &spec.precision].into_iter().flatten())
            .map(|part| (self.eval(at, state, operands, part), part.span))
            .collect();
        if never {
            return bottom;
        }
        let int = self.db.intrinsic(Intrinsic::Int);
        let result = match value_type {
            Some(value) => self.fmt_application(role, vec![self.db.decay(value)]),
            None => self.designated_type(role),
        };
        self.fits(at, &values, int, Misfit::Int, result)
    }

    /// A designated format class applied to its arguments. Synthetic standard
    /// declarations with no binders still describe the unparameterized class.
    fn fmt_application(&self, role: Designated, args: Vec<TypeId>) -> TypeId {
        let Some(class) = self.designated(role) else {
            return self.db.unknown();
        };
        if self.tables.binders(class, 0).is_empty() {
            return self.designated_type(role);
        }
        self.db.intern(Type::Apply {
            base: self.db.intern(Type::Decl(class)),
            args: args.into_iter().map(Argument::Positional).collect(),
            kind: Kind::Type,
        })
    }

    /// The unique expected template schema, including in a union.
    fn fmt_schema(&self, expected: Option<TypeId>) -> Option<TypeId> {
        let class = self.designated(Designated::Fmt)?;
        match self.application(class, expected?)?.1[..] {
            [_, schema] => Some(schema),
            _ => None,
        }
    }

    /// Refine only holes of a cached template, preserving its actual key set.
    fn contextual_fmt(&self, preview: TypeId, expected: Option<TypeId>) -> TypeId {
        let Some(expected) = self.fmt_schema(expected) else {
            return preview;
        };
        let Some(schema) = self.fmt_schema(Some(preview)) else {
            return preview;
        };
        let Type::Schema(items) = self.db.ty(schema) else {
            return preview;
        };
        let mut position = 0;
        let mut refined = Vec::new();
        for source in items.iter() {
            let key = match source.element {
                Element::Positional(_) => {
                    let key = self.db.intern(Type::Literal(Literal::Int(position)));
                    position += 1;
                    key
                }
                Element::Keyed { key, .. } => key,
                Element::Include(_) => return preview,
            };
            let value = match source.element {
                Element::Positional(value) | Element::Keyed { value, .. } => value,
                Element::Include(_) => unreachable!("not an item"),
            };
            let value = self
                .solver(&Gas::default())
                .schema_item(expected, key)
                .unwrap_or(value);
            let element = match source.element {
                Element::Positional(_) => Element::Positional(value),
                _ => Element::Keyed { key, value },
            };
            refined.push(item(source.multiplicity, element));
        }
        let schema = self.db.intern(Type::Schema(refined.into()));
        let Type::Apply { args, .. } = self.db.ty(preview) else {
            return preview;
        };
        let Argument::Positional(values) = args[0] else {
            return preview;
        };
        self.fmt_application(Designated::Fmt, vec![values, schema])
    }

    /// A template retains its values and the distinct names its consumer binds.
    pub(super) fn fmt_seq(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        parts: &[Expr],
        expected: Option<TypeId>,
    ) -> TypeId {
        let mut holes = FxHashMap::default();
        let mut positional = FxHashSet::default();
        for part in parts {
            if let ExprKind::FmtParam {
                name, name_span, ..
            } = &part.kind
            {
                let key = self.db.intern(Type::Literal(name.clone()));
                holes.entry(key).or_insert((*name_span, name.clone()));
                if let Literal::Int(index) = name {
                    positional.insert(*index);
                }
            }
        }
        let prefix = (0..).take_while(|index| positional.contains(index)).count();
        let mut items = Vec::new();
        let mut keyed = Vec::new();
        // Any value fills a hole, dynamically outside a strict unit
        let value = match self.strict() {
            true => self.db.top(),
            false => self.db.unknown(),
        };
        // Keep the schema's keyed items and gap diagnostics in TypeId order.
        let mut holes: Vec<_> = holes.into_iter().collect();
        holes.sort_unstable_by_key(|(key, _)| *key);
        for (key, (span, name)) in holes {
            let element = match name {
                Literal::Int(index) if index < prefix as i128 => {
                    items.push((
                        index,
                        item(Multiplicity::Required, Element::Positional(value)),
                    ));
                    continue;
                }
                Literal::Int(index) => {
                    if self.observing() {
                        self.problem(Problem::FmtGap {
                            span,
                            index,
                            missing: prefix,
                        });
                    }
                    Element::Keyed { key, value }
                }
                _ => Element::Keyed { key, value },
            };
            keyed.push(item(Multiplicity::Required, element));
        }
        items.sort_by_key(|(index, _)| *index);
        let mut schema: Vec<_> = items.into_iter().map(|(_, item)| item).collect();
        schema.extend(keyed);
        let schema = self.db.intern(Type::Schema(schema.into()));
        let bottom = self.db.bottom();
        let mut values = bottom;
        let mut never = false;
        for part in parts {
            let ty = self.eval(at, state, operands, part);
            never |= ty == bottom;
            if let ExprKind::FmtValue { .. } = part.kind {
                let value = self
                    .designated(Designated::FmtValue)
                    .and_then(|class| self.applied(class, ty).map(|(_, value)| value))
                    .unwrap_or(self.db.unknown());
                values = self.solver(&Gas::default()).lub(values, value);
            }
        }
        if never {
            return bottom;
        }
        self.contextual_fmt(
            self.fmt_application(Designated::Fmt, vec![values, schema]),
            expected,
        )
    }

    /// A pattern's items as the walk sees them, with each constant key evaluated
    pub(super) fn pattern_shape(
        &mut self,
        at: At,
        state: &mut State,
        items: &[PatternItem],
    ) -> PatternShape {
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
        pattern
    }

    /// The types a pattern's items unpack from a value, diagnosed at `span`,
    /// with the columns correlating them (see [`Group`]); `None` if the pattern
    /// can't match it. What its `(unpack)` gives must be
    /// an `Unpack[S]`. In a strict unit, an `irrefutable` pattern must match
    /// every value `S` admits.
    /// The pattern is walked against `S` (see [`Solver::unpack_pattern`]), giving
    /// a rest `Unpack[tail]`. Where `S` can't be found, every item is optional and
    /// the pattern admits any others, giving a const key or a rest `Unknown`.
    ///
    /// Each alternative of a union is sent `(unpack)` on its own, and walked on
    /// its own for a column, so the columns don't depend on how the solver joins
    /// what they give.
    pub(super) fn unpack(
        &mut self,
        at: At,
        state: &mut State,
        items: &[PatternItem],
        value: TypeId,
        span: Span,
        irrefutable: bool,
    ) -> Option<(Vec<TypeId>, Group)> {
        let unknown = self.db.unknown();
        let bottom = self.db.bottom();
        let original = value;
        if value == bottom {
            return Some((vec![value; items.len()], Group::default()));
        }
        let Some(unpack) = self.designated(Designated::Unpack) else {
            return Some((vec![unknown; items.len()], Group::default()));
        };
        // What's unpacked is what the value's `(unpack)` gives
        let each = self.send_special_each(at, state, value, "unpack", span);
        let value = match &each {
            Some(each) => (each.iter()).fold(bottom, |joined, &(_, given)| self.lub(joined, given)),
            None => self.send_special(at, state, value, "unpack", span),
        };
        if value == bottom {
            // Each alternative's column is impossible until its `(unpack)` gives
            // something, so the links made here survive joining a later pass's
            let group = Group {
                columns: match &each {
                    Some(each) => vec![vec![bottom; items.len() + 1].into(); each.len()],
                    None => Vec::new(),
                },
            };
            return Some((vec![value; items.len()], group));
        }
        let check = Check::Fits(span, Misfit::Unpackable);
        let pattern = self.pattern_shape(at, state, items);
        if let Some(unpacked) = self
            .solver(&Gas::default())
            .unpack_pattern(value, unpack, &pattern)
        {
            self.conclude(at, span, None, |_, rule| {
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
            if irrefutable
                && self.strict()
                && self.observing()
                && let Some(rejection) = unpacked.rejection
            {
                let schema = self.shown(rejection.schema);
                let items = |count| match count {
                    0 => "no items".to_owned(),
                    1 => "1 item".to_owned(),
                    _ => format!("{count} items"),
                };
                let (missing, example) = match rejection.reason {
                    Reason::Fewer(count) => (true, format!("may have {}", items(count))),
                    Reason::More(count) => (false, format!("may have {}", items(count))),
                    Reason::Lacks(key) => (
                        true,
                        format!("may have no item keyed `{}`", self.shown(key)),
                    ),
                    Reason::Extra(key) => (
                        false,
                        format!("may have an item keyed `{}`", self.shown(key)),
                    ),
                };
                let whole = match self.db.ty(original) {
                    Type::Union(_) => "a member",
                    _ => "it",
                };
                let note = format!("{whole} unpacks as `{schema}`, which {example}");
                let found = self.subject(original);
                self.problem(Problem::Unguaranteed {
                    span,
                    found,
                    missing,
                    note,
                });
            }
            let types = item_order(items, &pattern, &unpacked);
            let group = match each {
                Some(each) => self.columns(items, &pattern, unpack, &each),
                None => Group::default(),
            };
            return Some((types, group));
        }
        self.gap(span, "a pattern against a schema that can't be found");
        let types = self.conclude(at, span, None, |_, rule| {
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
        Some((types, Group::default()))
    }

    /// The columns of a pattern's items walked against what each alternative's
    /// `(unpack)` gave, each ending with the alternative itself. An alternative
    /// it can't match has a column of bottom. None when an alternative's schema
    /// can't be found or is dynamic, or fewer than two can match.
    fn columns(
        &self,
        items: &[PatternItem],
        pattern: &PatternShape,
        unpack: DeclId,
        each: &[(TypeId, TypeId)],
    ) -> Group {
        let (unknown, bottom) = (self.db.unknown(), self.db.bottom());
        let gas = Gas::default();
        let solver = self.solver(&gas);
        let mut columns = Vec::new();
        for &(alternative, given) in each {
            let unpacked = match given == bottom {
                true => None,
                false => match solver.unpack_pattern(given, unpack, pattern) {
                    Some(unpacked) => unpacked.possible.then_some(unpacked),
                    None => return Group::default(),
                },
            };
            let mut column = match &unpacked {
                Some(unpacked) => item_order(items, pattern, unpacked),
                None => vec![bottom; items.len()],
            };
            if column.contains(&unknown) {
                return Group::default();
            }
            column.push(match unpacked {
                Some(_) => alternative,
                None => bottom,
            });
            columns.push(column.into_boxed_slice());
        }
        let live = (columns.iter())
            .filter(|column| column.iter().any(|&ty| ty != bottom))
            .count();
        match live {
            0 | 1 => Group::default(),
            _ => Group { columns },
        }
    }
}

/// An unpacked pattern's types, in the order of its items
fn item_order(items: &[PatternItem], pattern: &PatternShape, unpacked: &Unpacked) -> Vec<TypeId> {
    let (positional, keyed) = unpacked.slots.split_at(pattern.positional.len());
    let (mut positional, mut keyed) = (positional.iter(), keyed.iter());
    let mut rests = unpacked.rests.iter();
    (items.iter())
        .map(|item| {
            let slot = match item.key {
                PatternKey::Pos => positional.next(),
                PatternKey::Key(_) | PatternKey::ConstKey(_) => keyed.next(),
                PatternKey::Rest(_) => rests.next(),
            };
            *slot.expect("a type for each item")
        })
        .collect()
}

/// Constrain a spread value to spread as `schema`
fn spread_into(
    rule: &mut Rule<'_, '_, '_>,
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

/// The derivation steps along a path, leaving out any not recorded
fn steps(path: &[Node]) -> Vec<Derivation> {
    (path.iter().skip(1))
        .filter_map(|node| node.step.clone())
        .collect()
}

/// Add a problem to a call's, as a note on the one already reported for its
/// argument, if any
fn merge(problems: &mut Vec<Problem>, problem: Problem) {
    if let Problem::Argument { span, causes, .. } = &problem
        && let Some(Problem::Argument { causes: notes, .. }) = (problems.iter_mut())
            .find(|other| matches!(other, Problem::Argument { span: at, .. } if at == span))
    {
        for cause in causes {
            if !notes.contains(cause) {
                notes.push(cause.clone());
            }
        }
    } else if !problems.contains(&problem) {
        problems.push(problem);
    }
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
/// to be bottom here, as a defaulting run takes it, so that a callee's binder it
/// also bounds, as `T` in `fold[T] init@T f@((T, T) -> T)`, is solved from the
/// other bounds.
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
/// without one that its upper bounds admit (see [`Solver::fall_back`]), to
/// the meet of its upper bounds (see [`Solver::default_upper`]),
/// until nothing more can be defaulted. A round that can default no bare
/// variable these ways defaults them all to the dynamic type of their kind, or
/// for a strict unit's solver, which doesn't make up `Unknown`, leaves them
/// unresolved. Literals decay as [`default_where`] decays them.
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
        // An upper bound may wait on another bare variable's default, so one
        // whose bounds don't give a default yet waits a round for it
        let count = bare.len();
        let mut deferred = Vec::new();
        for id in bare {
            if !solver.fall_back(id) && solver.default_upper(id).is_err() {
                deferred.push(id);
            }
        }
        if deferred.len() == count {
            if !solver.is_gradual() {
                break;
            }
            for id in deferred {
                let unknown = solver.closed(db.unknown_of(solver.variable_kind(id)));
                solver.constrain(unknown, Term::Infer(id), Provenance::default());
            }
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
    default_rounds(solver, |_, id| keep(id), roots, false)
}

/// Solve, defaulting a fork's own variables as [`default_where`] defaults
/// variables. A round that can't default any breaks a cycle of variables that
/// bound each other (see [`Solver::default_open`]): a fork's own, or one with
/// what it's nested in, which a rule of its own wouldn't have made.
fn default_own(solver: &mut Solver<'_>, roots: Option<&[(Term, Variance)]>) -> Vec<Outcome> {
    default_rounds(solver, |solver, id| solver.owns(id), roots, true)
}

fn default_rounds(
    solver: &mut Solver<'_>,
    keep: impl Fn(&Solver<'_>, InferVarId) -> bool,
    roots: Option<&[(Term, Variance)]>,
    open: bool,
) -> Vec<Outcome> {
    let mut outcomes = solver.solve();
    // Each round solves at least one variable; the limit only guards the solver
    for _ in 0..64 {
        let unsolved: Vec<_> = (solver.unresolved())
            .filter(|&id| keep(solver, id))
            .collect();
        // Defaults add bounds, so what's locked is found again each round
        let locked = roots.map(|roots| solver.locked(roots));
        let decays = |id| match &locked {
            Some(Ok(locked)) => locked.contains(&id),
            Some(Err(_)) | None => true,
        };
        let mut progress = false;
        for &id in &unsolved {
            progress |= solver.default_with(id, decays(id)).is_ok();
        }
        // One variable breaks the cycle
        if !progress && open {
            progress = (unsolved.iter()).any(|&id| solver.default_open(id, decays(id)).is_ok());
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
fn received(values: &mut Values, receivers: &[(TypeId, Span)], bottom: TypeId) {
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

/// The constraint of a call: its callee called with its arguments (see
/// [`Solver::constrain_call`]). Returns the variable standing for its result. An overloaded
/// callee's overloads are tried against what the arguments alone say, with each
/// `do` block's result hidden as it is from other alternatives (see
/// [`Solver::blind`]), and the result `Value`.
fn call_constraint<'p>(
    rule: &mut Rule<'_, '_, 'p>,
    values: &'p Values,
    spread: Option<DeclId>,
    (span, name, logs): (Span, Option<String>, bool),
    callee: TypeId,
    input: Option<TypeId>,
    output: Option<TypeId>,
) -> Term {
    let (arguments, spans) = rule.arguments(values, spread, span);
    let result = rule.solver.infer_result();
    let (input, output) = (
        input.map(|ty| rule.closed(ty)),
        output.map(|ty| rule.closed(ty)),
    );
    let blinded: Vec<(Multiplicity, CallArgument)> = (arguments.iter())
        .map(|&(multiplicity, argument)| {
            let twin = |term| rule.solver.twin(term);
            let argument = match argument {
                CallArgument::Positional(term) => CallArgument::Positional(twin(term)),
                CallArgument::Keyword(key, term) => CallArgument::Keyword(key, twin(term)),
                CallArgument::Pair(key, term) => CallArgument::Pair(key, twin(term)),
                CallArgument::Spread(term) => CallArgument::Spread(term),
            };
            (multiplicity, argument)
        })
        .collect();
    let blinded = rule.solver.arguments_schema(&blinded);
    let arguments = rule.solver.arguments_schema(&arguments);
    rule.solver.blind(arguments, blinded);
    let call = SolverCall {
        callee: rule.closed(callee),
        arguments,
        result,
        input,
        output,
    };
    let check = Check::Call {
        span,
        args: spans,
        callee: name,
    };
    let constraint = rule.solver.constrain_call(call, Provenance::default());
    rule.checks.push(check);
    if rule.logging && logs {
        let obligation = rule.solver.obligation(constraint);
        rule.logged.push(Pending::Callee(obligation, span));
    }
    result
}

/// The designated class a collection literal is
fn collection_role(kind: Collection) -> Designated {
    match kind {
        Collection::Array => Designated::Array,
        Collection::Dict => Designated::Dict,
        Collection::Tuple => Designated::Intrinsic(Intrinsic::Tuple),
        Collection::Record => Designated::Record,
    }
}

/// The constraints of a collection of `class` with `values`, as `shape` says to
/// build it. Returns the term standing for its type.
fn collection_constraint<'p>(
    rule: &mut Rule<'_, '_, 'p>,
    values: &'p Values,
    class: DeclId,
    shape: Shape,
    spread: Option<DeclId>,
    span: Span,
) -> Term {
    let unknown = rule.db.unknown();
    match shape {
        Shape::Array => {
            // An empty array's element is the least that what it's in allows
            let element = match values.values.is_empty() {
                true => rule.solver.infer_fresh(Kind::Type, Rest::All),
                false => rule.solver.infer(),
            };
            for placed in &values.values {
                match placed.value {
                    Value::Pos(ty, _) => rule.constrain(rule.closed(ty), element, Check::Quiet),
                    Value::Lambda(key, ref lambda, span) => {
                        let term = rule.lambda(lambda, span);
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
                    Value::Held(at, index, _) => {
                        let term = rule.hold(&values.held[index], placed.group.is_some());
                        let element = match at {
                            HeldAt::Positional => element,
                            HeldAt::Keyword(_) | HeldAt::Pair(_) => rule.closed(unknown),
                        };
                        rule.constrain(term, element, Check::Quiet);
                    }
                }
            }
            rule.term(|holes| {
                let element = holes.hole(element, Kind::Type);
                holes.apply(class, vec![element])
            })
        }
        // What's expected of a dict checks its items one by one, where joining
        // them would lose which is where
        Shape::Chosen(ty, schema) => {
            let (arguments, _) = rule.arguments(values, spread, span);
            let exact = rule.solver.arguments_schema(&arguments);
            rule.constrain(exact, schema, Check::Expected(span));
            ty
        }
        Shape::Empty => {
            let schema = rule.solver.infer_fresh(Kind::Schema, Rest::All);
            rule.constrain_empty(schema, Check::Empty(span));
            rule.term(|holes| {
                let schema = holes.hole(schema, Kind::Schema);
                holes.apply(class, vec![schema])
            })
        }
        // Positional items stay positional, as they were written: only an
        // explicit key is a key
        Shape::Dict(has_positional, has_keyed) => {
            let positional = rule.solver.infer();
            let keys = rule.solver.infer();
            let entries = rule.solver.infer();
            let entry = |rule: &mut Rule<'_, '_, '_>, key: TypeId, value: Term| {
                rule.constrain(rule.closed(key), keys, Check::Quiet);
                rule.constrain(value, entries, Check::Quiet);
            };
            let lanes = |holes: &mut Holes<'_>, has_positional: bool, has_keyed: bool| {
                let mut items = Vec::new();
                if has_positional {
                    let element = holes.hole(positional, Kind::Type);
                    items.push(item(Multiplicity::Repeated, Element::Positional(element)));
                }
                if has_keyed {
                    let key = holes.hole(keys, Kind::Type);
                    let value = holes.hole(entries, Kind::Type);
                    items.push(item(Multiplicity::Repeated, Element::Keyed { key, value }));
                }
                holes.schema(items)
            };
            // A key written in a dict literal is a term, which decays as its value would
            let symbol = |name| rule.db.intern(Type::Fresh(Literal::Sym(name)));
            for placed in &values.values {
                match placed.value {
                    Value::Pos(ty, _) => rule.constrain(rule.closed(ty), positional, Check::Quiet),
                    Value::Key(name, ty, _) => entry(rule, symbol(name), rule.closed(ty)),
                    Value::Pair(key_type, ty) => entry(rule, key_type, rule.closed(ty)),
                    Value::Lambda(key, ref lambda, span) => {
                        let term = rule.lambda(lambda, span);
                        match key {
                            Some(key) => entry(rule, symbol(key), term),
                            None => rule.constrain(term, positional, Check::Quiet),
                        }
                    }
                    Value::Spread(ty, span) => {
                        let schema = rule.term(|holes| lanes(holes, true, true));
                        spread_into(rule, spread, ty, schema, span);
                    }
                    Value::Held(at, index, _) => {
                        let term = rule.hold(&values.held[index], placed.group.is_some());
                        match at {
                            HeldAt::Positional => rule.constrain(term, positional, Check::Quiet),
                            HeldAt::Keyword(key) => entry(rule, symbol(key), term),
                            HeldAt::Pair(key) => entry(rule, key, term),
                        }
                    }
                }
            }
            rule.term(|holes| {
                let schema = lanes(holes, has_positional, has_keyed);
                holes.apply(class, vec![schema])
            })
        }
        Shape::Items(lanes) => {
            let mut elements = Vec::new();
            for placed in &values.values {
                match placed.value {
                    Value::Lambda(key, ref lambda, span) => {
                        elements.push((key, rule.lambda(lambda, span), Kind::Type));
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
                    Value::Held(at, index, _) => {
                        let term = rule.hold(&values.held[index], placed.group.is_some());
                        let var = rule.solver.infer();
                        rule.constrain(term, var, Check::Quiet);
                        let key = match at {
                            HeldAt::Positional => None,
                            HeldAt::Keyword(key) => Some(key),
                            HeldAt::Pair(_) => unreachable!("a tuple or record has no pairs"),
                        };
                        elements.push((key, var, Kind::Type));
                    }
                    Value::Pair(..) => unreachable!("a tuple or record has no pairs"),
                }
            }
            rule.term(|holes| {
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
            })
        }
    }
}

/// `class` applied to a fresh variable of `kind`: the variable, and the
/// application
fn applied_term(solver: &mut Solver<'_>, db: &Database, class: DeclId, kind: Kind) -> (Term, Term) {
    let argument = solver.infer_kind(kind, Rest::All);
    let mut holes = Holes {
        db,
        group: Vec::new(),
    };
    let hole = holes.hole(argument, kind);
    let ty = holes.apply(class, vec![hole]);
    let environment = solver.environment(solver.empty_environment(), holes.group);
    (argument, solver.view(ty, environment))
}

/// What a variable standing for a value, as a held argument, is expected to be
/// so far: its upper bounds, seen through the variables it's below, as the
/// variable of a `for`'s joined items stands between them. That's the one open
/// in what isn't solved yet, as the binders of a call its parameter names, or
/// else the solved one `below` shows is below the others, as a binder's bound
/// is above what's expected of it. Without another, it's the variable that
/// ends the walk. `None` if the bounds don't agree.
fn expectation(
    solver: &Solver<'_>,
    held: Term,
    below: impl Fn(TypeId, TypeId) -> bool,
) -> Option<Term> {
    let mut open: Option<Term> = None;
    let mut closed: Vec<TypeId> = Vec::new();
    let mut last = None;
    let mut pending = vec![held];
    let mut seen = FxHashSet::default();
    while let Some(term) = pending.pop() {
        let Term::Infer(id) = term else {
            unreachable!("a variable stands for a value")
        };
        if !seen.insert(id) {
            continue;
        }
        let mut bounded = false;
        for upper in solver.bounds(id).upper() {
            bounded = true;
            let upper = solver.resolve(upper).unwrap_or(upper);
            match (upper, solver.reify(upper)) {
                (_, Ok(ty)) if !closed.contains(&ty) => closed.push(ty),
                (_, Ok(_)) => {}
                (Term::Infer(_), Err(_)) => pending.push(upper),
                (_, Err(_)) => match open {
                    Some(other) if other != upper => return None,
                    _ => open = Some(upper),
                },
            }
        }
        if !bounded {
            last = Some(term);
        }
    }
    match (open, &closed[..]) {
        (Some(open), []) => Some(open),
        (Some(_), _) => None,
        (None, []) => last,
        (None, closed) => (closed.iter().copied())
            .find(|&ty| closed.iter().all(|&other| other == ty || below(ty, other)))
            .map(|ty| solver.closed(ty)),
    }
}
