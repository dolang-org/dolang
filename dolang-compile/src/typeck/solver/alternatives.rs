//! Judgments that need one of their alternatives to hold: a union on the right
//! whose terms aren't all closed, where a member must be chosen to infer through,
//! or a callable on the left with several signatures, one of which must fit.
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

/// A judgment that needs one of its alternatives to hold
#[derive(Clone, Debug)]
pub(super) struct Alternatives {
    /// Each alternative's own judgment, `actual <: expected`
    judgments: Vec<(Term, Term)>,
    /// What the judgment is when no alternative is possible
    none: Issue,
    /// The generation its trials last ran at
    tried: Option<usize>,
    verdict: Verdict,
}

impl Solver<'_> {
    /// Have trials judge `twin` in place of `term`, where `term` is the actual side
    /// of a judgment with alternatives. A `do` block's type whose result is known
    /// has a twin leaving the result to a variable, so that what the block gives
    /// doesn't choose: the choice is then checked with what it gives.
    pub(crate) fn blind(&mut self, term: Term, twin: Term) {
        self.blinded.insert(term, twin);
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
        let judgments = terms.into_iter().map(|term| (actual, term)).collect();
        self.choose_judgment(obligation, judgments, step, none)
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
        let judgments = terms.into_iter().map(|term| (term, expected)).collect();
        self.choose_judgment(obligation, judgments, step, none)
    }

    /// Derive the alternative judgment trials chose, labeled by `step`
    fn choose_judgment(
        &self,
        obligation: ObligationId,
        judgments: Vec<(Term, Term)>,
        step: fn(usize) -> Step,
        none: Issue,
    ) -> Result<(), Issue> {
        let verdict = {
            let mut records = self.alternatives.borrow_mut();
            let record = records.entry(obligation).or_insert_with(|| Alternatives {
                judgments,
                none,
                tried: None,
                verdict: Verdict::Untried,
            });
            match record.verdict {
                Verdict::Chosen(index) => Ok((index, record.judgments[index])),
                Verdict::Untried => Err(Residual::Inference.into()),
                Verdict::Ambiguous => Err(Residual::Ambiguous.into()),
                Verdict::Failed(issue) => Err(issue),
            }
        };
        let (index, (actual, expected)) = verdict?;
        self.derive(obligation, actual, expected, step(index));
        Ok(())
    }

    /// What a union on the right is when trials reject every member: a
    /// contradiction for a literal, a concrete class or a function, which are
    /// outside a union none of whose members admits them. A protocol may be
    /// covered by several members' classes, and a generic member's rejected
    /// arguments needn't exclude every value of a class, so they leave it residual,
    /// as do members that are projections. A literal's or function's class is
    /// fixed, so its rejections are final.
    pub(super) fn refuted(
        &self,
        actual: Term,
        view: TypeView,
        members: &[UnionMember],
    ) -> Result<Issue, Issue> {
        let residual = Residual::Unsupported("a type that may be inside a union member").into();
        let fixed = match self.head(actual) {
            Ok(Head::Structural(view)) => match self.db.ty(view.ty) {
                Type::Literal(_) | Type::Function(_) | Type::Quantified { .. } => true,
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
            .filter(|(_, record)| {
                matches!(record.verdict, Verdict::Untried | Verdict::Ambiguous)
                    && record.tried != Some(generation)
            })
            .map(|(&id, _)| id)
            .collect();
        pending.sort_by_key(|id| id.0);
        let mut changed = false;
        for id in pending {
            let verdict = match self.judge(id) {
                Ok(verdict) => verdict,
                Err(_) if self.exhausted.get() => return changed,
                Err(residual) => Verdict::Failed(residual.into()),
            };
            let mut records = self.alternatives.borrow_mut();
            let record = records.get_mut(&id).expect("a judged obligation");
            record.tried = Some(generation);
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
    /// holds outright and is chosen even beside other possible ones.
    fn judge(&self, id: ObligationId) -> Result<Verdict, Residual> {
        let (judgments, none) = {
            let records = self.alternatives.borrow();
            let record = &records[&id];
            (record.judgments.clone(), record.none)
        };
        let language = self.obligations[id.0].relation.language;
        let mut possible = Vec::new();
        for (index, &(actual, expected)) in judgments.iter().enumerate() {
            let resolved = self.resolve(actual)?;
            let actual = self.blinded.get(&resolved).copied().unwrap_or(actual);
            let (status, free) = self.trial(actual, expected, language)?;
            match status {
                Status::Contradicted => {}
                Status::Proven if free => return Ok(Verdict::Chosen(index)),
                _ => possible.push(index),
            }
        }
        Ok(match possible[..] {
            [index] => Verdict::Chosen(index),
            [] => Verdict::Failed(none),
            _ => Verdict::Ambiguous,
        })
    }

    /// Solve `actual <: expected` on a fork of this solver, whose work is charged
    /// to this one. Returns the judgment's status, and whether the fork's bounds
    /// and assignments stayed as they were. A fork that exhausts the budget
    /// exhausts this solver too, since they share it.
    pub(super) fn trial(
        &self,
        actual: Term,
        expected: Term,
        language: bool,
    ) -> Result<(Status, bool), Residual> {
        self.spend()?;
        trace!(
            self,
            "trial {} <: {}",
            self.render(actual),
            self.render(expected)
        );
        let mut fork = self.clone();
        fork.trial_depth += 1;
        #[cfg(feature = "debug")]
        {
            fork.indent += 1;
        }
        let generation = fork.generation.get();
        let obligation = fork.enqueue(Relation {
            actual,
            expected,
            language,
        });
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
        let status = fork.outcome(constraint).status;
        trace!(self, "trial: {status:?}");
        Ok((status, fork.generation.get() == generation))
    }
}
