//! A deliberately incomplete subtype engine over a sealed declaration database.
//!
//! Views capture immutable substitution environments. Append-only bounds constrain
//! inference variables; separate assignments commit only forced, fully resolved
//! solutions. Assignments wake dependent judgments without rewriting stored terms.
//!
//! Subtype judgments assume well-formed inputs. Callers must establish generic
//! argument bounds and validate declaration bodies and supertypes under their
//! binder assumptions. Nested declarations have their captured binders lambda-lifted
//! and are closed outside their own binder groups. Exposure performs substitution
//! without rechecking bounds or scanning declarations for free references.
//!
//! A declaration is checked by assuming it: its binders become rigids, whose bounds
//! are the only binder bounds taken as facts. Any other declaration's rigid has
//! escaped its check.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
};

use dolang_util::{
    intern,
    mono::{MonoHashMap, MonoHashSet, MonoVec},
};

use crate::typeck::r#type::UnitSpan;

use super::r#type::{
    Argument, Binder, Binding, Database, DeclId, Element, Function, Intrinsic, Kind, Literal,
    Multiplicity, Rest, SchemaItem, Type, TypeId, UnionMember, Variance,
};

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub(crate) struct $name(usize);
    };
}
id!(InferVarId);
id!(EnvironmentId);
id!(ObligationId);
id!(ConstraintId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TypeView {
    pub(crate) ty: TypeId,
    pub(crate) environment: EnvironmentId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Term {
    View(TypeView),
    Infer(InferVarId),
}

impl TypeView {
    fn child(self, ty: TypeId) -> Term {
        Term::View(Self { ty, ..self })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Environment {
    parent: EnvironmentId,
    group: Vec<Term>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Relation {
    pub(crate) actual: Term,
    pub(crate) expected: Term,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Provenance {
    pub(crate) actual: Option<UnitSpan>,
    pub(crate) expected: Option<UnitSpan>,
}

/// A reason a judgment remains unresolved, rather than proven or refuted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Residual {
    /// An inference variable has no committed solution.
    Inference,
    /// No implemented rule handles this combination of exposed type forms.
    Unsupported,
    /// Generic application needs unsupported argument matching or a constructor
    /// that cannot yet be exposed. Also covers unapplied generic declarations.
    GenericArguments,
    /// An intrinsic subtype rule needs a backing type that the database has not registered.
    MissingIntrinsic(Intrinsic),
    /// Function ambient input/output channels differ in presence or cannot be
    /// shown equal by contextual structural comparison.
    AmbientChannels,
    /// A candidate contains a recursive substitution, inheritance revisited a
    /// declaration, or current proof dependencies cycle. None establishes a proof.
    Recursive,
    /// A work or traversal-depth limit prevented completion. Also used for
    /// obligations left pending when solving exhausted its work budget.
    Limit,
    /// A rigid of a declaration this solver does not check has escaped its own check.
    Escape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Contradiction {
    DistinctLiterals,
    UnrelatedNominals,
    Arity,
    /// A rigid is related to something other than itself, and its bound can't show it
    Rigid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Issue {
    Residual(Residual),
    Contradiction(Contradiction),
}

impl From<Residual> for Issue {
    fn from(value: Residual) -> Self {
        Self::Residual(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Argument(usize),
    Parameter(usize),
    Return,
    IntrinsicBacking(Intrinsic),
    BoundPropagation,
    Assignment,
    UnionMember(usize),
    /// A rigid reduced to its written or default bound
    RigidBound,
    /// A rigid reduced to the default bound of an omitted ambient channel
    ImplicitBound,
}

/// Where an ancestor query ends
#[derive(Clone, Debug)]
pub(crate) enum Reach {
    /// The target, with its arguments
    Reached(Vec<Term>),
    Unreached,
    /// The dynamic type, which reaches anything
    Dynamic,
}

#[derive(Clone, Debug)]
pub(crate) struct Dependency {
    pub(crate) obligation: ObligationId,
    pub(crate) step: Step,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Pending,
    Reduced,
    Issue(Issue),
}

pub(crate) struct Obligation {
    pub(crate) relation: Relation,
    pub(crate) dependencies: MonoVec<Dependency>,
    state: Cell<State>,
    queued: Cell<bool>,
    // Child obligation IDs and labeled steps used as current proof premises.
    // Replaced on reprocessing; `dependencies` retains historical edges for diagnostics.
    active: RefCell<Vec<(ObligationId, Step)>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Diagnostic {
    pub(crate) issue: Issue,
    /// Root-to-leaf path; each node retains its relation and reduction metadata.
    pub(crate) path: Vec<ObligationId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Proven,
    Contradicted,
    Unresolved,
}

#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) constraint: ConstraintId,
    pub(crate) status: Status,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug)]
struct Root {
    obligation: ObligationId,
    provenance: Provenance,
}

/// Maps each distinct bound term to the obligations that introduced it. Several
/// obligations can impose the same bound; retain each so derived contradictions
/// remain reachable from every contributing constraint's diagnostic root.
/// Both terms and their source sets grow monotonically, independently of the
/// variable's committed assignment.
type BoundSet = MonoHashMap<Term, MonoHashSet<ObligationId>>;

/// Solver-local resolution and wake-up state, independent of accumulated bounds.
#[derive(Default)]
struct Inference {
    // Solutions are closed canonical types; no solver-local identity can escape.
    assignment: Cell<Option<TypeId>>,
    support: MonoHashSet<ObligationId>,
    subscribers: MonoHashSet<ObligationId>,
    dirty: Cell<bool>,
}

/// Accumulated constraints on one inference variable `V`.
#[derive(Default)]
pub(crate) struct Bounds {
    /// Each key `L` imposes `L <: V`.
    lower: BoundSet,
    /// Each key `U` imposes `V <: U`.
    upper: BoundSet,
}

impl Bounds {
    pub(crate) fn lower(&self) -> impl Iterator<Item = Term> + '_ {
        self.lower.iter().map(|(term, _)| *term)
    }

    pub(crate) fn upper(&self) -> impl Iterator<Item = Term> + '_ {
        self.upper.iter().map(|(term, _)| *term)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) work: usize,
    pub(crate) depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            work: 100_000,
            depth: 256,
        }
    }
}

/// A nominal declaration with interpreted arguments and the environment used
/// to expose its declared supertypes. Declaration identity remains opaque.
#[derive(Clone, Debug)]
struct Nominal {
    declaration: DeclId,
    arguments: Vec<Term>,
    environment: EnvironmentId,
}

/// The outer form of a term after resolving environment references, exposing
/// transparent declarations, and instantiating supported applications. Children
/// remain contextual terms rather than recursively normalized types. Nominal
/// declarations stay opaque; unapplied quantifiers remain structural.
#[derive(Clone, Debug)]
enum Head {
    Infer(InferVarId),
    Structural(TypeView),
    Nominal(Nominal),
}

/// All solver IDs are local to this solver, just as type IDs are database-local.
pub(crate) struct Solver<'db> {
    db: &'db Database,
    environments: intern::Table<Environment, EnvironmentId>,
    bounds: Vec<Bounds>,
    inference: Vec<Inference>,
    // Intern only the relation; processing state and diagnostic edges do not
    // participate in identity and can grow while existing nodes are borrowed.
    obligations: MonoVec<Obligation>,
    obligation_index: MonoHashMap<Relation, ObligationId>,
    queue: MonoVec<ObligationId>,
    roots: Vec<Root>,
    limits: Limits,
    work: Cell<usize>,
    exhausted: Cell<bool>,
    /// The declarations being checked, whose rigids' bounds are assumptions
    scope: HashSet<DeclId>,
    /// Each rigid's bound, once computed
    rigid_bounds: RefCell<HashMap<TypeId, Option<TypeId>>>,
}

impl<'db> Solver<'db> {
    pub(crate) fn new(db: &'db Database) -> Self {
        Self::with_limits(db, Limits::default())
    }

    pub(crate) fn with_limits(db: &'db Database, limits: Limits) -> Self {
        assert!(db.is_sealed(), "solver requires a sealed database");
        let environments = intern::Table::new();
        environments.id_owned(Environment {
            parent: EnvironmentId(0),
            group: vec![],
        });
        Self {
            db,
            environments,
            bounds: Vec::new(),
            inference: Vec::new(),
            obligations: MonoVec::new(),
            obligation_index: MonoHashMap::new(),
            queue: MonoVec::new(),
            roots: Vec::new(),
            limits,
            work: Cell::new(0),
            exhausted: Cell::new(false),
            scope: HashSet::new(),
            rigid_bounds: RefCell::new(HashMap::new()),
        }
    }

    /// Check `decl`: its rigids' bounds become assumptions. The rigids of any other
    /// declaration have escaped their own check.
    pub(crate) fn assume(&mut self, decl: DeclId) {
        self.scope.insert(decl);
    }

    /// Assume `decl`, and return an environment that interprets its group as its
    /// rigids. Its type, supertypes and members viewed there are what is checked.
    pub(crate) fn rigid_environment(&mut self, decl: DeclId) -> EnvironmentId {
        self.assume(decl);
        let group = self
            .db
            .rigids(decl)
            .into_iter()
            .map(|ty| self.closed(ty))
            .collect();
        self.intern_environment(self.empty_environment(), group)
    }

    /// A rigid in scope, and the binder it stands for
    fn rigid(&self, ty: TypeId) -> Result<Option<&Binder>, Residual> {
        let Type::Rigid { decl, slot, .. } = *self.db.ty(ty) else {
            return Ok(None);
        };
        if !self.scope.contains(&decl) {
            return Err(Residual::Escape);
        }
        let Type::Quantified { binders, .. } = self.db.ty(self.db.declaration(decl).ty) else {
            unreachable!("a rigid of a declaration without binders")
        };
        Ok(Some(&binders[usize::from(slot)]))
    }

    /// A rigid's bound, with its declaration's rigids for its group. A rest binder
    /// without one is bounded by its rest mode's shape.
    fn rigid_bound(&self, ty: TypeId) -> Option<TypeId> {
        if let Some(&bound) = self.rigid_bounds.borrow().get(&ty) {
            return bound;
        }
        let Type::Rigid { decl, slot, .. } = *self.db.ty(ty) else {
            unreachable!()
        };
        let Type::Quantified { binders, .. } = self.db.ty(self.db.declaration(decl).ty) else {
            unreachable!("a rigid of a declaration without binders")
        };
        let binder = &binders[usize::from(slot)];
        let bound = match (binder.bound, binder.binding) {
            (Some(bound), _) => Some(self.db.substitute(bound, &self.db.rigids(decl))),
            (None, Binding::Rest(rest)) => {
                let top = self.db.top();
                let key = self
                    .db
                    .intrinsic(Intrinsic::Sym)
                    .unwrap_or_else(|| self.db.unknown());
                let positional = SchemaItem {
                    multiplicity: Multiplicity::Repeated,
                    element: Element::Positional(top),
                };
                let keyed = SchemaItem {
                    multiplicity: Multiplicity::Repeated,
                    element: Element::Keyed { key, value: top },
                };
                let items = match rest {
                    Rest::Positional => vec![positional],
                    Rest::Keyed => vec![keyed],
                    Rest::All => vec![positional, keyed],
                };
                Some(self.db.intern(Type::Schema(items.into())))
            }
            (None, _) => None,
        };
        self.rigid_bounds.borrow_mut().insert(ty, bound);
        bound
    }

    /// Walk a term to the target declaration, carrying substitutions. A rigid in
    /// scope continues through its bound.
    pub(crate) fn reach(&self, mut term: Term, target: DeclId) -> Result<Reach, Issue> {
        for depth in 0.. {
            self.depth(depth)?;
            self.spend()?;
            match self.head(term)? {
                Head::Infer(_) => return Err(Residual::Inference.into()),
                Head::Nominal(nominal) => {
                    return Ok(
                        match self.ancestor(nominal, target, &mut HashSet::new(), 0)? {
                            Some(found) => Reach::Reached(found.arguments),
                            None => Reach::Unreached,
                        },
                    );
                }
                Head::Structural(view) => match self.db.ty(view.ty) {
                    Type::Unknown(_) => return Ok(Reach::Dynamic),
                    Type::Rigid { .. } => {
                        self.rigid(view.ty)?;
                        match self.rigid_bound(view.ty) {
                            Some(bound) => term = self.closed(bound),
                            None => return Ok(Reach::Unreached),
                        }
                    }
                    _ => return Ok(Reach::Unreached),
                },
            }
        }
        unreachable!()
    }

    pub(crate) fn empty_environment(&self) -> EnvironmentId {
        EnvironmentId(0)
    }

    pub(crate) fn environment(&mut self, parent: EnvironmentId, group: Vec<Term>) -> EnvironmentId {
        self.intern_environment(parent, group)
    }

    /// Intern a substitution frame; an empty group introduces no binder depth.
    fn intern_environment(&self, parent: EnvironmentId, group: Vec<Term>) -> EnvironmentId {
        assert!(self.environments.get_by_index(parent.0).is_some());
        for &term in &group {
            self.kind(term);
        }
        if group.is_empty() {
            return parent;
        }
        EnvironmentId(
            self.environments
                .id_owned(Environment { parent, group })
                .index(),
        )
    }

    pub(crate) fn view(&self, ty: TypeId, environment: EnvironmentId) -> Term {
        self.db.ty(ty);
        assert!(self.environments.get_by_index(environment.0).is_some());
        Term::View(TypeView { ty, environment })
    }

    pub(crate) fn closed(&self, ty: TypeId) -> Term {
        self.view(ty, self.empty_environment())
    }

    pub(crate) fn infer(&mut self) -> Term {
        let id = InferVarId(self.bounds.len());
        self.bounds.push(Bounds::default());
        self.inference.push(Inference::default());
        Term::Infer(id)
    }

    /// A committed, fully resolved solution. Bounds remain available independently.
    pub(crate) fn solution(&self, id: InferVarId) -> Option<TypeId> {
        self.inference[id.0].assignment.get()
    }

    /// Obligations that supported the commitment, retained for diagnostic inspection.
    pub(crate) fn solution_sources(
        &self,
        id: InferVarId,
    ) -> impl Iterator<Item = ObligationId> + '_ {
        self.inference[id.0].support.iter().copied()
    }

    pub(crate) fn unresolved(&self) -> impl Iterator<Item = InferVarId> + '_ {
        self.inference
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.assignment.get().is_none().then_some(InferVarId(i)))
    }

    /// Rebuild a closed canonical type, retaining references owned by local binders.
    pub(crate) fn reify(&self, term: Term) -> Result<TypeId, Residual> {
        self.reify_scoped(term, 0, 0)
    }

    fn reify_scoped(&self, term: Term, local: u32, depth: usize) -> Result<TypeId, Residual> {
        self.depth(depth)?;
        self.spend()?;
        match term {
            Term::Infer(id) => self.solution(id).ok_or(Residual::Inference),
            Term::View(view) => {
                let ty = self.db.ty(view.ty);
                if let Type::Bound { reference, kind } = *ty {
                    if u32::from(reference.depth) < local {
                        return Ok(view.ty);
                    }
                    let replacement = self.lookup(
                        view.environment,
                        (u32::from(reference.depth) - local) as u16,
                        reference.slot,
                        kind,
                    );
                    // Replacements carry their own context, not the caller's local scope.
                    return self.reify_scoped(replacement, 0, depth + 1);
                }
                self.rigid(view.ty)?;
                let mapped = ty.map_children(|child, groups| {
                    self.reify_scoped(view.child(child), local + groups, depth + 1)
                })?;
                Ok(self.db.intern(mapped))
            }
        }
    }

    fn variables(
        &self,
        term: Term,
        local: u32,
        depth: usize,
        found: &mut HashSet<InferVarId>,
    ) -> Result<(), Residual> {
        self.depth(depth)?;
        self.spend()?;
        match term {
            Term::Infer(id) => {
                found.insert(id);
            }
            Term::View(view) => {
                let ty = self.db.ty(view.ty);
                if let Type::Bound { reference, kind } = *ty {
                    if u32::from(reference.depth) >= local {
                        let value = self.lookup(
                            view.environment,
                            (u32::from(reference.depth) - local) as u16,
                            reference.slot,
                            kind,
                        );
                        self.variables(value, 0, depth + 1, found)?;
                    }
                } else {
                    let mut children = Vec::new();
                    ty.visit_children(|child, groups| children.push((child, groups)));
                    for (child, groups) in children {
                        self.variables(view.child(child), local + groups, depth + 1, found)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn subscribe(&self, obligation: ObligationId) -> Result<(), Residual> {
        let relation = self.obligation(obligation).relation;
        let mut variables = HashSet::new();
        self.variables(relation.actual, 0, 0, &mut variables)?;
        self.variables(relation.expected, 0, 0, &mut variables)?;
        for variable in variables {
            let bounds = &self.inference[variable.0];
            let _ = bounds.subscribers.try_insert(obligation);
            if bounds.assignment.get().is_some() {
                for &source in bounds.support.iter() {
                    self.link(source, obligation, Step::Assignment);
                }
            }
        }
        Ok(())
    }

    fn schedule(&self, id: ObligationId) {
        if !self.obligation(id).queued.replace(true) {
            self.queue.push(id);
        }
    }

    /// Closed proof queries cannot create inference bounds or leak alternative edges.
    fn probe(&self, actual: TypeId, expected: TypeId) -> Result<Status, Residual> {
        self.spend()?;
        if self.limits.depth <= 1 {
            return Err(Residual::Limit);
        }
        let mut proof = Self::with_limits(
            self.db,
            Limits {
                work: self.limits.work.saturating_sub(self.work.get()),
                depth: self.limits.depth - 1,
            },
        );
        proof.scope = self.scope.clone();
        proof.constrain(
            proof.closed(actual),
            proof.closed(expected),
            Provenance::default(),
        );
        let result = proof.solve().remove(0);
        self.work.set(self.work.get() + proof.work.get());
        if proof.exhausted.get()
            || result
                .diagnostics
                .iter()
                .any(|d| d.issue == Residual::Limit.into())
        {
            return Err(Residual::Limit);
        }
        Ok(result.status)
    }

    /// Follow exact candidate dependencies, without unfolding declaration bodies.
    /// Variable-only cycles are not recursive type substitutions.
    fn occurs(
        &self,
        target: InferVarId,
        term: Term,
        local: u32,
        nested: bool,
        visiting: &mut HashSet<InferVarId>,
        depth: usize,
    ) -> Result<bool, Residual> {
        self.depth(depth)?;
        self.spend()?;
        match term {
            Term::Infer(id) => {
                if id == target {
                    return Ok(nested);
                }
                if self.solution(id).is_some() || !visiting.insert(id) {
                    return Ok(false);
                }
                let bounds = self.bounds(id);
                for lower in bounds.lower() {
                    for upper in bounds.upper() {
                        if self.same(lower, upper)?
                            && self.occurs(target, lower, 0, nested, visiting, depth + 1)?
                        {
                            visiting.remove(&id);
                            return Ok(true);
                        }
                    }
                }
                visiting.remove(&id);
                Ok(false)
            }
            Term::View(view) => {
                let ty = self.db.ty(view.ty);
                if let Type::Bound { reference, kind } = *ty {
                    if u32::from(reference.depth) < local {
                        return Ok(false);
                    }
                    let value = self.lookup(
                        view.environment,
                        (u32::from(reference.depth) - local) as u16,
                        reference.slot,
                        kind,
                    );
                    return self.occurs(target, value, 0, nested, visiting, depth + 1);
                }
                let mut children = Vec::new();
                ty.visit_children(|ty, groups| children.push((ty, groups)));
                for (ty, groups) in children {
                    if self.occurs(
                        target,
                        view.child(ty),
                        local + groups,
                        true,
                        visiting,
                        depth + 1,
                    )? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    fn try_assign(&self, id: InferVarId) -> Result<bool, Residual> {
        let bounds = &self.bounds[id.0];
        let inference = &self.inference[id.0];
        for lower in bounds.lower() {
            for upper in bounds.upper() {
                if self.same(lower, upper)?
                    && self.occurs(id, lower, 0, false, &mut HashSet::new(), 0)?
                {
                    for (_, sources) in bounds.lower.iter().chain(bounds.upper.iter()) {
                        for &source in sources.iter() {
                            let state = &self.obligation(source).state;
                            if matches!(
                                state.get(),
                                State::Issue(Issue::Residual(Residual::Inference))
                            ) {
                                state.set(State::Issue(Residual::Recursive.into()));
                            }
                        }
                    }
                }
            }
        }
        let mut lower = Vec::new();
        let mut upper = Vec::new();
        for (set, values) in [(&bounds.lower, &mut lower), (&bounds.upper, &mut upper)] {
            for (&term, _) in set.iter() {
                match self.reify(term) {
                    Ok(ty) => values.push(ty),
                    Err(Residual::Inference) => {}
                    Err(issue) => return Err(issue),
                }
            }
        }
        // Consistency with the dynamic type is not antisymmetric, so a bound containing
        // it never builds a candidate or forces one. It must still admit the candidate.
        lower.retain(|&ty| !self.contains_unknown(ty));
        if lower.is_empty() || upper.is_empty() {
            return Ok(false);
        }
        let candidate = self.db.intern(Type::Union(
            lower.iter().copied().map(UnionMember::Type).collect(),
        ));
        let mut forced = false;
        for &ty in &upper {
            if self.probe(candidate, ty)? != Status::Proven {
                return Ok(false);
            }
            forced |= !self.contains_unknown(ty) && self.probe(ty, candidate)? == Status::Proven;
        }
        if !forced {
            return Ok(false);
        }
        // Only closed candidates can commit, so substitution cycles cannot be introduced.
        for (_, sources) in bounds.lower.iter().chain(bounds.upper.iter()) {
            for &source in sources.iter() {
                let _ = inference.support.try_insert(source);
            }
        }
        inference.assignment.set(Some(candidate));
        for &obligation in inference.subscribers.iter() {
            self.schedule(obligation);
        }
        // A bound may contain the assigned variable deeply in a contextual view.
        for other in &self.inference {
            if other.assignment.get().is_none() {
                other.dirty.set(true);
            }
        }
        Ok(true)
    }

    pub(crate) fn bounds(&self, id: InferVarId) -> &Bounds {
        &self.bounds[id.0]
    }

    pub(crate) fn obligation(&self, id: ObligationId) -> &Obligation {
        &self.obligations[id.0]
    }

    pub(crate) fn provenance(&self, id: ConstraintId) -> &Provenance {
        &self.roots[id.0].provenance
    }

    /// Check the term IDs and return their kind; inference variables have type kind.
    fn kind(&self, term: Term) -> Kind {
        match term {
            Term::Infer(id) => {
                let _ = &self.bounds[id.0];
                Kind::Type
            }
            Term::View(view) => {
                assert!(self.environments.get_by_index(view.environment.0).is_some());
                self.db.kind(view.ty)
            }
        }
    }

    /// Register a diagnostic root and enqueue its relation, sharing any existing obligation.
    pub(crate) fn constrain(
        &mut self,
        actual: Term,
        expected: Term,
        provenance: Provenance,
    ) -> ConstraintId {
        assert_eq!(
            self.kind(actual),
            self.kind(expected),
            "constraint kind mismatch"
        );
        let obligation = self.enqueue(Relation { actual, expected });
        let id = ConstraintId(self.roots.len());
        self.roots.push(Root {
            obligation,
            provenance,
        });
        id
    }

    /// Intern a relation and queue it only on first insertion.
    fn enqueue(&self, relation: Relation) -> ObligationId {
        if let Some(&id) = self.obligation_index.get(&relation) {
            return id;
        }
        let id = ObligationId(self.obligations.len());
        self.obligations.push(Obligation {
            relation,
            dependencies: MonoVec::new(),
            state: Cell::new(State::Pending),
            queued: Cell::new(true),
            active: RefCell::new(Vec::new()),
        });
        self.obligation_index.try_insert(relation, id).unwrap();
        self.queue.push(id);
        id
    }

    /// Add a child subtype obligation and a labeled dependency from its parent.
    fn derive(&self, parent: ObligationId, actual: Term, expected: Term, step: Step) {
        assert_eq!(
            self.kind(actual),
            self.kind(expected),
            "derived constraint kind mismatch"
        );
        let child = self.enqueue(Relation { actual, expected });
        if step != Step::BoundPropagation && step != Step::Assignment {
            self.obligations[parent.0]
                .active
                .borrow_mut()
                .push((child, step.clone()));
        }
        self.link(parent, child, step);
    }

    fn link(&self, parent: ObligationId, child: ObligationId, step: Step) {
        let dependencies = &self.obligations[parent.0].dependencies;
        if !dependencies
            .iter()
            .any(|d| d.obligation == child && d.step == step)
        {
            dependencies.push(Dependency {
                obligation: child,
                step,
            });
        }
    }

    /// Charge the solver-wide work budget, permanently marking exhaustion on failure.
    fn spend(&self) -> Result<(), Residual> {
        if self.work.get() >= self.limits.work {
            self.exhausted.set(true);
            return Err(Residual::Limit);
        }
        self.work.set(self.work.get() + 1);
        Ok(())
    }

    /// Reject a traversal that reaches the configured recursion limit.
    fn depth(&self, depth: usize) -> Result<(), Residual> {
        if depth >= self.limits.depth {
            Err(Residual::Limit)
        } else {
            Ok(())
        }
    }

    /// Interpret a binder coordinate in its environment, checking the replacement kind.
    fn lookup(&self, mut environment: EnvironmentId, depth: u16, slot: u16, kind: Kind) -> Term {
        for _ in 0..depth {
            assert_ne!(environment.0, 0, "unbound reference");
            environment = self
                .environments
                .get_by_index(environment.0)
                .unwrap()
                .parent;
        }
        assert_ne!(environment.0, 0, "unbound reference");
        let value = self.environments.get_by_index(environment.0).unwrap().group[usize::from(slot)];
        assert_eq!(self.kind(value), kind, "substitution kind mismatch");
        value
    }

    /// Follow root environment substitutions and committed assignments without exposing declarations.
    fn resolve(&self, mut term: Term) -> Result<Term, Residual> {
        for depth in 0.. {
            self.depth(depth)?;
            self.spend()?;
            if let Term::Infer(id) = term {
                if let Some(ty) = self.solution(id) {
                    term = self.closed(ty);
                    continue;
                }
                return Ok(term);
            }
            let Term::View(view) = term else {
                unreachable!()
            };
            let Type::Bound { reference, kind } = *self.db.ty(view.ty) else {
                return Ok(term);
            };
            term = self.lookup(view.environment, reference.depth, reference.slot, kind);
        }
        unreachable!()
    }

    /// Whether an exposed head is the dynamic type or schema
    fn is_unknown(&self, head: &Head) -> bool {
        matches!(head, Head::Structural(view) if matches!(self.db.ty(view.ty), Type::Unknown(_)))
    }

    /// Whether a term resolves to the dynamic type or schema
    fn unknown(&self, term: Term) -> Result<bool, Residual> {
        Ok(match self.resolve(term)? {
            Term::View(view) => matches!(self.db.ty(view.ty), Type::Unknown(_)),
            Term::Infer(_) => false,
        })
    }

    /// Whether a closed type contains the dynamic type or schema anywhere
    fn contains_unknown(&self, ty: TypeId) -> bool {
        let mut found = false;
        self.db.walk(ty, |node, _| {
            found |= matches!(self.db.ty(node), Type::Unknown(_));
        });
        found
    }

    /// Compare structure without substituting into the canonical database. Local
    /// quantifier references stay local; only free references consult a telescope.
    fn same(&self, a: Term, b: Term) -> Result<bool, Residual> {
        self.same_scoped(a, 0, b, 0, 0)
    }

    /// Compare contextual structure while tracking locally bound groups on each side.
    fn same_scoped(
        &self,
        a: Term,
        ad: u32,
        b: Term,
        bd: u32,
        depth: usize,
    ) -> Result<bool, Residual> {
        self.depth(depth)?;
        self.spend()?;
        let a = match a {
            Term::Infer(id) if self.solution(id).is_some() => {
                self.closed(self.solution(id).unwrap())
            }
            _ => a,
        };
        let b = match b {
            Term::Infer(id) if self.solution(id).is_some() => {
                self.closed(self.solution(id).unwrap())
            }
            _ => b,
        };
        for (term, local, other, other_local, left) in [(a, ad, b, bd, true), (b, bd, a, ad, false)]
        {
            if let Term::View(view) = term
                && let Type::Bound { reference, kind } = *self.db.ty(view.ty)
                && u32::from(reference.depth) >= local
            {
                let value = self.lookup(
                    view.environment,
                    (u32::from(reference.depth) - local) as u16,
                    reference.slot,
                    kind,
                );
                return if left {
                    self.same_scoped(value, 0, other, other_local, depth + 1)
                } else {
                    self.same_scoped(other, other_local, value, 0, depth + 1)
                };
            }
        }
        if a == b && ad == bd {
            return Ok(true);
        }
        let (Term::View(a), Term::View(b)) = (a, b) else {
            return Ok(false);
        };
        let at = self.db.ty(a.ty);
        let bt = self.db.ty(b.ty);
        if !at.same_shape(bt) {
            return Ok(false);
        }
        let mut ac = vec![];
        let mut bc = vec![];
        at.visit_children(|ty, groups| ac.push((ty, groups)));
        bt.visit_children(|ty, groups| bc.push((ty, groups)));
        for ((at, ag), (bt, bg)) in ac.into_iter().zip(bc) {
            if !self.same_scoped(a.child(at), ad + ag, b.child(bt), bd + bg, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Substitute fixed positional arguments whose binder bounds the caller has established.
    fn instantiate(
        &self,
        binders: &[Binder],
        arguments: &[Argument],
        view: TypeView,
        parent: EnvironmentId,
    ) -> Result<(Vec<Term>, EnvironmentId), Issue> {
        if binders
            .iter()
            .any(|b| b.kind != Kind::Type || b.binding != Binding::Positional)
            || arguments
                .iter()
                .any(|a| !matches!(a, Argument::Positional(_)))
        {
            return Err(Residual::GenericArguments.into());
        }
        // Every application has been checked by its producer, whether source
        // elaboration or a solver operation, so its arity must already match.
        if arguments.len() < binders.len()
            && binders[arguments.len()..]
                .iter()
                .any(|b| b.default.is_some())
        {
            return Err(Residual::GenericArguments.into());
        }
        assert_eq!(
            binders.len(),
            arguments.len(),
            "generic argument arity mismatch"
        );
        let args: Vec<_> = arguments
            .iter()
            .map(|arg| {
                let Argument::Positional(ty) = *arg else {
                    unreachable!()
                };
                // Resolving keeps environments from nesting through forwarded binders.
                let term = self.resolve(view.child(ty))?;
                assert_eq!(self.kind(term), Kind::Type, "argument kind mismatch");
                Ok(term)
            })
            .collect::<Result<_, Residual>>()?;
        let environment = self.intern_environment(parent, args.clone());
        Ok((args, environment))
    }

    /// Expose the head form of a solver term. Transparent declarations must be
    /// acyclic; well-formedness checking rejects cycles before solving, so
    /// exposing the same declaration or application twice panics.
    fn head(&self, mut term: Term) -> Result<Head, Issue> {
        let mut exposed = HashSet::new();
        for depth in 0.. {
            self.depth(depth)?;
            term = self.resolve(term)?;
            let view = match term {
                Term::View(view) => view,
                Term::Infer(id) => return Ok(Head::Infer(id)),
            };
            match *self.db.ty(view.ty) {
                Type::Decl(id) => {
                    let decl = self.db.declaration(id);
                    if decl.source.kind.nominal() {
                        if matches!(self.db.ty(decl.ty), Type::Quantified { .. }) {
                            return Err(Residual::GenericArguments.into());
                        }
                        return Ok(Head::Nominal(Nominal {
                            declaration: id,
                            arguments: vec![],
                            environment: EnvironmentId(0),
                        }));
                    }
                    // Declarations are closed, so their environment is irrelevant.
                    assert!(
                        exposed.insert(self.closed(view.ty)),
                        "transparent declaration cycle"
                    );
                    term = self.closed(decl.ty);
                }
                Type::Apply { base, ref args, .. } => {
                    assert!(exposed.insert(term), "transparent declaration cycle");
                    // Scoped to this application: applications with different
                    // arguments can legitimately expose the same constructor.
                    let mut constructors = HashSet::new();
                    let mut base = view.child(base);
                    let (nominal, constructor) = loop {
                        self.spend()?;
                        base = self.resolve(base)?;
                        let Term::View(base_view) = base else {
                            return Err(Residual::GenericArguments.into());
                        };
                        if let Type::Decl(id) = *self.db.ty(base_view.ty) {
                            let decl = self.db.declaration(id);
                            let constructor = TypeView {
                                ty: decl.ty,
                                environment: EnvironmentId(0),
                            };
                            if decl.source.kind.nominal() {
                                break (Some(id), constructor);
                            }
                            assert!(constructors.insert(id), "transparent declaration cycle");
                            base = Term::View(constructor);
                        } else {
                            break (None, base_view);
                        }
                    };
                    let Type::Quantified { binders, body } = self.db.ty(constructor.ty) else {
                        panic!("generic argument arity mismatch");
                    };
                    let (arguments, environment) =
                        self.instantiate(binders, args, view, constructor.environment)?;
                    if let Some(declaration) = nominal {
                        return Ok(Head::Nominal(Nominal {
                            declaration,
                            arguments,
                            environment,
                        }));
                    }
                    term = self.view(*body, environment);
                }
                _ => return Ok(Head::Structural(view)),
            }
        }
        unreachable!()
    }

    /// Walk supertypes in MRO order, carrying substitutions through each edge.
    /// A failed first match is final. Earlier incomplete branches also stop the
    /// search: skipping one could choose an ancestor out of MRO order.
    fn ancestor(
        &self,
        current: Nominal,
        target: DeclId,
        path: &mut HashSet<DeclId>,
        depth: usize,
    ) -> Result<Option<Nominal>, Issue> {
        self.depth(depth)?;
        self.spend()?;
        if current.declaration == target {
            return Ok(Some(current));
        }
        if !path.insert(current.declaration) {
            return Err(Residual::Recursive.into());
        }
        let supers = &self.db.declaration(current.declaration).supertypes;
        for &ty in supers.iter() {
            let head = self.head(self.view(ty, current.environment))?;
            let Head::Nominal(next) = head else {
                return Err(Residual::Unsupported.into());
            };
            if let Some(found) = self.ancestor(next, target, path, depth + 1)? {
                return Ok(Some(found));
            }
        }
        path.remove(&current.declaration);
        Ok(None)
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
            match binder.variance {
                Variance::Covariant => self.derive(obligation, a, b, Step::Argument(index)),
                Variance::Contravariant => self.derive(obligation, b, a, Step::Argument(index)),
                Variance::Invariant => {
                    self.derive(obligation, a, b, Step::Argument(index));
                    self.derive(obligation, b, a, Step::Argument(index));
                }
            }
        }
        Ok(())
    }

    /// Expose a parameter schema and extract required positional types.
    fn parameters(&self, view: TypeView, function: &Function) -> Result<Vec<Term>, Issue> {
        let Head::Structural(params) = self.head(view.child(function.params))? else {
            return Err(Residual::Unsupported.into());
        };
        let Type::Schema(items) = self.db.ty(params.ty) else {
            return Err(Residual::Unsupported.into());
        };
        items
            .iter()
            .map(|item| match item.element {
                Element::Positional(ty) if item.multiplicity == Multiplicity::Required => {
                    Ok(params.child(ty))
                }
                _ => Err(Residual::Unsupported.into()),
            })
            .collect()
    }

    /// Derive contravariant parameter and covariant result obligations; check ambient channels.
    fn functions(
        &self,
        av: TypeView,
        a: &Function,
        bv: TypeView,
        b: &Function,
        obligation: ObligationId,
    ) -> Result<(), Issue> {
        let ap = self.parameters(av, a)?;
        let bp = self.parameters(bv, b)?;
        if ap.len() != bp.len() {
            return Err(Issue::Contradiction(Contradiction::Arity));
        }
        for (index, (a, b)) in ap.into_iter().zip(bp).enumerate() {
            self.derive(obligation, b, a, Step::Parameter(index));
        }
        self.derive(
            obligation,
            av.child(a.result),
            bv.child(b.result),
            Step::Return,
        );
        for (a, b) in [(a.input, b.input), (a.output, b.output)] {
            match (a, b) {
                (None, None) => {}
                (Some(a), Some(b))
                    if self.same(av.child(a), bv.child(b))?
                        || self.unknown(av.child(a))?
                        || self.unknown(bv.child(b))? => {}
                _ => return Err(Residual::AmbientChannels.into()),
            }
        }
        Ok(())
    }

    /// Reduce one relation, recording bounds or child obligations, or return a diagnostic issue.
    /// Success means local reduction succeeded; child obligations may still fail or remain unresolved.
    fn reduce(&self, obligation: ObligationId) -> Result<(), Issue> {
        let Relation { actual, expected } = self.obligations[obligation.0].relation;
        for (term, other, lower) in [(actual, expected, false), (expected, actual, true)] {
            let mut term = term;
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
                    Term::View(view) => {
                        let Type::Bound { reference, kind } = *self.db.ty(view.ty) else {
                            break;
                        };
                        term = self.lookup(view.environment, reference.depth, reference.slot, kind);
                    }
                }
            }
        }
        let a = self.head(actual)?;
        let b = self.head(expected)?;
        // The dynamic type or schema is consistent with anything of its kind
        if [&a, &b].into_iter().any(|head| self.is_unknown(head)) {
            return Ok(());
        }
        if let Head::Structural(view) = &b
            && view.ty == self.db.top()
            && self.kind(actual) == Kind::Type
        {
            return Ok(());
        }
        if let Head::Structural(view) = &a
            && view.ty == self.db.bottom()
            && self.kind(expected) == Kind::Type
        {
            return Ok(());
        }
        if let (Head::Structural(a), Head::Structural(b)) = (&a, &b)
            && self.same(Term::View(*a), Term::View(*b))?
        {
            return Ok(());
        }
        // Past identity, a rigid of a declaration not being checked has escaped
        for head in [&a, &b] {
            if let Head::Structural(view) = head {
                self.rigid(view.ty)?;
            }
        }
        if !matches!(a, Head::Infer(_))
            && let Head::Structural(view) = &b
            && let Type::Union(members) = self.db.ty(view.ty)
        {
            for member in members.iter() {
                if let UnionMember::Type(ty) = *member
                    && self.same(actual, view.child(ty))?
                {
                    return Ok(());
                }
            }
        }
        // A rigid is below whatever its bound is below
        if !matches!(b, Head::Infer(_))
            && let Head::Structural(view) = &a
            && let Some(binder) = self.rigid(view.ty)?
        {
            let step = match binder.binding {
                Binding::Implicit => Step::ImplicitBound,
                _ => Step::RigidBound,
            };
            match self.rigid_bound(view.ty) {
                Some(bound) => {
                    self.derive(obligation, self.closed(bound), expected, step);
                    return Ok(());
                }
                // A union may still have a member that admits anything
                None if matches!(&b, Head::Structural(view)
                    if matches!(self.db.ty(view.ty), Type::Union(_))) => {}
                None => return Err(Issue::Contradiction(Contradiction::Rigid)),
            }
        }
        if !matches!(b, Head::Infer(_))
            && let Head::Structural(view) = &a
            && let Type::Union(members) = self.db.ty(view.ty)
        {
            if members.iter().any(|m| matches!(m, UnionMember::Expand(_))) {
                return Err(Residual::Unsupported.into());
            }
            for (index, member) in members.iter().enumerate() {
                let UnionMember::Type(ty) = *member else {
                    unreachable!()
                };
                self.derive(
                    obligation,
                    view.child(ty),
                    expected,
                    Step::UnionMember(index),
                );
            }
            return Ok(());
        }
        // Only itself, bottom and the dynamic type are below a rigid
        if !matches!(a, Head::Infer(_))
            && let Head::Structural(view) = &b
            && self.rigid(view.ty)?.is_some()
        {
            return Err(Issue::Contradiction(Contradiction::Rigid));
        }
        if !matches!(a, Head::Infer(_))
            && let Head::Structural(view) = &b
            && let Type::Union(members) = self.db.ty(view.ty)
        {
            // Testing alternatives must never add bounds to this solver.
            let actual = self.reify(actual)?;
            for member in members.iter() {
                if let UnionMember::Type(ty) = *member
                    && let Ok(expected) = self.reify(view.child(ty))
                    && self.probe(actual, expected)? == Status::Proven
                {
                    return Ok(());
                }
            }
            return Err(Residual::Unsupported.into());
        }
        match (a, b) {
            (Head::Infer(a), Head::Infer(b)) if a == b => Ok(()),
            (Head::Infer(a), Head::Infer(b)) => {
                self.add_bound(a, Term::Infer(b), false, obligation)?;
                self.add_bound(b, Term::Infer(a), true, obligation)?;
                Err(Residual::Inference.into())
            }
            (Head::Infer(id), _) => {
                self.add_bound(id, expected, false, obligation)?;
                Err(Residual::Inference.into())
            }
            (_, Head::Infer(id)) => {
                self.add_bound(id, actual, true, obligation)?;
                Err(Residual::Inference.into())
            }
            (Head::Nominal(a), Head::Nominal(b)) => self.nominal(a, b, obligation),
            (Head::Structural(a), Head::Structural(b)) => {
                if self.same(Term::View(a), Term::View(b))? {
                    return Ok(());
                }
                match (self.db.ty(a.ty), self.db.ty(b.ty)) {
                    (Type::Literal(_), Type::Literal(_)) => {
                        Err(Issue::Contradiction(Contradiction::DistinctLiterals))
                    }
                    (Type::Function(a_func), Type::Function(b_func)) => {
                        self.functions(a, a_func, b, b_func, obligation)
                    }
                    _ => Err(Residual::Unsupported.into()),
                }
            }
            (Head::Structural(view), Head::Nominal(_)) => {
                // Quantifiers preserve function membership without requiring
                // instantiation or higher-rank comparison of the signature.
                let mut ty = view.ty;
                while let Type::Quantified { body, .. } = self.db.ty(ty) {
                    self.spend()?;
                    ty = *body;
                }
                let intrinsic = match self.db.ty(view.ty) {
                    _ if matches!(self.db.ty(ty), Type::Function(_)) => Intrinsic::Func,
                    Type::Literal(Literal::Nil) => Intrinsic::Nil,
                    Type::Literal(Literal::Bool(_)) => Intrinsic::Bool,
                    Type::Literal(Literal::Int(_)) => Intrinsic::Int,
                    Type::Literal(Literal::Str(_)) => Intrinsic::Str,
                    Type::Literal(Literal::Sym(_)) => Intrinsic::Sym,
                    _ => return Err(Residual::Unsupported.into()),
                };
                let Some(backing) = self.db.intrinsic(intrinsic) else {
                    return Err(Residual::MissingIntrinsic(intrinsic).into());
                };
                self.derive(
                    obligation,
                    self.closed(backing),
                    expected,
                    Step::IntrinsicBacking(intrinsic),
                );
                Ok(())
            }
            _ => Err(Residual::Unsupported.into()),
        }
    }

    /// Every new bound is paired with the opposite bounds. Variable-to-variable
    /// bounds use this same rule; derived obligations carry propagation onward.
    fn add_bound(
        &self,
        id: InferVarId,
        term: Term,
        lower: bool,
        source: ObligationId,
    ) -> Result<(), Residual> {
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

    /// Process queued obligations to quiescence or exhaustion and report each submitted root.
    pub(crate) fn solve(&mut self) -> Vec<Outcome> {
        loop {
            while !self.exhausted.get() && !self.queue.is_empty() {
                let mut batch = std::mem::take(&mut self.queue);
                for id in batch.drain() {
                    let node = &self.obligations[id.0];
                    node.queued.set(false);
                    node.active.borrow_mut().clear();
                    let result = self
                        .spend()
                        .map_err(Issue::from)
                        .and_then(|()| self.subscribe(id).map_err(Issue::from))
                        .and_then(|()| self.reduce(id));
                    node.state.set(match result {
                        Ok(()) => State::Reduced,
                        Err(issue) => State::Issue(issue),
                    });
                    if self.exhausted.get() {
                        break;
                    }
                }
            }
            if self.exhausted.get() {
                break;
            }
            let mut assigned = false;
            for index in 0..self.bounds.len() {
                if self.inference[index].dirty.replace(false)
                    && self.inference[index].assignment.get().is_none()
                {
                    match self.try_assign(InferVarId(index)) {
                        Ok(changed) => assigned |= changed,
                        Err(Residual::Limit) => {
                            self.exhausted.set(true);
                            break;
                        }
                        Err(_) => {}
                    }
                }
            }
            if !assigned && self.queue.is_empty() {
                break;
            }
        }
        (0..self.roots.len())
            .map(|index| self.outcome(ConstraintId(index)))
            .collect()
    }

    /// Trace a root through its dependencies to collect issues and determine its status.
    fn outcome(&self, constraint: ConstraintId) -> Outcome {
        let root = self.roots[constraint.0].obligation;
        let mut diagnostics = vec![];
        // Iterative DFS keeps reporting safe even when the obligation graph is deep.
        let mut stack = vec![(root, false)];
        let mut active = HashSet::new();
        let mut visited = HashSet::new();
        let mut path = vec![];
        while let Some((id, exit)) = stack.pop() {
            if exit {
                active.remove(&id);
                path.pop();
                continue;
            }
            if active.contains(&id) {
                let mut cycle = path.clone();
                cycle.push(id);
                diagnostics.push(Diagnostic {
                    issue: Residual::Recursive.into(),
                    path: cycle,
                });
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            active.insert(id);
            path.push(id);
            match self.obligations[id.0].state.get() {
                State::Pending => diagnostics.push(Diagnostic {
                    issue: Residual::Limit.into(),
                    path: path.clone(),
                }),
                State::Issue(issue) => diagnostics.push(Diagnostic {
                    issue,
                    path: path.clone(),
                }),
                State::Reduced => {}
            }
            stack.push((id, true));
            for dependency in self.obligations[id.0].dependencies.iter().filter(|d| {
                self.obligations[id.0]
                    .active
                    .borrow()
                    .contains(&(d.obligation, d.step.clone()))
            }) {
                stack.push((dependency.obligation, false));
            }
        }
        // Historical edges explain contradictions, but are not current proof premises.
        let mut history = vec![(root, vec![root])];
        let mut seen = HashSet::new();
        while let Some((id, path)) = history.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let State::Issue(issue @ Issue::Contradiction(_)) =
                self.obligations[id.0].state.get()
                && !diagnostics
                    .iter()
                    .any(|d| d.path.last() == Some(&id) && d.issue == issue)
            {
                diagnostics.push(Diagnostic {
                    issue,
                    path: path.clone(),
                });
            }
            for dependency in self.obligations[id.0].dependencies.iter() {
                let mut next = path.clone();
                next.push(dependency.obligation);
                history.push((dependency.obligation, next));
            }
        }
        if self.exhausted.get() {
            diagnostics.push(Diagnostic {
                issue: Residual::Limit.into(),
                path: vec![root],
            });
        }
        let status = if diagnostics
            .iter()
            .any(|d| matches!(d.issue, Issue::Contradiction(_)))
        {
            Status::Contradicted
        } else if diagnostics.is_empty() {
            Status::Proven
        } else {
            Status::Unresolved
        };
        Outcome {
            constraint,
            status,
            diagnostics,
        }
    }
}

#[cfg(test)]
mod tests;
