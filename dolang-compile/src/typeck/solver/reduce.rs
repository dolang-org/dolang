//! Reduction of one subtype judgment.

use std::ops::ControlFlow::{self, Break, Continue};

use super::*;
use crate::typeck::r#type::Projected;

/// A judgment being reduced, with the heads of its terms
struct Judgment {
    obligation: ObligationId,
    actual: Term,
    expected: Term,
    a: Head,
    b: Head,
}

/// A rule of [`Solver::reduce`]
type Rule<'db> = fn(&Solver<'db>, &Judgment) -> Result<ControlFlow<()>, Issue>;

impl Solver<'_> {
    /// Reduce one relation, recording bounds or child obligations, or return a diagnostic issue.
    /// Success means local reduction succeeded; child obligations may still fail or remain unresolved.
    ///
    /// Each rule in turn may decide the judgment, by succeeding with
    /// [`Break`] or failing, or leave it to the next with [`Continue`]. Past
    /// [`Self::variables`], neither head is an inference variable.
    pub(super) fn reduce(&self, obligation: ObligationId) -> Result<(), Issue> {
        let (actual, expected) = match self.obligations[obligation.0].relation {
            Relation::Subtype {
                actual, expected, ..
            } => (actual, expected),
            Relation::Call(call) => return self.call(obligation, call),
        };
        self.record_bounds(obligation, actual, expected)?;
        // Anything is below top and the dynamic type, even what can't be exposed
        let b = self.head(expected)?;
        if self.is_unknown(&b)
            || matches!(&b, Head::Structural(view) if view.ty == self.db.top())
                && self.kind(actual) == Kind::Type
        {
            return Ok(());
        }
        let a = self.head(actual)?;
        let judgment = Judgment {
            obligation,
            actual,
            expected,
            a,
            b,
        };
        let rules: [Rule<'_>; 10] = [
            Self::identical,
            Self::judgeable,
            Self::variables,
            Self::listed,
            Self::pack,
            Self::bounded,
            Self::projections,
            Self::abstract_right,
            Self::union_right,
            Self::overloaded,
        ];
        for rule in rules {
            if rule(self, &judgment)?.is_break() {
                return Ok(());
            }
        }
        self.structure(judgment)
    }

    /// Record a bound for an inference variable that either side resolves to
    fn record_bounds(
        &self,
        obligation: ObligationId,
        actual: Term,
        expected: Term,
    ) -> Result<(), Issue> {
        for (mut term, other, lower) in [(actual, expected, false), (expected, actual, true)] {
            for depth in 0.. {
                self.depth(depth)?;
                self.spend()?;
                match term {
                    Term::Infer(id) => {
                        if term != other {
                            self.add_bound(id, other, lower, obligation)?;
                        }
                        break;
                    }
                    Term::Skolem(_) => break,
                    Term::View(view) => {
                        let Type::Bound { reference, kind } = *self.db.ty(view.ty) else {
                            break;
                        };
                        term = self.lookup(view.environment, reference.depth, reference.slot, kind);
                    }
                }
            }
        }
        Ok(())
    }

    /// The dynamic type or schema is consistent with anything of its kind, bottom
    /// is below any type, and anything is below itself
    fn identical(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        let holds = match (&j.a, &j.b) {
            (a, _) if self.is_unknown(a) => true,
            (Head::Structural(a), _)
                if a.ty == self.db.bottom() && self.kind(j.expected) == Kind::Type =>
            {
                true
            }
            (Head::Structural(a), Head::Structural(b)) => {
                self.same(Term::View(*a), Term::View(*b))?
            }
            (Head::Skolem(a), Head::Skolem(b)) => a == b,
            _ => false,
        };
        Ok(if holds { Break(()) } else { Continue(()) })
    }

    /// Past identity, a type the database can't represent can't be judged, and
    /// a rigid of a declaration not being checked has escaped
    fn judgeable(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        for head in [&j.a, &j.b] {
            if let Head::Structural(view) = head
                && let Type::Unsupported { .. } = self.db.ty(view.ty)
            {
                return Err(UNREPRESENTED.into());
            }
        }
        for head in [&j.a, &j.b] {
            if let Head::Structural(view) = head {
                self.rigid(view.ty)?;
            }
        }
        Ok(Continue(()))
    }

    /// An inference variable on either side takes the other as a bound, and the
    /// judgment waits on its solution
    fn variables(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        match (&j.a, &j.b) {
            (Head::Infer(a), Head::Infer(b)) if a == b => return Ok(Break(())),
            (&Head::Infer(a), &Head::Infer(b)) => {
                self.add_bound(a, Term::Infer(b), false, j.obligation)?;
                self.add_bound(b, Term::Infer(a), true, j.obligation)?;
            }
            (&Head::Infer(id), _) => self.add_bound(id, j.expected, false, j.obligation)?,
            (_, &Head::Infer(id)) => self.add_bound(id, j.actual, true, j.obligation)?,
            _ => return Ok(Continue(())),
        }
        Err(Residual::Inference.into())
    }

    /// Anything is below a union that lists it
    fn listed(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        if let Head::Structural(view) = &j.b
            && let Type::Union(members) = self.db.ty(view.ty)
        {
            for member in members.iter() {
                if let UnionMember::Type(ty) = *member
                    && self.same(j.actual, view.child(ty))?
                {
                    return Ok(Break(()));
                }
            }
        }
        Ok(Continue(()))
    }

    /// A positional pack's items are each below a union that expands it
    fn pack(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        let binding = match &j.a {
            Head::Skolem(id) => Some(self.skolems[id.0].binding),
            Head::Structural(view) => self.rigid(view.ty)?.map(|binder| binder.binding),
            _ => None,
        };
        if binding == Some(Binding::Rest(Rest::Positional)) && self.expands_into(j.actual, &j.b)? {
            return Ok(Break(()));
        }
        Ok(Continue(()))
    }

    /// A rigid or skolem is below whatever its bound is below. Without a bound,
    /// a skolem is below only a union with a member that admits anything, since
    /// it can't be a member's alternative; a rigid is left to such a union's
    /// members.
    fn bounded(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        let (bound, step) = match &j.a {
            Head::Structural(view) => {
                let Some(binder) = self.rigid(view.ty)? else {
                    return Ok(Continue(()));
                };
                let step = match binder.binding {
                    Binding::Implicit => Step::ImplicitBound,
                    _ => Step::RigidBound,
                };
                (
                    self.rigid_bound(view.ty).map(|bound| self.closed(bound)),
                    step,
                )
            }
            Head::Skolem(id) => {
                let skolem = &self.skolems[id.0];
                let step = match skolem.binding {
                    Binding::Implicit => Step::ImplicitBound,
                    _ => Step::SkolemBound,
                };
                (skolem.bound.get(), step)
            }
            _ => return Ok(Continue(())),
        };
        if let Some(bound) = bound {
            self.derive(j.obligation, bound, j.expected, step);
            return Ok(Break(()));
        }
        match &j.a {
            Head::Skolem(_) => self.unbounded_skolem(&j.b).map(Break),
            _ if matches!(&j.b, Head::Structural(view)
                if matches!(self.db.ty(view.ty), Type::Union(_))) =>
            {
                Ok(Continue(()))
            }
            _ => Err(Issue::Contradiction(Contradiction::Rigid)),
        }
    }

    /// A union of projections on the left. A projection is of a rigid's schema,
    /// or can't be evaluated. The former is below the same projection of the
    /// rigid's bound. An item projection is also below one of the same schema
    /// whose key selects as much, or for `AssignItem`, as little.
    fn projections(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        let Head::Structural(view) = &j.a else {
            return Ok(Continue(()));
        };
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(Continue(()));
        };
        self.conflicting(members)?;
        let mut derived = Vec::new();
        for (index, &member) in members.iter().enumerate() {
            let (term, step) = match member {
                UnionMember::Type(ty) => (view.child(ty), Step::UnionMember(index)),
                _ if self.shares(*view, member, &j.b)? => continue,
                _ if self.congruent(*view, member, &j.b)? => continue,
                _ if self.rigid(member.id())?.is_some() => {
                    let Some(bound) = self.rigid_bound(member.id()) else {
                        return Err(Residual::Unsupported(
                            "a projection of a rigid without a bound",
                        )
                        .into());
                    };
                    // The key may refer to the view's binders
                    let projected = self.db.intern(Type::Union(vec![member.with(bound)].into()));
                    (view.child(projected), Step::RigidBound)
                }
                _ => return Err(Residual::Unsupported("an unevaluated projection").into()),
            };
            derived.push((term, step));
        }
        for (term, step) in derived {
            self.derive(j.obligation, term, j.expected, step);
        }
        Ok(Break(()))
    }

    /// Only itself, bottom and the dynamic type are below a rigid or skolem
    fn abstract_right(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        let rigid = match &j.b {
            Head::Structural(view) => self.rigid(view.ty)?.is_some(),
            Head::Skolem(_) => true,
            _ => false,
        };
        match rigid {
            true => Err(Issue::Contradiction(Contradiction::Rigid)),
            false => Ok(Continue(())),
        }
    }

    /// Something below a union is below one of its members. With a member to
    /// infer through, trials choose one; otherwise each is probed.
    fn union_right(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        let Head::Structural(view) = &j.b else {
            return Ok(Continue(()));
        };
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Ok(Continue(()));
        };
        self.conflicting(members)?;
        let alternatives: Vec<Term> = (members.iter())
            .map(|&member| match member {
                UnionMember::Type(ty) => view.child(ty),
                _ => view.child(self.db.intern(Type::Union(vec![member].into()))),
            })
            .collect();
        // A judgment that once had a member to infer through stays with
        // trials, which judge a `do` block without its result
        let tried = self.alternatives.borrow().contains_key(&j.obligation);
        // An overloaded function's members are chosen among, as a call's are
        let overloaded = matches!(&j.a, Head::Structural(view)
            if matches!(self.db.ty(view.ty), Type::Overloaded { .. }));
        let closed = (self.reify(j.actual).ok()).filter(|_| {
            !tried && !overloaded && alternatives.iter().all(|&term| self.reify(term).is_ok())
        });
        match closed {
            Some(actual) => self.closed_member(actual, *view, members)?,
            None => {
                let refuted = self.refuted(j.actual, *view, members)?;
                let step = Step::UnionMember;
                self.choose(j.obligation, j.actual, alternatives, step, refuted)?;
            }
        }
        Ok(Break(()))
    }

    /// An overloaded function on the left of a function type is one of its
    /// overloads, chosen by trials against what the function type's parameters
    /// alone say (see [`Solver::selection`]); none fitting contradicts it. A
    /// lone overload is chosen without a trial.
    /// Anywhere else it's its implementation, and dynamic without one. What's
    /// below it is below each of its signatures.
    fn overloaded(&self, j: &Judgment) -> Result<ControlFlow<()>, Issue> {
        if let Head::Structural(view) = &j.b
            && let Type::Overloaded {
                overloads,
                implementation,
                ..
            } = self.db.ty(view.ty)
        {
            for (index, &ty) in overloads.iter().enumerate() {
                self.derive(
                    j.obligation,
                    j.actual,
                    view.child(ty),
                    Step::Overload(index),
                );
            }
            if let Some(ty) = *implementation {
                self.derive(j.obligation, j.actual, view.child(ty), Step::Implementation);
            }
            return Ok(Break(()));
        }
        let Head::Structural(view) = &j.a else {
            return Ok(Continue(()));
        };
        let Type::Overloaded {
            overloads,
            implementation,
            ..
        } = self.db.ty(view.ty)
        else {
            return Ok(Continue(()));
        };
        let function = match &j.b {
            Head::Structural(function) if self.callee(function.ty) => *function,
            _ => {
                if let Some(ty) = *implementation {
                    let step = Step::Implementation;
                    self.derive(j.obligation, view.child(ty), j.expected, step);
                }
                return Ok(Break(()));
            }
        };
        if let Type::Quantified { binders, body } = self.db.ty(function.ty) {
            self.skolemization(function, binders, *body, j.actual, j.obligation)?;
            return Ok(Break(()));
        }
        // A lone overload is always chosen, so what doesn't fit it is diagnosed
        // as for any function
        if let &[overload] = &overloads[..] {
            self.derive(
                j.obligation,
                view.child(overload),
                j.expected,
                Step::Overload(0),
            );
            return Ok(Break(()));
        }
        let terms = overloads.iter().map(|&ty| view.child(ty)).collect();
        let selection = self.selection(function)?;
        let none = Issue::Contradiction(Contradiction::NoOverload);
        self.choose_selected(
            j.obligation,
            terms,
            j.expected,
            selection,
            Step::Overload,
            none,
        )?;
        Ok(Break(()))
    }

    /// What an overloaded function's overloads are tried against in place of
    /// `function`, a function type: it with its result `Value`, since only what's
    /// passed chooses
    fn selection(&self, function: TypeView) -> Result<Term, Issue> {
        let Type::Function(function_type) = self.db.ty(function.ty) else {
            unreachable!("a function type")
        };
        let ty = self.db.intern(Type::Function(Function {
            result: self.db.top(),
            ..function_type.clone()
        }));
        Ok(self.view(ty, function.environment))
    }

    /// Whether a closed type is below a member of a closed union. Testing
    /// closed alternatives must never add bounds to this solver.
    ///
    /// A literal, concrete class or top is outside a union if every member
    /// excludes it. For a class with infinitely many literals, a finite
    /// set of literals cannot cover it either. Protocols may be covered
    /// by several implementations, so unrelated alternatives are not
    /// enough to refute their inclusion.
    fn closed_member(
        &self,
        actual: TypeId,
        view: TypeView,
        members: &[UnionMember],
    ) -> Result<(), Issue> {
        let infinite = [Intrinsic::Int, Intrinsic::Str, Intrinsic::Sym]
            .into_iter()
            .any(|intrinsic| self.db.intrinsic(intrinsic) == Some(actual));
        let function = matches!(
            self.db.ty(actual),
            Type::Function(_) | Type::Quantified { .. }
        );
        // No members but top and the dynamic type cover top
        let top = actual == self.db.top();
        let mut outside = function || top || self.class_like(actual)?;
        for member in members.iter() {
            let UnionMember::Type(ty) = *member else {
                outside = false;
                continue;
            };
            let Ok(expected) = self.reify(view.child(ty)) else {
                outside = false;
                continue;
            };
            if infinite && self.db.literal(expected).is_some() {
                continue;
            }
            match self.probe(actual, expected)? {
                Status::Proven => return Ok(()),
                // Keep generic alternatives conservative: a failed argument
                // comparison need not exclude every value of the actual class
                // (notably recursive data unions).
                Status::Contradicted => {
                    if !infinite
                        && !function
                        && !top
                        && self.db.literal(actual).is_none()
                        && self
                            .start(expected)?
                            .is_some_and(|nominal| !nominal.arguments.is_empty())
                    {
                        outside = false;
                    }
                }
                Status::Unresolved => outside = false,
            }
        }
        Err(match outside {
            true => Issue::Contradiction(Contradiction::Outside),
            false => Residual::Unsupported("a type that may be inside a union member").into(),
        })
    }

    /// Relate the heads by their forms
    fn structure(&self, j: Judgment) -> Result<(), Issue> {
        let Judgment {
            obligation,
            actual,
            expected,
            a,
            b,
        } = j;
        // Top has values outside anything else: top itself and the dynamic type
        // are proven before the rules, and variables, unions and rigids are theirs.
        // This holds while protocols are nominal; once they're structural (#828),
        // top is inside a protocol without members.
        if matches!(&a, Head::Structural(view) if view.ty == self.db.top()) {
            return Err(Issue::Contradiction(Contradiction::Outside));
        }
        match (a, b) {
            (Head::Nominal(a), Head::Nominal(b)) => self.nominal(a, b, obligation),
            (Head::Nominal(nominal), Head::Structural(view)) => {
                if let Some(literal) = self.db.literal(view.ty) {
                    self.class_below_literal(actual, literal)
                } else if self.callee(view.ty) {
                    self.callable(nominal, actual, view, expected, obligation)
                } else {
                    Err(Residual::Unsupported("these kinds of type").into())
                }
            }
            (Head::Structural(a), Head::Structural(b)) => {
                self.structural(a, b, actual, expected, obligation)
            }
            (Head::Structural(view), Head::Nominal(_)) => {
                self.structural_below_class(view, expected, obligation)
            }
            _ => Err(Residual::Unsupported("these kinds of type").into()),
        }
    }

    /// A class is below one of its literals only if it's `nil`'s class, and
    /// outside it if it's outside the literal's class
    fn class_below_literal(&self, actual: Term, literal: &Literal) -> Result<(), Issue> {
        let intrinsic = literal.intrinsic();
        let backing =
            (self.db.intrinsic(intrinsic)).ok_or(Residual::MissingIntrinsic(intrinsic))?;
        if self.probe(self.reify(actual)?, backing)? == Status::Contradicted {
            return Err(Issue::Contradiction(Contradiction::Outside));
        }
        if *literal == Literal::Nil && self.same(actual, self.closed(backing))? {
            return Ok(());
        }
        Err(Residual::Unsupported("a class below one of its literals").into())
    }

    /// Relate two distinct structural types
    fn structural(
        &self,
        a: TypeView,
        b: TypeView,
        actual: Term,
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        match (self.db.ty(a.ty), self.db.ty(b.ty)) {
            (Type::Literal(_), Type::Literal(_)) => {
                Err(Issue::Contradiction(Contradiction::DistinctLiterals))
            }
            (Type::Function(a_func), Type::Function(b_func)) => {
                self.functions(a, a_func, b, b_func, obligation)
            }
            (Type::Quantified { binders, body }, Type::Function(_)) => {
                self.instantiation(a, binders, *body, expected, obligation)
            }
            (_, Type::Quantified { binders, body }) => {
                self.skolemization(b, binders, *body, actual, obligation)
            }
            (Type::Schema(xs), Type::Schema(ys)) => {
                self.schemas(a, xs, b, ys, expected, obligation)
            }
            // A mapping relates as a schema including it, unless it relates
            // to the same mapping pack by pack
            (Type::Schema(_) | Type::Map { .. }, Type::Schema(_) | Type::Map { .. }) => {
                if self.mappings(a, b, obligation)? {
                    return Ok(());
                }
                let schema = |view: TypeView| match self.db.ty(view.ty) {
                    Type::Map { .. } => TypeView {
                        ty: self.db.intern(Type::Schema(
                            vec![SchemaItem {
                                multiplicity: Multiplicity::Required,
                                element: Element::Include(view.ty),
                            }]
                            .into(),
                        )),
                        ..view
                    },
                    _ => view,
                };
                let (a, b) = (schema(a), schema(b));
                let (Type::Schema(xs), Type::Schema(ys)) = (self.db.ty(a.ty), self.db.ty(b.ty))
                else {
                    unreachable!("both are schemas")
                };
                self.schemas(a, xs, b, ys, Term::View(b), obligation)
            }
            // A literal is a function only if its class is
            (Type::Literal(literal), Type::Function(_)) => {
                let intrinsic = literal.intrinsic();
                let backing =
                    (self.db.intrinsic(intrinsic)).ok_or(Residual::MissingIntrinsic(intrinsic))?;
                let step = Step::IntrinsicBacking(intrinsic);
                self.derive(obligation, self.closed(backing), expected, step);
                Ok(())
            }
            // A function is a literal only if its class is
            (Type::Function(_) | Type::Quantified { .. }, Type::Literal(_)) => {
                let backing = (self.db.intrinsic(Intrinsic::Func))
                    .ok_or(Residual::MissingIntrinsic(Intrinsic::Func))?;
                let step = Step::IntrinsicBacking(Intrinsic::Func);
                self.derive(obligation, self.closed(backing), expected, step);
                Ok(())
            }
            _ => Err(Residual::Unsupported("these structural types").into()),
        }
    }

    /// Relate a structural type to a class through the class it belongs to
    fn structural_below_class(
        &self,
        view: TypeView,
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        // A generic class's object, `[S] Type[C[S]]`, is its class applied
        // to unknown arguments, as a type test narrows to: written alone,
        // the class says nothing of them, so they aren't inferred
        if let Type::Quantified { binders, body } = self.db.ty(view.ty)
            && let Type::Apply { base, .. } = self.db.ty(*body)
            && Some(*base) == self.db.intrinsic(Intrinsic::Type)
        {
            let unknowns: Vec<TypeId> = (binders.iter())
                .map(|binder| self.db.unknown_of(binder.kind))
                .collect();
            let applied = self.db.substitute(*body, &unknowns);
            let applied = self.view(applied, view.environment);
            self.derive(obligation, applied, expected, Step::Instantiation);
            return Ok(());
        }
        // A quantified function belongs to its body's class, `Func`, as an
        // unquantified one does
        let mut ty = view.ty;
        while let Type::Quantified { body, .. } = self.db.ty(ty) {
            self.spend()?;
            ty = *body;
        }
        let (class, intrinsic) = match self.db.ty(ty) {
            Type::Function(_) => {
                let class = (self.db.func_class(view.ty))
                    .ok_or(Residual::MissingIntrinsic(Intrinsic::Func))?;
                (self.view(class, view.environment), Intrinsic::Func)
            }
            Type::Literal(literal) => {
                let intrinsic = literal.intrinsic();
                let backing =
                    (self.db.intrinsic(intrinsic)).ok_or(Residual::MissingIntrinsic(intrinsic))?;
                (self.closed(backing), intrinsic)
            }
            _ => return Err(Residual::Unsupported("a structural type below a class").into()),
        };
        let step = Step::IntrinsicBacking(intrinsic);
        self.derive(obligation, class, expected, step);
        Ok(())
    }

    /// Every new bound is paired with the opposite bounds. Variable-to-variable
    /// bounds use this same rule; derived obligations carry propagation onward.
    ///
    /// A bound may only hold skolems the variable's scope sees. A skolem from
    /// inside it that is a whole lower bound is promoted: the variable is above
    /// its bound instead, the least type above it without it. Any other escape
    /// leaves the bound unrecorded and the judgment residual; a solution found
    /// for the variable otherwise is then related to the skolem directly. A
    /// quantified upper bound would need impredicative instantiation, so it is
    /// residual too.
    fn add_bound(
        &self,
        id: InferVarId,
        term: Term,
        lower: bool,
        source: ObligationId,
    ) -> Result<(), Residual> {
        let scope = self.inference[id.0].scope;
        let mut leaves = HashSet::new();
        self.solved_leaves(term, &mut leaves)?;
        let escaped = leaves.iter().any(|leaf| {
            matches!(*leaf, Term::Skolem(skolem)
                if !self.visible(scope, self.skolems[skolem.0].scope))
        });
        if escaped {
            // A solution is related to the skolem directly
            if self.assignment(id).is_some() {
                return Ok(());
            }
            if lower && let Term::Skolem(skolem) = self.resolve(term)? {
                let bound = (self.skolems[skolem.0].bound.get())
                    .unwrap_or_else(|| self.closed(self.db.top()));
                self.derive(source, bound, Term::Infer(id), Step::Promotion);
                return Ok(());
            }
            return Err(Residual::Escape);
        }
        if !lower && self.quantified(term)? {
            return Err(Residual::Unsupported("a variable below a quantified type"));
        }
        let bounds = &self.bounds[id.0];
        let (same, opposite) = if lower {
            (&bounds.lower, &bounds.upper)
        } else {
            (&bounds.upper, &bounds.lower)
        };
        let sources = same.get_or_insert_with(&term, |term| (*term, MonoHashSet::new()));
        // A new source for an existing term still needs diagnostic edges, even
        // though the resulting subtype obligations may already be interned.
        if sources.try_insert(source).is_err() {
            return Ok(());
        }
        self.inference[id.0].dirty.set(true);
        self.generation.set(self.generation.get() + 1);
        for (&other, other_sources) in opposite.iter() {
            self.spend()?;
            // L <: V <: U requires L <: U. L or U may itself be an inference
            // variable, so ordinary reduction also propagates variable chains.
            let (actual, expected) = if lower { (term, other) } else { (other, term) };
            for &parent in sources.iter().chain(other_sources.iter()) {
                self.derive(parent, actual, expected, Step::BoundPropagation);
            }
        }
        Ok(())
    }

    /// Whether a term is a quantified type, directly or as a declaration's
    fn quantified(&self, term: Term) -> Result<bool, Residual> {
        let Term::View(view) = self.resolve(term)? else {
            return Ok(false);
        };
        let ty = match *self.db.ty(view.ty) {
            Type::Decl(id) if !self.db.declaration(id).source.kind.nominal() => {
                self.db.declaration(id).ty
            }
            _ => view.ty,
        };
        Ok(matches!(self.db.ty(ty), Type::Quantified { .. }))
    }

    /// Whether a skolem without a bound is below an expected head that isn't
    /// itself: only top, `Unknown`, or a union with a member that is either
    fn unbounded_skolem(&self, expected: &Head) -> Result<(), Issue> {
        let Head::Structural(view) = expected else {
            return Err(Issue::Contradiction(Contradiction::Rigid));
        };
        let Type::Union(members) = self.db.ty(view.ty) else {
            return Err(Issue::Contradiction(Contradiction::Rigid));
        };
        let mut unresolved = false;
        for member in members.iter() {
            let UnionMember::Type(ty) = *member else {
                unresolved = true;
                continue;
            };
            match self.head(view.child(ty))? {
                Head::Structural(member)
                    if member.ty == self.db.top()
                        || matches!(self.db.ty(member.ty), Type::Unknown(_)) =>
                {
                    return Ok(());
                }
                Head::Infer(_) => unresolved = true,
                _ => {}
            }
        }
        Err(match unresolved {
            true => Residual::Unsupported("a type that may be inside a union member").into(),
            false => Issue::Contradiction(Contradiction::Rigid),
        })
    }

    /// Whether a projection on the left is also a member of the union on the
    /// right, as a projection of a skolem's schema must be to be below it
    fn shares(&self, view: TypeView, member: UnionMember, b: &Head) -> Result<bool, Issue> {
        let Head::Structural(other_view) = b else {
            return Ok(false);
        };
        let Type::Union(others) = self.db.ty(other_view.ty) else {
            return Ok(false);
        };
        let single = |member: UnionMember| self.db.intern(Type::Union(vec![member].into()));
        let left = view.child(single(member));
        for &other in others.iter() {
            if std::mem::discriminant(&member) == std::mem::discriminant(&other)
                && self.same(left, other_view.child(single(other)))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether an item projection on the left is below an item projection of the
    /// same kind and schema on the right: `IndexItem` is monotone in its key,
    /// and `AssignItem` antitone
    fn congruent(&self, view: TypeView, member: UnionMember, b: &Head) -> Result<bool, Issue> {
        let (Some(key), Head::Structural(other_view)) = (member.key(), b) else {
            return Ok(false);
        };
        let Type::Union(others) = self.db.ty(other_view.ty) else {
            return Ok(false);
        };
        for &other in others.iter() {
            let Some(other_key) = other.key() else {
                continue;
            };
            if std::mem::discriminant(&member) != std::mem::discriminant(&other)
                || !self.same(view.child(member.id()), other_view.child(other.id()))?
            {
                continue;
            }
            let (key, other_key) = (
                self.reify(view.child(key))?,
                self.reify(other_view.child(other_key))?,
            );
            let (lower, upper) = match member {
                UnionMember::IndexItem(..) => (key, other_key),
                _ => (other_key, key),
            };
            if self.probe(lower, upper)? == Status::Proven {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A contradiction if a union's projection has a schema whose keyed view has
    /// a key that may be a position's index (see [`Database::promoted`])
    pub(super) fn conflicting(&self, members: &[UnionMember]) -> Result<(), Issue> {
        let conflict =
            (members.iter()).any(|&member| matches!(self.db.project(member), Projected::Conflict));
        match conflict {
            true => Err(Issue::Contradiction(Contradiction::Conflict)),
            false => Ok(()),
        }
    }

    /// Find the expected nominal ancestor and derive argument constraints according to variance.
    fn nominal(
        &self,
        actual: Nominal,
        expected: Nominal,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let Some(actual) = self.ancestor(actual, expected.declaration, &mut HashSet::new(), 0)?
        else {
            return Err(Issue::Contradiction(Contradiction::UnrelatedNominals));
        };
        assert_eq!(actual.arguments.len(), expected.arguments.len());
        if actual.arguments.is_empty() {
            return Ok(());
        }
        let Type::Quantified { binders, .. } =
            self.db.ty(self.db.declaration(expected.declaration).ty)
        else {
            unreachable!()
        };
        for (index, ((a, b), binder)) in actual
            .arguments
            .into_iter()
            .zip(expected.arguments)
            .zip(binders.iter())
            .enumerate()
        {
            let step = |reversed| Step::Argument { index, reversed };
            match binder.variance {
                Variance::Covariant => self.derive(obligation, a, b, step(false)),
                Variance::Contravariant => self.derive(obligation, b, a, step(true)),
                Variance::Invariant => {
                    self.derive(obligation, a, b, step(false));
                    self.derive(obligation, b, a, step(true));
                }
            }
        }
        Ok(())
    }

    /// Derive a contravariant parameter list and a covariant result. The ambient
    /// channels are implicit arguments, so they are contravariant too; `Sink`'s
    /// own contravariance makes the element types written covariant. An omitted
    /// channel stands for its default bound.
    fn functions(
        &self,
        av: TypeView,
        a: &Function,
        bv: TypeView,
        b: &Function,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        self.derive(
            obligation,
            bv.child(b.params),
            av.child(a.params),
            Step::Parameters,
        );
        self.derive(
            obligation,
            av.child(a.result),
            bv.child(b.result),
            Step::Return,
        );
        // A result that selects by a key is exposed, so a key its schema doesn't
        // admit is reported though nothing uses the result
        if let Type::Union(members) = self.db.ty(a.result)
            && members.iter().any(|member| member.key().is_some())
        {
            self.derive(
                obligation,
                self.closed(self.db.bottom()),
                av.child(a.result),
                Step::Return,
            );
        }
        // An omitted channel is gradual
        let channel = |view: TypeView, ty: Option<TypeId>| match ty {
            Some(ty) => view.child(ty),
            None => self.closed(self.db.unknown()),
        };
        if a.input.is_some() || b.input.is_some() {
            self.derive(
                obligation,
                channel(bv, b.input),
                channel(av, a.input),
                Step::Input,
            );
        }
        if a.output.is_some() || b.output.is_some() {
            self.derive(
                obligation,
                channel(bv, b.output),
                channel(av, a.output),
                Step::Output,
            );
        }
        Ok(())
    }

    /// Relate a quantified function to a function type through fresh variables
    /// for its binders, including implicit ambient ones: a call's own channels
    /// bound them from below. The variables are created once per obligation, so
    /// reprocessing it derives the same obligations. They belong to the
    /// obligation's scope, so they may take the skolems it relates.
    fn instantiation(
        &self,
        view: TypeView,
        binders: &[Binder],
        body: TypeId,
        expected: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let environment = self.instantiate_binders(view, binders, &[expected], obligation)?;
        self.derive(
            obligation,
            self.view(body, environment),
            expected,
            Step::Instantiation,
        );
        Ok(())
    }

    /// The environment of fresh variables for a quantified type's binders, each
    /// below its bound, related by `obligation` to `others`
    pub(super) fn instantiate_binders(
        &self,
        view: TypeView,
        binders: &[Binder],
        others: &[Term],
        obligation: ObligationId,
    ) -> Result<EnvironmentId, Issue> {
        let known = self.instantiations.borrow().get(&obligation).copied();
        let environment = match known {
            Some(environment) => environment,
            None => {
                let mut terms = vec![Term::View(view)];
                terms.extend_from_slice(others);
                let scope = self.scope(&terms)?;
                let group: Vec<Term> = binders
                    .iter()
                    .map(|binder| match binder.binding {
                        Binding::Rest(rest) => self.fresh(binder.kind, rest, scope),
                        _ => self.fresh(binder.kind, Rest::All, scope),
                    })
                    .collect();
                for slot in self.db.item_keys(view.ty) {
                    if let Some(&Term::Infer(id)) = group.get(usize::from(slot)) {
                        self.inference[id.0].exact.set(true);
                    }
                }
                let environment = self.intern_environment(view.environment, group.clone());
                for (binder, term) in binders.iter().zip(&group) {
                    if let (Some(default), &Term::Infer(id)) = (binder.default, term) {
                        let default = self.view(default, environment);
                        self.inference[id.0].fallback.set(Some(default));
                    }
                }
                self.instantiations
                    .borrow_mut()
                    .insert(obligation, environment);
                environment
            }
        };
        let group = self
            .environments
            .get_by_index(environment.0)
            .unwrap()
            .group
            .clone();
        for (index, (binder, term)) in binders.iter().zip(group).enumerate() {
            let bound = match (binder.bound, binder.binding) {
                (Some(bound), _) => self.view(bound, environment),
                (None, Binding::Rest(rest)) => self.closed(self.db.rest_shape(rest)),
                (None, _) => continue,
            };
            self.derive(obligation, term, bound, Step::InstantiationBound(index));
        }
        Ok(environment)
    }

    /// Relate a type to a quantified type through a skolem for each of its
    /// binders, bounded by the binder's bound, in a new scope inside the
    /// obligation's. Its body must hold for every choice of binders, so it must
    /// hold for these. The skolems are created once per obligation, so
    /// reprocessing it derives the same obligation.
    pub(super) fn skolemization(
        &self,
        view: TypeView,
        binders: &[Binder],
        body: TypeId,
        actual: Term,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let known = self.skolemizations.borrow().get(&obligation).copied();
        let environment = match known {
            Some(environment) => environment,
            None => {
                let scope = self.enter(self.scope(&[actual, Term::View(view)])?);
                let group: Vec<Term> = (binders.iter())
                    .map(|binder| {
                        let id = SkolemId(self.skolems.len());
                        self.skolems.push(Skolem {
                            kind: binder.kind,
                            binding: binder.binding,
                            bound: Cell::new(None),
                            scope,
                        });
                        Term::Skolem(id)
                    })
                    .collect();
                let environment = self.intern_environment(view.environment, group.clone());
                for (binder, term) in binders.iter().zip(&group) {
                    let bound = match (binder.bound, binder.binding) {
                        (Some(bound), _) => Some(self.view(bound, environment)),
                        (None, Binding::Rest(rest)) => Some(self.closed(self.db.rest_shape(rest))),
                        (None, _) => None,
                    };
                    let &Term::Skolem(id) = term else {
                        unreachable!()
                    };
                    self.skolems[id.0].bound.set(bound);
                }
                self.skolemizations
                    .borrow_mut()
                    .insert(obligation, environment);
                environment
            }
        };
        self.derive(
            obligation,
            actual,
            self.view(body, environment),
            Step::Skolemization,
        );
        Ok(())
    }
}
