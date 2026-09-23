//! A deliberately incomplete subtype engine over a sealed declaration database.
//!
//! Views capture immutable substitution environments. Bounds constrain existential
//! inference variables, but this engine does not choose or reify their solutions.
//! A report with retained bounds is therefore unresolved, not a proof.
//!
//! Subtype judgments assume well-formed inputs. Callers must establish generic
//! argument bounds and validate declaration bodies and supertypes under their
//! binder assumptions. Nested declarations have their captured binders lambda-lifted
//! and are closed outside their own binder groups. Exposure performs substitution
//! without rechecking bounds or scanning declarations for free references.

use std::{cell::Cell, collections::HashSet};

use dolang_util::{
    intern,
    mono::{MonoHashMap, MonoHashSet, MonoVec},
};

use super::r#type::{
    Argument, Binder, Binding, Database, DeclId, Element, Function, Intrinsic, Kind, Literal,
    Multiplicity, SourceSpan, Type, TypeId, Variance,
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
    pub(crate) actual: Option<SourceSpan>,
    pub(crate) expected: Option<SourceSpan>,
}

/// A reason a judgment remains unresolved, rather than proven or refuted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Residual {
    /// Constraints were recorded on inference variables, but no solution was chosen.
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
    /// Inheritance traversal revisited a declaration, or reporting found a cycle
    /// in obligation dependencies. Neither cycle is accepted as a proof.
    Recursive,
    /// A work or traversal-depth limit prevented completion. Also used for
    /// obligations left pending when solving exhausted its work budget.
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Contradiction {
    DistinctLiterals,
    UnrelatedNominals,
    Arity,
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
/// Both terms and their source sets grow monotonically, without choosing or
/// simplifying a solution for the variable.
type BoundSet = MonoHashMap<Term, MonoHashSet<ObligationId>>;

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
    // Intern only the relation; processing state and diagnostic edges do not
    // participate in identity and can grow while existing nodes are borrowed.
    obligations: MonoVec<Obligation>,
    obligation_index: MonoHashMap<Relation, ObligationId>,
    queue: MonoVec<ObligationId>,
    roots: Vec<Root>,
    limits: Limits,
    work: Cell<usize>,
    exhausted: Cell<bool>,
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
            obligations: MonoVec::new(),
            obligation_index: MonoHashMap::new(),
            queue: MonoVec::new(),
            roots: Vec::new(),
            limits,
            work: Cell::new(0),
            exhausted: Cell::new(false),
        }
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
        Term::Infer(id)
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

    /// Follow environment substitutions at the root without exposing declarations.
    fn resolve(&self, mut term: Term) -> Result<Term, Residual> {
        for depth in 0.. {
            self.depth(depth)?;
            self.spend()?;
            let Term::View(view) = term else {
                return Ok(term);
            };
            let Type::Bound { reference, kind } = *self.db.ty(view.ty) else {
                return Ok(term);
            };
            term = self.lookup(view.environment, reference.depth, reference.slot, kind);
        }
        unreachable!()
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
                (Some(a), Some(b)) if self.same(av.child(a), bv.child(b))? => {}
                _ => return Err(Residual::AmbientChannels.into()),
            }
        }
        Ok(())
    }

    /// Reduce one relation, recording bounds or child obligations, or return a diagnostic issue.
    /// Success means local reduction succeeded; child obligations may still fail or remain unresolved.
    fn reduce(&self, obligation: ObligationId) -> Result<(), Issue> {
        let Relation { actual, expected } = self.obligations[obligation.0].relation;
        let a = self.head(actual)?;
        let b = self.head(expected)?;
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
        while !self.exhausted.get() && !self.queue.is_empty() {
            // Detach the current batch so reduction can append through &self.
            // Newly discovered obligations run after the rest of this batch.
            let mut batch = std::mem::take(&mut self.queue);
            for id in batch.drain() {
                let result = self
                    .spend()
                    .map_err(Issue::from)
                    .and_then(|()| self.reduce(id));
                self.obligations[id.0].state.set(match result {
                    Ok(()) => State::Reduced,
                    Err(issue) => State::Issue(issue),
                });
                if self.exhausted.get() {
                    break;
                }
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
            for dependency in self.obligations[id.0].dependencies.iter() {
                stack.push((dependency.obligation, false));
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
