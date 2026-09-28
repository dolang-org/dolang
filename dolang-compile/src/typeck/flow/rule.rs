//! Checking rules. Each run of a rule solves its constraints with a fresh solver,
//! and only reified types leave it.
//!
//! A rule contributes its results only once it's decided: solved without
//! contradiction, with every result solved without defaulting. Until then it
//! contributes bottom, which adds nothing and never has to be retracted. Once the
//! queue empties, every rule still undecided is frozen and its block queued again.
//! From then on each of its runs defaults its unsolved variables, upstream first,
//! and a variable with no lower bounds becomes dynamic. A rule's results in a
//! context are joined over its runs.
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
        cfg::{
            BlockId, Collection, Expr, ExprKind, FuncId, Item, Pattern, PatternItem, PatternKey,
            RuleId, VarId,
        },
        elab::Designated,
        solver::{
            CallArgument, Contradiction, Issue, ObligationId, Outcome, Provenance, Residual,
            Solver, Status, Step as Derivation, Term,
        },
        r#type::{
            Argument, BoundRef, Database, DeclId, Element, Function, Kind, Literal, Multiplicity,
            Rest, SchemaItem, SymbolId, Type, TypeId, UnionMember,
        },
    },
};

/// Where a rule is
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Site {
    /// An expression's rule
    Rule(RuleId),
    /// A pattern that unpacks a value: a step's, by index, or else its block's
    /// terminal's
    Pattern(BlockId, Option<usize>),
    /// A `for`'s next item
    Next(BlockId),
}

/// What a rule concluded in one context
pub(super) struct Conclusion {
    /// Joined over every run
    results: Vec<TypeId>,
    /// Whether its latest run was decided
    pub(super) decided: bool,
    pub(super) frozen: bool,
    /// The block it runs in
    pub(super) block: BlockId,
}

/// What a constraint of a rule checks, to diagnose it by
#[derive(Clone)]
enum Check {
    /// A callee against the call, by the call's span and each argument's
    Call { span: Span, args: Vec<Span> },
    /// A value that must be something
    Fits(Span, Misfit),
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
    /// A `do` block passed to a call, positionally or by key
    Lambda(Option<SymbolId>, Lambda, Span),
}

/// A `do` block passed to a call, which types it: its function type, with a hole
/// at depth 0 for each item its signature leaves to the call
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

/// The evaluated items of an argument list or collection
#[derive(Default)]
struct Values {
    values: Vec<Value>,
    /// Whether a value isn't produced, so neither is the whole
    never: bool,
    /// Whether it has a comprehension
    comprehension: bool,
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
    /// this context, joined with what its earlier runs concluded. `expected` is
    /// pre-seeded as an upper bound on the first result, unless that contradicts:
    /// then the check against the expectation reports it instead.
    fn conclude(
        &mut self,
        at: At,
        site: Site,
        expected: Option<TypeId>,
        build: impl Fn(&mut Rule<'_, 'a>) -> Vec<Term>,
    ) -> Vec<TypeId> {
        let key = (site, at.ctx);
        let frozen = self.rules.get(&key).is_some_and(|rule| rule.frozen);
        let run = |seed: bool| {
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
            let outcomes = if frozen {
                default_all(&mut solver, self.db, true)
            } else {
                solver.solve()
            };
            let rejected = seeded && outcomes[checks.len()].status == Status::Contradicted;
            (solver, results, checks, lambdas, outcomes, rejected)
        };
        let (mut solver, mut results, mut checks, mut lambdas, mut outcomes, rejected) = run(true);
        if rejected {
            (solver, results, checks, lambdas, outcomes, _) = run(false);
        }
        let contradicted =
            (outcomes[..checks.len()].iter()).any(|outcome| outcome.status == Status::Contradicted);
        let reified: Option<Vec<TypeId>> = (results.iter())
            .map(|&term| solver.reify(term).ok())
            .collect();
        let decided = frozen || (!contradicted && reified.is_some());
        let values: Vec<TypeId> = match reified {
            Some(values) if decided => values,
            _ if frozen => (results.iter())
                .map(|&term| solver.reify(term).unwrap_or(self.db.unknown()))
                .collect(),
            _ => vec![self.db.bottom(); results.len()],
        };
        if self.observing() {
            self.blame(&solver, &checks, &outcomes[..checks.len()]);
            return self
                .rules
                .get(&key)
                .map_or(values, |rule| rule.results.clone());
        }
        let (passed, pending) = lambdas;
        for (var, ty) in self::passed(&mut solver, self.db, &passed, &pending) {
            self.join(var, ty);
        }
        let joined = match self.rules.get(&key) {
            Some(rule) => (rule.results.iter().zip(&values))
                .map(|(&old, &new)| self.lub(old, new))
                .collect(),
            None => values,
        };
        let rule = self.rules.entry(key).or_insert(Conclusion {
            results: Vec::new(),
            decided,
            frozen: false,
            block: at.block,
        });
        rule.results.clone_from(&joined);
        rule.decided = decided;
        joined
    }

    /// Diagnose the checks a rule's final run contradicted, and record the ones it
    /// couldn't decide
    fn blame(&mut self, solver: &Solver<'_>, checks: &[Check], outcomes: &[Outcome]) {
        for (check, outcome) in checks.iter().zip(outcomes) {
            let span = match *check {
                Check::Call { span, .. } | Check::Fits(span, _) => span,
                Check::Quiet => continue,
            };
            match outcome.status {
                Status::Proven => {}
                Status::Unresolved => {
                    let residual = (outcome.diagnostics.iter())
                        .find_map(|diagnostic| match diagnostic.issue {
                            Issue::Residual(residual) => Some(residual),
                            Issue::Contradiction(_) => None,
                        })
                        .unwrap_or(Residual::Unsupported);
                    self.undecided(span, residual);
                }
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
    /// analysis gives it, is a variable that keeps the call undecided. A callee
    /// that isn't a function or a union of them, or whose arguments aren't
    /// followed yet, gives the dynamic type.
    pub(super) fn call(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Call { callee, args, rule } = &expr.kind else {
            unreachable!("a call")
        };
        let bottom = self.db.bottom();
        let unknown = self.db.unknown();
        let callee_type = self.eval(at, state, operands, callee);
        let params = self.params(callee_type).unwrap_or_default();
        let values = self.values(at, state, operands, args, Some(&params));
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
        if !callable
            || values.comprehension
            || (values.values.iter()).any(|value| matches!(value, Value::Pair(..)))
        {
            // Nothing is expected of a `do` block the call doesn't type
            for value in &values.values {
                if let Value::Lambda(_, lambda, _) = value {
                    self.lambda(at, lambda.func);
                }
            }
            return unknown;
        }
        let (input, output) = self.channels(at.func);
        let spread = self.designated(Designated::Spread);
        let span = expr.span;
        self.conclude(at, Site::Rule(*rule), expected, |rule| {
            // A spread's schema is the least its value spreads as, solved before the
            // call, which can't show an unsolved schema supplies its parameters
            let spreads: Vec<Term> = (values.values.iter())
                .filter_map(|value| match *value {
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
                solved.push(
                    rule.solver
                        .reify(schema)
                        .map_or(schema, |ty| rule.closed(ty)),
                );
            }
            let mut spreads = solved.into_iter();
            let mut arguments = Vec::new();
            let mut spans = Vec::new();
            for value in &values.values {
                let (argument, span) = match *value {
                    Value::Pos(ty, span) => (CallArgument::Positional(rule.closed(ty)), span),
                    Value::Key(key, ty, span) => {
                        (CallArgument::Keyword(key, rule.closed(ty)), span)
                    }
                    Value::Spread(_, span) => {
                        let schema = spreads.next().expect("a schema for each spread");
                        (CallArgument::Spread(schema), span)
                    }
                    Value::Lambda(key, ref lambda, span) => {
                        let term = rule.lambda(lambda);
                        match key {
                            Some(key) => (CallArgument::Keyword(key, term), span),
                            None => (CallArgument::Positional(term), span),
                        }
                    }
                    Value::Pair(..) => unreachable!("a call has no pairs"),
                };
                arguments.push(argument);
                spans.push(span);
            }
            let result = rule.solver.infer();
            let call = rule.solver.call(
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
    fn params(&self, callee: TypeId) -> Option<Params> {
        let mut ty = callee;
        if let Type::Quantified { body, .. } = self.db.ty(ty) {
            ty = *body;
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
    /// expecting what `params` gives its position. A call's are given `params`,
    /// and a `do` block among them is left for the call to type.
    fn values(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &[Item],
        params: Option<&Params>,
    ) -> Values {
        let mut values = Values::default();
        let mut position = Some(0);
        self.gather(
            at,
            state,
            operands,
            items,
            params,
            &mut position,
            false,
            &mut values,
        );
        values
    }

    #[expect(clippy::too_many_arguments, reason = "one recursion's state")]
    fn gather(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        items: &[Item],
        params: Option<&Params>,
        position: &mut Option<usize>,
        comprehension: bool,
        values: &mut Values,
    ) {
        let bottom = self.db.bottom();
        let produced = |values: &mut Values, ty: TypeId| {
            let never = ty == bottom;
            values.never |= never && !comprehension;
            !never
        };
        for item in items {
            // A call types a `do` block it's given
            let lambda = match item {
                Item::Pos(value) | Item::Key(_, value) if params.is_some() => match value.kind {
                    ExprKind::Lambda(func) => self.contextual(at, func),
                    _ => None,
                },
                _ => None,
            };
            if let Some(lambda) = lambda {
                let (key, value) = match item {
                    Item::Pos(value) => {
                        *position = position.map(|index| index + 1);
                        (None, value)
                    }
                    &Item::Key(key, ref value) => (Some(key), value),
                    _ => unreachable!("a positional or keyed item"),
                };
                values.values.push(Value::Lambda(key, lambda, value.span));
                continue;
            }
            match item {
                Item::Pos(value) => {
                    let expected = params.zip(*position).and_then(|(params, index)| {
                        match params.positional.get(index) {
                            Some(&ty) => ty,
                            None => params.rest,
                        }
                    });
                    *position = position.map(|index| index + 1);
                    let ty = self.expect(at, state, operands, value, expected);
                    if produced(values, ty) {
                        values.values.push(Value::Pos(ty, value.span));
                    }
                }
                &Item::Key(key, ref value) => {
                    let expected = params.and_then(|params| {
                        (params.keyed.iter()).find_map(|&(name, ty)| (name == key).then_some(ty))
                    });
                    let ty = self.expect(at, state, operands, value, expected);
                    if produced(values, ty) {
                        values.values.push(Value::Key(key, ty, value.span));
                    }
                }
                Item::Pair(key, value) => {
                    let key_type = self.eval(at, state, operands, key);
                    let ty = self.eval(at, state, operands, value);
                    if produced(values, key_type) && produced(values, ty) {
                        values.values.push(Value::Pair(key_type, ty));
                    }
                }
                Item::Spread(value) => {
                    *position = None;
                    let ty = self.eval(at, state, operands, value);
                    if produced(values, ty) {
                        values.values.push(Value::Spread(ty, value.span));
                    }
                }
                Item::For(items) => {
                    values.comprehension = true;
                    *position = None;
                    self.gather(at, state, operands, items, None, position, true, values);
                }
                Item::If { then, else_ } => {
                    values.comprehension = true;
                    *position = None;
                    for items in [then, else_] {
                        self.gather(at, state, operands, items, None, position, true, values);
                    }
                }
            }
        }
    }

    /// A collection literal of a designated class. An array or dict joins its items
    /// into its element types, whether a comprehension repeats them or not. A tuple
    /// or record takes its items' schema, so a comprehension in one gives the
    /// dynamic type until its schema is built from the comprehension's structure.
    pub(super) fn collection(
        &mut self,
        at: At,
        state: &mut State,
        operands: &mut VecDeque<TypeId>,
        expr: &Expr,
        expected: Option<TypeId>,
    ) -> TypeId {
        let ExprKind::Collection { kind, items, rule } = &expr.kind else {
            unreachable!("a collection")
        };
        let unknown = self.db.unknown();
        let values = self.values(at, state, operands, items, None);
        if values.never {
            return self.db.bottom();
        }
        let role = match kind {
            Collection::Array => Designated::Array,
            Collection::Dict => Designated::Dict,
            Collection::Tuple => Designated::Tuple,
            Collection::Record => Designated::Record,
        };
        let Some(class) = self.designated(role) else {
            return unknown;
        };
        if values.comprehension && matches!(kind, Collection::Tuple | Collection::Record) {
            return unknown;
        }
        let spread = self.designated(Designated::Spread);
        let int = self.intrinsic(crate::typeck::r#type::Intrinsic::Int);
        let site = Site::Rule(*rule);
        let result = match kind {
            Collection::Array => self.conclude(at, site, expected, |rule| {
                let element = rule.solver.infer();
                for value in &values.values {
                    match *value {
                        Value::Pos(ty, _) => rule.constrain(rule.closed(ty), element, Check::Quiet),
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
                        Value::Lambda(..) => unreachable!("only a call types a `do` block"),
                    }
                }
                vec![rule.term(|holes| {
                    let element = holes.hole(element, Kind::Type);
                    holes.apply(class, vec![element])
                })]
            }),
            Collection::Dict => self.conclude(at, site, expected, |rule| {
                let keys = rule.solver.infer();
                let entries = rule.solver.infer();
                let entry = |rule: &mut Rule<'_, 'a>, key: Term, ty: TypeId| {
                    rule.constrain(key, keys, Check::Quiet);
                    rule.constrain(rule.closed(ty), entries, Check::Quiet);
                };
                for value in &values.values {
                    match *value {
                        Value::Pos(ty, _) => entry(rule, rule.closed(int), ty),
                        Value::Key(name, ty, _) => {
                            let name = rule.db.intern(Type::Literal(Literal::Sym(name)));
                            entry(rule, rule.closed(name), ty);
                        }
                        Value::Pair(key_type, ty) => entry(rule, rule.closed(key_type), ty),
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
                        Value::Lambda(..) => unreachable!("only a call types a `do` block"),
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
            Collection::Tuple | Collection::Record => {
                let lanes = match kind {
                    Collection::Tuple => Rest::Positional,
                    _ => Rest::All,
                };
                self.conclude(at, site, expected, |rule| {
                    let mut elements = Vec::new();
                    for value in &values.values {
                        match *value {
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
                            Value::Pair(..) | Value::Lambda(..) => {
                                unreachable!("a tuple or record has no pairs, and only a call types a `do` block")
                            }
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

    /// Values that must each fit a type, giving a known result. Without the type,
    /// a value is unchecked.
    fn fits(
        &mut self,
        at: At,
        rule: RuleId,
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
        self.conclude(at, Site::Rule(rule), None, |rule| {
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
        rule: RuleId,
    ) -> TypeId {
        let values: Vec<_> = (parts.iter())
            .map(|part| (self.eval(at, state, operands, part), part.span))
            .collect();
        let bin = self.designated(Designated::Bin);
        let result = self.designated_type(Designated::Bin);
        let bin = bin.map(|decl| self.db.intern(Type::Decl(decl)));
        self.fits(at, rule, &values, bin, Misfit::Binary, result)
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
        let (value, spec, rule, role) = match &expr.kind {
            ExprKind::FmtValue { value, spec, rule } => {
                (Some(&**value), spec, *rule, Designated::FmtValue)
            }
            ExprKind::FmtParam { spec, rule } => (None, spec, *rule, Designated::FmtParam),
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
        self.fits(at, rule, &values, int, Misfit::Int, result)
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
        self.conclude(at, Site::Next(at.block), None, |rule| {
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
        site: Site,
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
        self.conclude(at, site, None, |rule| {
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

/// What a call passes the `do` blocks it's given: each variable's solution, once
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
