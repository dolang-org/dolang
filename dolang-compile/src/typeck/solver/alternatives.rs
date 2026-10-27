//! Judgments that need one of their alternatives to hold: a union on the right
//! whose terms aren't all closed, where a member must be chosen to infer through,
//! or a callable or overloaded function on the left or called, with several
//! signatures, one of which must fit.
//!
//! Alternatives are judged by trials, each on a fork: a solver nested in the
//! one judging. A trial adds the alternative's own judgment and solves, so
//! nothing it finds reaches the solver it was forked from. A trial whose judgment is contradicted rejects its
//! alternative; any other leaves it possible. One alternative left possible is
//! chosen, and the judgment derives it. A rejection holds under every later state,
//! since bounds only grow, so a choice doesn't depend on the order judgments are
//! made in. Several possible alternatives leave the judgment ambiguous until what
//! they relate grows; none contradicts it.
//!
//! A judgment may be tried against a selection in place of what it relates to,
//! as a call's overloads are tried against what its arguments alone say: its
//! choice is then related to the whole. When every alternative is rejected,
//! what each trial found is kept, to say why (see [`Solver::rejections`]).
//!
//! Trials run when solving is quiescent, before any scope is settled, so a fork
//! never holds a judgment half processed. They judge one obligation at a time, in
//! creation order, and solving resumes after each choice.

use super::*;

/// How deeply trials nest: a trial's fork judges its own alternatives, but one
/// nested this deep leaves them untried
const TRIAL_DEPTH: usize = 2;

/// What trials found of a judgment's alternatives
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Untried,
    Chosen(usize),
    /// Several remain possible
    Ambiguous,
    /// None is possible, or trials couldn't finish
    Failed(Issue),
}

/// What a trial that rejected an alternative found, resolved on its fork
#[derive(Clone)]
pub(crate) struct Rejection {
    /// The alternative's index
    pub(crate) index: usize,
    /// The alternative's checked side, as its trial solved it
    pub(crate) overload: Option<TypeId>,
    pub(crate) contradictions: alias::Box<[Contradicted]>,
}

/// A judgment that needs one of its alternatives to hold
#[derive(Clone)]
pub(super) struct Alternatives {
    /// Each alternative's own judgment
    judgments: Vec<Relation>,
    /// What trials judge in place of each alternative's judgment, if anything
    selections: Option<Vec<Relation>>,
    /// What the judgment is when no alternative is possible
    none: Issue,
    /// The generation its trials last ran at
    tried: Option<usize>,
    verdict: Verdict,
    /// The alternatives left possible, while several are
    possible: Vec<usize>,
    /// What trials found of every alternative, once they rejected each
    rejections: alias::Box<[Rejection]>,
}

impl<'db> Solver<'db> {
    /// Have trials judge `twin` in place of `term`, where `term` is the actual side
    /// of a judgment with alternatives. A `do` block's type whose result is known
    /// has a twin leaving the result to a variable, so that what the block gives
    /// doesn't choose: the choice is then checked with what it gives.
    pub(crate) fn blind(&mut self, term: Term, twin: Term) {
        self.blinded.insert(term, twin);
    }

    /// What trials judge in place of `term`: its twin, if it has one
    pub(crate) fn twin(&self, term: Term) -> Term {
        self.blinded(term).unwrap_or(term)
    }

    /// Relate `actual` to the alternative trials chose among `terms`, labeled by
    /// `step`. Until one is chosen, the judgment is residual.
    pub(super) fn choose(
        &self,
        obligation: ObligationId,
        actual: Term,
        terms: Vec<Term>,
        step: fn(usize) -> Step,
        none: Issue,
    ) -> Result<(), Issue> {
        let judgments = terms
            .into_iter()
            .map(|term| self.alternative(obligation, actual, term))
            .collect();
        self.choose_judgment(obligation, judgments, None, step, none)
    }

    /// Relate the alternative trials chose among `terms` to `expected`, labeled
    /// by `step`. Until one is chosen, the judgment is residual.
    pub(super) fn choose_left(
        &self,
        obligation: ObligationId,
        terms: Vec<Term>,
        expected: Term,
        step: fn(usize) -> Step,
        none: Issue,
    ) -> Result<(), Issue> {
        let judgments = terms
            .into_iter()
            .map(|term| self.alternative(obligation, term, expected))
            .collect();
        self.choose_judgment(obligation, judgments, None, step, none)
    }

    /// Relate the alternative trials chose among `terms` to `expected`, labeled
    /// by `step`, where trials relate them to `selection` instead
    pub(super) fn choose_selected(
        &self,
        obligation: ObligationId,
        terms: Vec<Term>,
        expected: Term,
        selection: Term,
        step: fn(usize) -> Step,
        none: Issue,
    ) -> Result<(), Issue> {
        let (judgments, selections) = terms
            .into_iter()
            .map(|term| {
                (
                    self.alternative(obligation, term, expected),
                    self.alternative(obligation, term, selection),
                )
            })
            .unzip();
        self.choose_judgment(obligation, judgments, Some(selections), step, none)
    }

    /// Relate the call trials chose among `calls` in place of `obligation`'s,
    /// labeled by `step`, where trials judge `selections` instead, if any
    pub(super) fn choose_call(
        &self,
        obligation: ObligationId,
        calls: Vec<Call>,
        selections: Option<Vec<Call>>,
        step: fn(usize) -> Step,
        none: Issue,
    ) -> Result<(), Issue> {
        let judgments = calls.into_iter().map(Relation::Call).collect();
        let selections = selections.map(|calls| calls.into_iter().map(Relation::Call).collect());
        self.choose_judgment(obligation, judgments, selections, step, none)
    }

    /// An alternative's judgment, `actual <: expected`, reading schemas as its
    /// parent's does
    fn alternative(&self, obligation: ObligationId, actual: Term, expected: Term) -> Relation {
        let fill = match self.relation(obligation) {
            Relation::Subtype { fill, .. } => fill,
            Relation::Call(_) | Relation::Empty(_) => Fill::Arguments,
        };
        Relation::Subtype {
            actual,
            expected,
            fill,
        }
    }

    /// What trials found of each alternative of a judgment none of whose
    /// alternatives is possible
    pub(crate) fn rejections(&self, obligation: ObligationId) -> alias::Box<[Rejection]> {
        self.alternative_record(obligation, |record| record.rejections.clone())
            .unwrap_or_default()
    }

    /// The alternatives left possible of a judgment that's ambiguous
    pub(crate) fn possible(&self, obligation: ObligationId) -> Vec<usize> {
        self.alternative_record(obligation, |record| {
            (record.verdict == Verdict::Ambiguous).then(|| record.possible.clone())
        })
        .flatten()
        .unwrap_or_default()
    }

    /// Derive the alternative judgment trials chose, labeled by `step`
    fn choose_judgment(
        &self,
        obligation: ObligationId,
        judgments: Vec<Relation>,
        selections: Option<Vec<Relation>>,
        step: fn(usize) -> Step,
        none: Issue,
    ) -> Result<(), Issue> {
        let verdict = self.alternative_record(obligation, |record| match record.verdict {
            Verdict::Chosen(index) => Ok((index, record.judgments[index])),
            Verdict::Untried => Err(Residual::Inference.into()),
            Verdict::Ambiguous => Err(Residual::Ambiguous.into()),
            Verdict::Failed(issue) => Err(issue),
        });
        let Some(verdict) = verdict else {
            self.add_alternatives(
                obligation,
                Alternatives {
                    judgments,
                    selections,
                    none,
                    tried: None,
                    verdict: Verdict::Untried,
                    possible: Vec::new(),
                    rejections: Default::default(),
                },
            );
            return Err(Residual::Inference.into());
        };
        let (index, judgment) = verdict?;
        self.derive_relation(obligation, judgment, step(index));
        Ok(())
    }

    /// What a union on the right is when trials reject every member: a
    /// contradiction for a literal, a class or a function, which have values
    /// outside a union none of whose members admits them. A protocol may be
    /// covered by several members' classes, so it is left residual. So is a
    /// class reaching several generic members, whose values may fit their
    /// applications together without fitting any one, and anything beside a
    /// member that is a projection. A class has values outside a member it
    /// doesn't reach, and one reaching a sole generic member has none another
    /// member collects. A
    /// literal's or function's class is fixed, so its rejections are final, and
    /// so are top's, which no member but top covers.
    pub(super) fn refuted(
        &self,
        actual: Term,
        view: TypeView,
        members: &[UnionMember],
    ) -> Result<Issue, Issue> {
        let residual = Residual::Unsupported("a type that may be inside a union member").into();
        let mut class = None;
        let fixed = match self.widened_head(actual) {
            Ok(Head::Structural(view)) => match self.db.ty(view.ty) {
                _ if view.ty == self.db.top() => true,
                Type::Literal(_)
                | Type::Function(_)
                | Type::Quantified { .. }
                | Type::Overloaded { .. } => true,
                _ => return Ok(residual),
            },
            Ok(Head::Nominal(nominal)) => {
                if self.db.declaration(nominal.declaration).source.kind != DeclKind::Class {
                    return Ok(residual);
                }
                class = Some(nominal);
                false
            }
            Ok(_) => return Ok(residual),
            Err(Issue::Residual(_)) => return Ok(residual),
            Err(issue) => return Err(issue),
        };
        let mut reached = 0;
        for member in members {
            let UnionMember::Type(ty) = *member else {
                return Ok(residual);
            };
            if fixed {
                continue;
            }
            match self.head(view.child(ty)) {
                Ok(Head::Nominal(nominal)) if !nominal.arguments.is_empty() => {
                    match class
                        .as_ref()
                        .and_then(|class| self.reaches(class, nominal.declaration))
                    {
                        Some(false) => {}
                        Some(true) => reached += 1,
                        None => return Ok(residual),
                    }
                    if reached > 1 {
                        return Ok(residual);
                    }
                }
                Ok(Head::Meta(nominal)) if !nominal.arguments.is_empty() => {
                    return Ok(residual);
                }
                Ok(Head::Infer(_)) | Err(Issue::Residual(_)) => return Ok(residual),
                Ok(_) => {}
                Err(issue) => return Err(issue),
            }
        }
        Ok(Issue::Contradiction(Contradiction::Outside))
    }

    /// Judge the alternatives of each judgment whose trials haven't seen the
    /// current generation, in creation order, stopping at the first choice. Each
    /// judgment whose verdict changes is reduced again. Whether any did.
    pub(super) fn try_alternatives(&self) -> bool {
        if self.trial_depth() >= TRIAL_DEPTH {
            return false;
        }
        let generation = self.generation.get();
        let mut pending: Vec<ObligationId> = self
            .alternative_ids(self.trials_from)
            .into_iter()
            .filter(|&id| {
                self.alternative_record(id, |record| {
                    matches!(record.verdict, Verdict::Untried | Verdict::Ambiguous)
                        && record.tried != Some(generation)
                }) == Some(true)
            })
            .collect();
        pending.sort_by_key(|id| id.0);
        let mut changed = false;
        for id in pending {
            let (verdict, possible, rejections) = match self.judge(id) {
                Ok(judged) => judged,
                Err(_) if self.gas.is_exhausted() => return changed,
                Err(residual) => (Verdict::Failed(residual.into()), Vec::new(), Vec::new()),
            };
            let mut record = self.alternative_record_mut(id);
            record.tried = Some(generation);
            record.possible = possible;
            record.rejections = rejections.into();
            if record.verdict != verdict {
                record.verdict = verdict;
                changed = true;
                self.schedule(id);
            }
            if matches!(verdict, Verdict::Chosen(_)) {
                return true;
            }
        }
        changed
    }

    /// Try each of a judgment's alternatives. One proven without adding a bound
    /// holds outright and is chosen even beside other possible ones, unless
    /// they're overloads. Returns the
    /// verdict, the alternatives left possible, and what trials found of every
    /// alternative, if they rejected each.
    fn judge(&self, id: ObligationId) -> Result<(Verdict, Vec<usize>, Vec<Rejection>), Residual> {
        let (judgments, selections, none) = self
            .alternative_record(id, |record| {
                (
                    record.judgments.clone(),
                    record.selections.clone(),
                    record.none,
                )
            })
            .expect("a judgment with alternatives");
        let mut possible = Vec::new();
        let mut rejections = Vec::new();
        // Only the arguments choose an overload, under the least choice of its
        // own binders, as a call of it alone is solved
        let owned = selections.is_some();
        // An overload whose parameters can't take the call's keys or count is
        // tried only to say why, if none takes it
        let (shaped, misshapen): (Vec<usize>, Vec<usize>) =
            (0..judgments.len()).partition(|&index| !owned || self.admits_shape(judgments[index]));
        let passes = [shaped, misshapen];
        for (pass, indexes) in passes.iter().enumerate() {
            if pass > 0 && !possible.is_empty() {
                break;
            }
            for &index in indexes {
                let judgment = judgments[index];
                let relation = match selections
                    .as_ref()
                    .map_or(judgment, |selections| selections[index])
                {
                    Relation::Subtype {
                        actual,
                        expected,
                        fill,
                    } => {
                        let resolved = self.resolve(actual)?;
                        let actual = self.blinded(resolved).unwrap_or(actual);
                        Relation::Subtype {
                            actual,
                            expected,
                            fill,
                        }
                    }
                    call => call,
                };
                let (status, free, fork, outcome) = self.trial(relation, owned)?;
                match status {
                    Status::Contradicted => {
                        let root = fork.roots[outcome.constraint.0].obligation;
                        let overload = fork.reify(fork.relation(root).checked());
                        rejections.push(Rejection {
                            index,
                            overload: overload.ok(),
                            contradictions: fork.explain(&outcome),
                        })
                    }
                    // Overloads give different results, so another that also takes the
                    // call leaves it ambiguous
                    Status::Proven if free && !owned => {
                        return Ok((Verdict::Chosen(index), vec![], vec![]));
                    }
                    _ => possible.push(index),
                }
            }
        }
        rejections.sort_by_key(|rejection| rejection.index);
        Ok(match possible[..] {
            [index] => (Verdict::Chosen(index), vec![], vec![]),
            [] => (Verdict::Failed(none), vec![], rejections),
            _ => (Verdict::Ambiguous, possible, vec![]),
        })
    }

    /// Whether a call's arguments may fit its callee's parameters by their
    /// shape alone: the keys passed and required, and how many are positional.
    /// Anything it can't read, such as a spread or a key domain passed, may fit.
    fn admits_shape(&self, relation: Relation) -> bool {
        let Relation::Call(call) = relation else {
            return true;
        };
        let (Term::View(arguments), Term::View(callee)) = (call.arguments, call.callee) else {
            return true;
        };
        let Type::Schema(arguments) = self.db.ty(arguments.ty) else {
            return true;
        };
        let mut function = callee.ty;
        if let Type::Quantified { body, .. } = self.db.ty(function) {
            function = *body;
        }
        let Type::Function(function) = self.db.ty(function) else {
            return true;
        };
        let Type::Schema(params) = self.db.ty(function.params) else {
            return true;
        };
        let sym = |key: TypeId| match self.db.ty(key) {
            &Type::Literal(Literal::Sym(name)) => Some(name),
            _ => None,
        };
        let mut positional = 0;
        let mut keys = Vec::new();
        for item in arguments.iter() {
            match (item.multiplicity, &item.element) {
                (Multiplicity::Required, Element::Positional(_)) => positional += 1,
                (Multiplicity::Required, &Element::Keyed { key, .. }) => match sym(key) {
                    Some(name) => keys.push(name),
                    None => return true,
                },
                _ => return true,
            }
        }
        let (mut required, mut most) = (0, Some(0));
        let mut domain = false;
        for item in params.iter() {
            match &item.element {
                Element::Positional(_) => {
                    required += usize::from(item.multiplicity == Multiplicity::Required);
                    most = match item.multiplicity {
                        Multiplicity::Repeated => None,
                        _ => most.map(|most| most + 1),
                    };
                }
                &Element::Keyed { key, .. } => match sym(key) {
                    Some(name) => {
                        let passed = keys.contains(&name);
                        if item.multiplicity == Multiplicity::Required && !passed {
                            return false;
                        }
                        keys.retain(|&key| key != name);
                    }
                    None => domain = true,
                },
                Element::Include(_) => return true,
            }
        }
        (domain || keys.is_empty())
            && positional >= required
            && most.is_none_or(|most| positional <= most)
    }

    /// Solve `relation` on a solver nested in this one, whose work is charged to
    /// this one. Returns the judgment's status, whether the fork's bounds and
    /// assignments stayed as they were, the fork, and the judgment's outcome
    /// there. If `owned`, the fork settles the variables it creates. A fork that
    /// exhausts the budget exhausts this solver too, since they share it.
    fn trial(
        &self,
        relation: Relation,
        owned: bool,
    ) -> Result<(Status, bool, Solver<'_>, Outcome), Residual> {
        self.spend()?;
        trace!(self, "trial {}", self.render_relation(relation));
        let mut fork = self.child(Nesting::Trial, self.gas);
        fork.trials_from = fork.obligation_count();
        if owned {
            fork.owned = Some(fork.variable_count());
        }
        let generation = fork.generation.get();
        // Alternatives choose through variables, whose recursion is the occurs
        // check's, so a trial starts unguarded
        let obligation = fork.enqueue(relation, false);
        let constraint = ConstraintId(fork.roots.len());
        fork.roots.push(Root {
            obligation,
            provenance: Provenance::default(),
        });
        fork.quiesce();
        if self.gas.is_exhausted() {
            return Err(Residual::Limit);
        }
        let outcome = fork.outcome(constraint);
        let status = outcome.status;
        trace!(self, "trial: {status:?}");
        let free = fork.generation.get() == generation;
        Ok((status, free, fork, outcome))
    }
}
