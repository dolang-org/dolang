//! Judgments that need one of their alternatives to hold: a union on the right
//! whose terms aren't all closed, where a member must be chosen to infer through,
//! or a callable or overloaded function on the left or called, with several
//! signatures, one of which must fit.
//!
//! Alternatives are judged by trials, each on a fork of the solver: a trial adds
//! the alternative's own judgment and solves, so nothing it finds reaches the
//! solver it was forked from. A trial whose judgment is contradicted rejects its
//! alternative; any other leaves it possible. One alternative left possible is
//! chosen, and the judgment derives it. A rejection holds under every later state,
//! since bounds only grow, so a choice doesn't depend on the order judgments are
//! made in. Several possible alternatives leave the judgment ambiguous until what
//! they relate grows; none contradicts it.
//!
//! A judgment may be tried against a selection in place of what it relates to,
//! as a call's overloads are tried against what its arguments alone say: its
//! choice is then related to the whole. When every alternative is rejected, the
//! trials that rejected them are kept, to say why (see [`Solver::rejections`]).
//!
//! Trials run when solving is quiescent, before any scope is settled, so a fork
//! never holds a judgment half processed. They judge one obligation at a time, in
//! creation order, and solving resumes after each choice.

use std::rc::Rc;

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

/// A trial that rejected an alternative, in order: the fork it solved, and the
/// outcome there of the alternative's judgment
#[derive(Clone)]
pub(crate) struct Rejection<'db> {
    /// The alternative's index
    pub(crate) index: usize,
    pub(crate) solver: Rc<Solver<'db>>,
    pub(crate) outcome: Outcome,
}

/// A judgment that needs one of its alternatives to hold
#[derive(Clone)]
pub(super) struct Alternatives<'db> {
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
    /// The trials that rejected every alternative, once they have
    rejections: Vec<Rejection<'db>>,
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
        self.blinded.get(&term).copied().unwrap_or(term)
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
        let judgments = (terms.into_iter())
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
        let judgments = (terms.into_iter())
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
        let (judgments, selections) = (terms.into_iter())
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
        let fill = match self.obligations[obligation.0].relation {
            Relation::Subtype { fill, .. } => fill,
            Relation::Call(_) => Fill::Arguments,
        };
        Relation::Subtype {
            actual,
            expected,
            fill,
        }
    }

    /// The trials that rejected each alternative of a judgment none of whose
    /// alternatives is possible
    pub(crate) fn rejections(&self, obligation: ObligationId) -> Vec<Rejection<'db>> {
        (self.alternatives.borrow().get(&obligation))
            .map_or_else(Vec::new, |record| record.rejections.clone())
    }

    /// The alternatives left possible of a judgment that's ambiguous
    pub(crate) fn possible(&self, obligation: ObligationId) -> Vec<usize> {
        match self.alternatives.borrow().get(&obligation) {
            Some(record) if record.verdict == Verdict::Ambiguous => record.possible.clone(),
            _ => Vec::new(),
        }
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
        let verdict = {
            let mut records = self.alternatives.borrow_mut();
            let record = records.entry(obligation).or_insert_with(|| Alternatives {
                judgments,
                selections,
                none,
                tried: None,
                verdict: Verdict::Untried,
                possible: Vec::new(),
                rejections: Vec::new(),
            });
            match record.verdict {
                Verdict::Chosen(index) => Ok((index, record.judgments[index])),
                Verdict::Untried => Err(Residual::Inference.into()),
                Verdict::Ambiguous => Err(Residual::Ambiguous.into()),
                Verdict::Failed(issue) => Err(issue),
            }
        };
        let (index, judgment) = verdict?;
        self.derive_relation(obligation, judgment, step(index));
        Ok(())
    }

    /// What a union on the right is when trials reject every member: a
    /// contradiction for a literal, a concrete class or a function, which are
    /// outside a union none of whose members admits them. A protocol may be
    /// covered by several members' classes, and a generic member's rejected
    /// arguments needn't exclude every value of a class, so they leave it residual,
    /// as do members that are projections. A literal's or function's class is
    /// fixed, so its rejections are final, and so are top's, which no member
    /// but top covers.
    pub(super) fn refuted(
        &self,
        actual: Term,
        view: TypeView,
        members: &[UnionMember],
    ) -> Result<Issue, Issue> {
        let residual = Residual::Unsupported("a type that may be inside a union member").into();
        let fixed = match self.head(actual) {
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
                false
            }
            Ok(_) => return Ok(residual),
            Err(Issue::Residual(_)) => return Ok(residual),
            Err(issue) => return Err(issue),
        };
        for member in members {
            let UnionMember::Type(ty) = *member else {
                return Ok(residual);
            };
            if fixed {
                continue;
            }
            match self.head(view.child(ty)) {
                Ok(Head::Nominal(nominal)) if !nominal.arguments.is_empty() => return Ok(residual),
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
        if self.trial_depth >= TRIAL_DEPTH {
            return false;
        }
        let generation = self.generation.get();
        let mut pending: Vec<ObligationId> = (self.alternatives.borrow().iter())
            .filter(|(id, record)| {
                id.0 >= self.trials_from
                    && matches!(record.verdict, Verdict::Untried | Verdict::Ambiguous)
                    && record.tried != Some(generation)
            })
            .map(|(&id, _)| id)
            .collect();
        pending.sort_by_key(|id| id.0);
        let mut changed = false;
        for id in pending {
            let (verdict, possible, rejections) = match self.judge(id) {
                Ok(judged) => judged,
                Err(_) if self.exhausted.get() => return changed,
                Err(residual) => (Verdict::Failed(residual.into()), Vec::new(), Vec::new()),
            };
            let mut records = self.alternatives.borrow_mut();
            let record = records.get_mut(&id).expect("a judged obligation");
            record.tried = Some(generation);
            record.possible = possible;
            record.rejections = rejections;
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
    /// holds outright and is chosen even beside other possible ones. Returns the
    /// verdict, the alternatives left possible, and the trials that rejected
    /// every alternative, if they did.
    fn judge(
        &self,
        id: ObligationId,
    ) -> Result<(Verdict, Vec<usize>, Vec<Rejection<'db>>), Residual> {
        let (judgments, selections, none) = {
            let records = self.alternatives.borrow();
            let record = &records[&id];
            (
                record.judgments.clone(),
                record.selections.clone(),
                record.none,
            )
        };
        let mut possible = Vec::new();
        let mut rejections = Vec::new();
        for (index, &judgment) in judgments.iter().enumerate() {
            // Only the arguments choose an overload, under the least choice of
            // its own binders, as a call of it alone is solved
            let owned = selections.is_some();
            let relation =
                match (selections.as_ref()).map_or(judgment, |selections| selections[index]) {
                    Relation::Subtype {
                        actual,
                        expected,
                        fill,
                    } => {
                        let resolved = self.resolve(actual)?;
                        let actual = self.blinded.get(&resolved).copied().unwrap_or(actual);
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
                Status::Contradicted => rejections.push(Rejection {
                    index,
                    solver: Rc::new(fork),
                    outcome,
                }),
                Status::Proven if free => return Ok((Verdict::Chosen(index), vec![], vec![])),
                _ => possible.push(index),
            }
        }
        Ok(match possible[..] {
            [index] => (Verdict::Chosen(index), vec![], vec![]),
            [] => (Verdict::Failed(none), vec![], rejections),
            _ => (Verdict::Ambiguous, possible, vec![]),
        })
    }

    /// Solve `relation` on a fork of this solver, whose work is charged to this
    /// one. Returns the judgment's status, whether the fork's bounds and
    /// assignments stayed as they were, the fork, and the judgment's outcome
    /// there. If `owned`, the fork settles the variables it creates. A fork that
    /// exhausts the budget exhausts this solver too, since they share it.
    fn trial(
        &self,
        relation: Relation,
        owned: bool,
    ) -> Result<(Status, bool, Solver<'db>, Outcome), Residual> {
        self.spend()?;
        trace!(self, "trial {}", self.render_relation(relation));
        let mut fork = self.clone();
        fork.trial_depth += 1;
        fork.trials_from = fork.obligations.len();
        if owned {
            fork.owned = Some(fork.inference.len());
        }
        #[cfg(feature = "debug")]
        {
            fork.indent += 1;
        }
        let generation = fork.generation.get();
        let obligation = fork.enqueue(relation);
        let constraint = ConstraintId(fork.roots.len());
        fork.roots.push(Root {
            obligation,
            provenance: Provenance::default(),
        });
        fork.quiesce();
        self.work.set(fork.work.get());
        if fork.exhausted.get() {
            self.exhausted.set(true);
            return Err(Residual::Limit);
        }
        let outcome = fork.outcome(constraint);
        let status = outcome.status;
        trace!(self, "trial: {status:?}");
        let free = fork.generation.get() == generation;
        Ok((status, free, fork, outcome))
    }
}
