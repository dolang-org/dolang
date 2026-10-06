//! A deliberately incomplete subtype engine over a sealed declaration database.
//!
//! Views capture immutable substitution environments. Append-only bounds constrain
//! inference variables; separate assignments commit fully resolved solutions,
//! either forced or defaulted at the caller's request. Assignments wake dependent
//! judgments without rewriting stored terms.
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
    Argument, Binder, Binding, BoundRef, Database, DeclId, DeclKind, Element, Function, Intrinsic,
    Kind, Literal, MemberKey, Multiplicity, Promotion, Rest, SchemaItem, SymbolId, Type, TypeId,
    UnionMember, Variance,
};

/// A member of a generic class's object: its class, the class a private one is
/// looked up through, and its key
type GenericMember = (DeclId, Option<DeclId>, MemberKey);

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
id!(SkolemId);
id!(ScopeId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TypeView {
    pub(crate) ty: TypeId,
    pub(crate) environment: EnvironmentId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Term {
    View(TypeView),
    Infer(InferVarId),
    /// A binder of a quantified type on the right of a judgment, held abstract
    /// while its body is related. It never enters a canonical type.
    Skolem(SkolemId),
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
    /// Whether schemas relate as the items a value holds, which may fill the
    /// expected side's multiplicities any way that fits, rather than as
    /// arguments bound to a parameter list by count
    pub(crate) language: bool,
}

/// An argument of a call, by how it is passed
#[derive(Clone, Copy, Debug)]
pub(crate) enum CallArgument {
    Positional(Term),
    Keyword(SymbolId, Term),
    /// A value by a key of the first type
    Pair(Term, Term),
    /// The schema of a spread value's items
    Spread(Term),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Provenance {
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    pub(crate) actual: Option<UnitSpan>,
    #[expect(dead_code, reason = "diagnostics don't cite provenance yet")]
    pub(crate) expected: Option<UnitSpan>,
}

/// A reason a judgment remains unresolved, rather than proven or refuted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Residual {
    /// An inference variable has no committed solution.
    Inference,
    /// No implemented rule handles this combination of exposed type forms. It
    /// names what wasn't handled.
    Unsupported(&'static str),
    /// Generic application needs unsupported argument matching or a constructor
    /// that cannot yet be exposed. Also covers unapplied generic declarations.
    GenericArguments,
    /// An intrinsic subtype rule needs a backing type that the database has not registered.
    MissingIntrinsic(Intrinsic),
    /// A candidate contains a recursive substitution, inheritance revisited a
    /// declaration, or current proof dependencies cycle. None establishes a proof.
    Recursive,
    /// A work or traversal-depth limit prevented completion. Also used for
    /// obligations left pending when solving exhausted its work budget.
    Limit,
    /// A rigid of a declaration this solver does not check has escaped its own check.
    Escape,
    /// Positional schema items can't be matched up by count: several expected
    /// items repeat, or an opaque schema precedes items of varying count.
    Alignment,
    /// Several alternatives of a judgment that needs one remain possible, so
    /// none is chosen (see [`Solver::trial`]).
    Ambiguous,
}

/// What relating a [`Type::Unsupported`] stand-in is
const UNREPRESENTED: Residual = Residual::Unsupported("a type the checker can't represent yet");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Contradiction {
    DistinctLiterals,
    UnrelatedNominals,
    /// A rigid or skolem is related to something other than itself, and its bound
    /// can't show it
    Rigid,
    /// The actual schema's item can be more than the expected schema admits
    Excess(usize),
    /// The expected schema's item can be missing from the actual schema
    Missing(usize),
    /// A literal or concrete class has a value outside every union member
    Outside,
    /// A projection's schema has a key that may be a position's index
    Conflict,
    /// An item projection's key has a member its schema doesn't admit
    Unadmitted(TypeId),
    /// A mapping's packs have items that don't correspond, by count,
    /// multiplicity or form
    MappedPacks,
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
    /// A type argument by its index, relating the parent's expected argument
    /// below its actual one if `reversed`, as for a contravariant binder
    Argument {
        index: usize,
        reversed: bool,
    },
    /// A function's parameter list, related contravariantly
    Parameters,
    Return,
    /// A function's ambient input channel, related contravariantly
    Input,
    /// A function's ambient output channel, related contravariantly
    Output,
    IntrinsicBacking(Intrinsic),
    BoundPropagation,
    Assignment,
    UnionMember(usize),
    /// A rigid reduced to its written or default bound
    RigidBound,
    /// A rigid reduced to the default bound of an omitted ambient channel
    ImplicitBound,
    /// A schema item's positional type, keyed value or included schema
    Item(usize),
    /// A schema item's key
    Key(usize),
    /// A quantified type's body under fresh variables for its binders
    Instantiation,
    /// A fresh variable below its binder's bound
    InstantiationBound(usize),
    /// A quantified type's body under skolems for its binders
    Skolemization,
    /// A skolem reduced to its binder's bound
    SkolemBound,
    /// A skolem outside a variable's scope replaced by its bound, as the
    /// variable's lower bound
    Promotion,
    /// A callable value's signature by its index among those it's called with,
    /// below a function type. A contradiction under it leaves the judgment
    /// unresolved instead, since a subclass may fit (see [`callable`]).
    Callable(usize),
}

impl Step {
    /// Whether the child relates the parent's expected side below its actual
    /// side, as a contravariant position does
    pub(crate) fn reverses(&self) -> bool {
        matches!(
            self,
            Step::Argument { reversed: true, .. } | Step::Parameters | Step::Input | Step::Output
        )
    }
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

#[derive(Clone)]
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
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    pub(crate) constraint: ConstraintId,
    pub(crate) status: Status,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug)]
struct Root {
    obligation: ObligationId,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    provenance: Provenance,
}

/// Maps each distinct bound term to the obligations that introduced it. Several
/// obligations can impose the same bound; retain each so derived contradictions
/// remain reachable from every contributing constraint's diagnostic root.
/// Both terms and their source sets grow monotonically, independently of the
/// variable's committed assignment.
type BoundSet = MonoHashMap<Term, MonoHashSet<ObligationId>>;

/// Solver-local resolution and wake-up state, independent of accumulated bounds.
#[derive(Clone)]
struct Inference {
    kind: Kind,
    /// For a schema variable, the lanes its items can occupy
    lanes: Rest,
    /// The solution: a closed type, or a term holding skolems of the scopes it
    /// sees. Either way it holds no unsolved variable, so assignments can't cycle.
    assignment: Cell<Option<Term>>,
    /// Whether the assignment is a default rather than forced
    defaulted: Cell<bool>,
    /// Whether a default keeps its literals, as an item projection's key does
    exact: Cell<bool>,
    /// The default of the binder it instantiates, which it takes when nothing
    /// bounds it from below
    fallback: Cell<Option<Term>>,
    support: MonoHashSet<ObligationId>,
    subscribers: MonoHashSet<ObligationId>,
    dirty: Cell<bool>,
    /// The scope it was created in, which bounds the skolems it may take
    scope: ScopeId,
}

/// A binder of a quantified type on the right, held abstract while its body is
/// related (see [`Solver::skolemization`])
#[derive(Clone)]
struct Skolem {
    kind: Kind,
    binding: Binding,
    /// The binder's bound in the skolemization's environment, or a rest binder's
    /// shape. Set once the environment exists.
    bound: Cell<Option<Term>>,
    scope: ScopeId,
}

/// A skolemization's extent: its skolems are visible to variables of this scope
/// and the scopes inside it. The root scope has no skolems.
#[derive(Clone)]
struct Scope {
    parent: ScopeId,
    depth: usize,
}

/// Accumulated constraints on one inference variable `V`.
#[derive(Default, Clone)]
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

/// What an MRO walk visits
enum Visited<'a> {
    Nominal(&'a Nominal),
    /// A supertype that isn't nominal
    Structural,
}

/// The outer form of a term after resolving environment references, exposing
/// transparent declarations, and instantiating supported applications. Children
/// remain contextual terms rather than recursively normalized types. Nominal
/// declarations stay opaque; unapplied quantifiers remain structural.
#[derive(Clone, Debug)]
enum Head {
    Infer(InferVarId),
    Skolem(SkolemId),
    Structural(TypeView),
    Nominal(Nominal),
}

/// All solver IDs are local to this solver, just as type IDs are database-local.
/// A clone is a fork: a trial on it can't affect the original (see
/// [`Solver::trial`]).
#[derive(Clone)]
pub(crate) struct Solver<'db> {
    db: &'db Database,
    environments: intern::Table<Environment, EnvironmentId>,
    bounds: MonoVec<Bounds>,
    inference: MonoVec<Inference>,
    /// The environment each quantifier instantiation created, by the obligation
    /// that instantiated it, so reprocessing reuses its variables
    instantiations: RefCell<HashMap<ObligationId, EnvironmentId>>,
    /// The environment of skolems each skolemization created, by the obligation
    /// that skolemized, so reprocessing reuses its skolems
    skolemizations: RefCell<HashMap<ObligationId, EnvironmentId>>,
    skolems: MonoVec<Skolem>,
    scopes: MonoVec<Scope>,
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
    /// Whether the judgments own the root scope's variables, so solving settles
    /// them as it settles a skolem scope's
    closed: bool,
    /// The judgments that need one of their alternatives to hold, by obligation
    alternatives: RefCell<HashMap<ObligationId, Alternatives>>,
    /// How each class's object is called, once found
    constructors: RefCell<HashMap<DeclId, Constructor>>,
    /// Each member of a generic class's object, once found
    generic_members: RefCell<HashMap<GenericMember, Result<Lookup, Issue>>>,
    /// Counts changes to bounds and assignments, so trials rerun only after
    /// what they saw has grown
    generation: Cell<usize>,
    /// How many trials this solver is nested in. A solver nested too deeply
    /// leaves its own alternatives untried.
    trial_depth: usize,
    /// What trials judge in place of a term, as a `do` block whose result
    /// mustn't choose an alternative (see [`Solver::blind`])
    blinded: HashMap<Term, Term>,
    /// How traces render a type (see [`Solver::named`])
    #[cfg(feature = "debug")]
    names: Option<std::rc::Rc<dyn Fn(TypeId) -> String + 'db>>,
    /// How many trials and side queries the solver is nested in, which indents
    /// its traces
    #[cfg(feature = "debug")]
    indent: usize,
}

/// Trace under `typeck.solver`, indented by how deeply the solver is nested
macro_rules! trace {
    ($solver:expr, $($arg:tt)*) => {
        dolang_util::debug_eprintln!(
            topic: "typeck.solver",
            "{:indent$}{}",
            "",
            format_args!($($arg)*),
            indent = 2 * $solver.indent
        )
    };
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
        let scopes = MonoVec::new();
        scopes.push(Scope {
            parent: ScopeId(0),
            depth: 0,
        });
        Self {
            db,
            environments,
            bounds: MonoVec::new(),
            inference: MonoVec::new(),
            instantiations: RefCell::new(HashMap::new()),
            skolemizations: RefCell::new(HashMap::new()),
            skolems: MonoVec::new(),
            scopes,
            obligations: MonoVec::new(),
            obligation_index: MonoHashMap::new(),
            queue: MonoVec::new(),
            roots: Vec::new(),
            limits,
            work: Cell::new(0),
            exhausted: Cell::new(false),
            scope: HashSet::new(),
            rigid_bounds: RefCell::new(HashMap::new()),
            closed: false,
            alternatives: RefCell::new(HashMap::new()),
            constructors: RefCell::new(HashMap::new()),
            generic_members: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
            trial_depth: 0,
            blinded: HashMap::new(),
            #[cfg(feature = "debug")]
            names: None,
            #[cfg(feature = "debug")]
            indent: 0,
        }
    }

    /// Render types in traces with `names`, if `typeck.solver` is traced
    #[cfg(feature = "debug")]
    pub(crate) fn named(&mut self, names: impl Fn(TypeId) -> String + 'db) {
        if dolang_util::debug_enabled!("typeck.solver") {
            self.names = Some(std::rc::Rc::new(names));
        }
    }

    /// A term for a trace: its type, if it reifies, and otherwise its type with
    /// its environment's group, `?n` for an unsolved variable, or `!n` for a skolem
    #[cfg(feature = "debug")]
    fn render(&self, term: Term) -> String {
        match term {
            Term::Infer(id) => match self.assignment(id) {
                Some(assigned) => self.render(assigned),
                None => format!("?{}", id.0),
            },
            Term::Skolem(id) => format!("!{}", id.0),
            Term::View(view) => {
                let Some(names) = &self.names else {
                    return format!("{view:?}");
                };
                // Rendering mustn't spend the work it limits
                let (work, exhausted) = (self.work.get(), self.exhausted.get());
                let reified = self.reify(term);
                self.work.set(work);
                self.exhausted.set(exhausted);
                if let Ok(ty) = reified {
                    return names(ty);
                }
                let group = (self.environments.get_by_index(view.environment.0))
                    .map(|environment| environment.group.clone())
                    .unwrap_or_default();
                let group: Vec<String> = group.into_iter().map(|term| self.render(term)).collect();
                format!("{} with [{}]", names(view.ty), group.join(", "))
            }
        }
    }

    /// Declare that no caller will default the root scope's variables, as in a
    /// judgment between two declarations' types. Solving then settles them after
    /// every skolem scope's, as it settles those.
    pub(crate) fn close(&mut self) {
        self.closed = true;
    }

    /// Check `decl`: its rigids' bounds become assumptions. The rigids of any other
    /// declaration have escaped their own check.
    pub(crate) fn assume(&mut self, decl: DeclId) {
        self.scope.insert(decl);
        self.constructors.get_mut().clear();
        self.generic_members.get_mut().clear();
    }

    /// A solver for a side query about `class`, its rigids assumed
    pub(super) fn side_query(&self, class: DeclId) -> Solver<'db> {
        let mut solver = Solver::new(self.db);
        solver.scope = self.scope.clone();
        solver.assume(class);
        #[cfg(feature = "debug")]
        {
            solver.names = self.names.clone();
        }
        solver
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
        let bound = self.db.binder_bound(binder, &self.db.rigids(decl));
        self.rigid_bounds.borrow_mut().insert(ty, bound);
        bound
    }

    /// Walk a term to the target declaration, carrying substitutions. A rigid in
    /// scope continues through its bound.
    pub(crate) fn reach(&self, term: Term, target: DeclId) -> Result<Reach, Issue> {
        self.reach_in(term, target, false)
    }

    /// Walk a term to the target declaration as [`Self::reach`] does, but only
    /// through the supertypes the runtime inherits from, so a class claiming a
    /// protocol reaches only what it inherits
    pub(crate) fn inherits(&self, term: Term, target: DeclId) -> Result<Reach, Issue> {
        self.reach_in(term, target, true)
    }

    fn reach_in(&self, mut term: Term, target: DeclId, runtime: bool) -> Result<Reach, Issue> {
        for depth in 0.. {
            self.depth(depth)?;
            self.spend()?;
            match self.head(term)? {
                Head::Infer(_) => return Err(Residual::Inference.into()),
                Head::Skolem(id) => match self.skolems[id.0].bound.get() {
                    Some(bound) => term = bound,
                    None => return Ok(Reach::Unreached),
                },
                Head::Nominal(nominal) => {
                    return Ok(
                        match self.ancestor_in(nominal, target, runtime, &mut HashSet::new(), 0)? {
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

    /// The function type a call expects of its callee: `(args) <input >output ->
    /// result`. Constraining the callee's type below it checks the call.
    /// Contradictions and derivations under the parameter list name an argument
    /// by its index in `args`, through [`Step::Item`] and [`Step::Key`].
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn call(
        &self,
        args: &[CallArgument],
        result: Term,
        input: Option<Term>,
        output: Option<Term>,
    ) -> Term {
        let args: Vec<_> = (args.iter())
            .map(|&arg| (Multiplicity::Required, arg))
            .collect();
        self.call_items(&args, result, input, output)
    }

    /// [`Solver::call`] with arguments that may be passed zero or more times, as a
    /// comprehension passes them
    pub(crate) fn call_items(
        &self,
        args: &[(Multiplicity, CallArgument)],
        result: Term,
        input: Option<Term>,
        output: Option<Term>,
    ) -> Term {
        let mut group = Vec::new();
        let params = self.arguments(args, &mut group);
        let mut slot = |term: Term, kind| hole(self.db, &mut group, term, kind);
        let function = Function {
            params,
            result: slot(result, Kind::Type),
            input: input.map(|term| slot(term, Kind::Type)),
            output: output.map(|term| slot(term, Kind::Type)),
        };
        let ty = self.db.intern(Type::Function(function));
        let environment = self.intern_environment(self.empty_environment(), group);
        self.view(ty, environment)
    }

    /// The schema of items passed as arguments are
    pub(crate) fn arguments_schema(&self, args: &[(Multiplicity, CallArgument)]) -> Term {
        let mut group = Vec::new();
        let schema = self.arguments(args, &mut group);
        let environment = self.intern_environment(self.empty_environment(), group);
        self.view(schema, environment)
    }

    /// Arguments' schema, with a hole in `group` for each term
    fn arguments(&self, args: &[(Multiplicity, CallArgument)], group: &mut Vec<Term>) -> TypeId {
        let mut slot = |term: Term, kind| hole(self.db, group, term, kind);
        let items: Vec<_> = args
            .iter()
            .map(|&(multiplicity, arg)| SchemaItem {
                multiplicity,
                element: match arg {
                    CallArgument::Positional(term) => Element::Positional(slot(term, Kind::Type)),
                    CallArgument::Keyword(name, term) => Element::Keyed {
                        key: self.db.intern(Type::Literal(Literal::Sym(name))),
                        value: slot(term, Kind::Type),
                    },
                    CallArgument::Pair(key, value) => Element::Keyed {
                        key: slot(key, Kind::Type),
                        value: slot(value, Kind::Type),
                    },
                    CallArgument::Spread(term) => Element::Include(slot(term, Kind::Schema)),
                },
            })
            .collect();
        self.db.intern(Type::Schema(items.into()))
    }

    pub(crate) fn infer(&mut self) -> Term {
        self.fresh(Kind::Type, Rest::All, ScopeId(0))
    }

    /// A fresh variable of `kind`. A schema variable's items occupy `lanes`.
    pub(crate) fn infer_kind(&mut self, kind: Kind, lanes: Rest) -> Term {
        self.fresh(kind, lanes, ScopeId(0))
    }

    fn fresh(&self, kind: Kind, lanes: Rest, scope: ScopeId) -> Term {
        let id = InferVarId(self.bounds.len());
        self.bounds.push(Bounds::default());
        self.inference.push(Inference {
            kind,
            lanes,
            assignment: Cell::new(None),
            defaulted: Cell::new(false),
            exact: Cell::new(false),
            fallback: Cell::new(None),
            support: MonoHashSet::new(),
            subscribers: MonoHashSet::new(),
            dirty: Cell::new(false),
            scope,
        });
        Term::Infer(id)
    }

    /// A new scope inside `parent`
    fn enter(&self, parent: ScopeId) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        let depth = self.scopes[parent.0].depth + 1;
        self.scopes.push(Scope { parent, depth });
        id
    }

    /// The innermost scope of the variables and skolems in `terms`, where what
    /// relating them creates belongs. Scopes that meet are nested, since a
    /// variable never takes a skolem from outside its own.
    fn scope(&self, terms: &[Term]) -> Result<ScopeId, Residual> {
        let mut leaves = HashSet::new();
        for &term in terms {
            self.leaves(term, 0, 0, &mut leaves)?;
        }
        let scope = |leaf: &Term| match *leaf {
            Term::Infer(id) => self.inference[id.0].scope,
            Term::Skolem(id) => self.skolems[id.0].scope,
            Term::View(_) => unreachable!("a view is not a leaf"),
        };
        Ok((leaves.iter().map(scope))
            .max_by_key(|scope| self.scopes[scope.0].depth)
            .unwrap_or(ScopeId(0)))
    }

    /// A committed, fully resolved solution. Bounds remain available independently.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn solution(&self, id: InferVarId) -> Option<TypeId> {
        match self.inference[id.0].assignment.get()? {
            Term::View(view) if view.environment == self.empty_environment() => Some(view.ty),
            _ => None,
        }
    }

    /// A variable's committed solution as a term, which may hold skolems
    fn assignment(&self, id: InferVarId) -> Option<Term> {
        self.inference[id.0].assignment.get()
    }

    /// Obligations that supported the commitment, retained for diagnostic inspection.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
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

    /// A closed type with any transparent declaration it's an application of
    /// expanded, as a union alias is to its union. `None` if it isn't structural.
    pub(crate) fn exposed(&self, ty: TypeId) -> Option<TypeId> {
        match self.head(self.closed(ty)) {
            Ok(Head::Structural(view)) => self.reify(Term::View(view)).ok(),
            _ => None,
        }
    }

    /// The nominal declaration a closed type is, or is an application of, with any
    /// transparent declaration expanded as [`Self::exposed`] does, and its
    /// arguments. `None` if it isn't nominal.
    pub(crate) fn exposed_nominal(&self, ty: TypeId) -> Option<(DeclId, Vec<TypeId>)> {
        let Ok(Head::Nominal(nominal)) = self.head(self.closed(ty)) else {
            return None;
        };
        let arguments = (nominal.arguments.into_iter())
            .map(|argument| self.reify(argument).ok())
            .collect::<Option<_>>()?;
        Some((nominal.declaration, arguments))
    }

    /// Rebuild a closed canonical type, retaining references owned by local binders.
    pub(crate) fn reify(&self, term: Term) -> Result<TypeId, Residual> {
        self.reify_scoped(term, 0, 0)
    }

    fn reify_scoped(&self, term: Term, local: u32, depth: usize) -> Result<TypeId, Residual> {
        self.depth(depth)?;
        self.spend()?;
        match term {
            Term::Infer(id) => match self.assignment(id) {
                Some(term) => self.reify_scoped(term, 0, depth + 1),
                None => Err(Residual::Inference),
            },
            // A skolem has no canonical form; nothing leaves its judgment with one
            Term::Skolem(_) => Err(Residual::Escape),
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
                let reified = self.db.intern(mapped);
                // Item projections are evaluated where they're closed
                if local == 0
                    && let Ok(Some(evaluated)) = self.evaluate_items(reified)
                {
                    return Ok(evaluated);
                }
                Ok(reified)
            }
        }
    }

    /// Collect the variables and skolems a term contains, through its environments
    fn leaves(
        &self,
        term: Term,
        local: u32,
        depth: usize,
        found: &mut HashSet<Term>,
    ) -> Result<(), Residual> {
        self.depth(depth)?;
        self.spend()?;
        match term {
            Term::Infer(_) | Term::Skolem(_) => {
                found.insert(term);
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
                        self.leaves(value, 0, depth + 1, found)?;
                    }
                } else {
                    let mut children = Vec::new();
                    ty.visit_children(|child, groups| children.push((child, groups)));
                    for (child, groups) in children {
                        self.leaves(view.child(child), local + groups, depth + 1, found)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn subscribe(&self, obligation: ObligationId) -> Result<(), Residual> {
        let relation = self.obligation(obligation).relation;
        let mut leaves = HashSet::new();
        self.leaves(relation.actual, 0, 0, &mut leaves)?;
        self.leaves(relation.expected, 0, 0, &mut leaves)?;
        for leaf in leaves {
            let Term::Infer(variable) = leaf else {
                continue;
            };
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

    /// A solver for a side query, with this one's scope and remaining budget. The
    /// caller adds its work back.
    fn nested(&self) -> Result<Self, Residual> {
        self.spend()?;
        if self.limits.depth <= 1 {
            return Err(Residual::Limit);
        }
        let mut nested = Self::with_limits(
            self.db,
            Limits {
                work: self.limits.work.saturating_sub(self.work.get()),
                depth: self.limits.depth - 1,
            },
        );
        nested.scope = self.scope.clone();
        #[cfg(feature = "debug")]
        {
            nested.names = self.names.clone();
            nested.indent = self.indent + 1;
        }
        Ok(nested)
    }

    /// Closed proof queries cannot create inference bounds or leak alternative edges.
    fn probe(&self, actual: TypeId, expected: TypeId) -> Result<Status, Residual> {
        let mut proof = self.nested()?;
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
                if self.assignment(id).is_some() || !visiting.insert(id) {
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
            Term::Skolem(_) => Ok(false),
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

    /// The unsolved variables that raising the given terms could raise: those at
    /// an output position of theirs, covariant or invariant, including through
    /// the upper bounds of a variable raised. A function's parameters and channels
    /// are inputs, which whatever supplies the function takes from its expected
    /// type and never raises. A form the walk can't see into counts as an output.
    pub(crate) fn raised(&self, terms: &[Term]) -> Result<HashSet<InferVarId>, Residual> {
        let mut raised = HashSet::new();
        let mut visit = |id: InferVarId, variance: Variance| {
            if variance == Variance::Contravariant || !raised.insert(id) {
                return Vec::new();
            }
            self.bounds(id).upper().collect()
        };
        for &term in terms {
            self.variances(term, Variance::Covariant, false, &mut visit, 0, 0)?;
        }
        Ok(raised)
    }

    /// The unsolved variables a default would lock in by choosing a literal: those
    /// that the given terms, at the given variances, reach at a position that
    /// isn't covariant, where a later value can't widen them. A variable's bounds
    /// are walked at its own position, since its default is the join of its lower
    /// bounds; its upper bounds are walked too, which can only lock more. A form
    /// the walk can't see into counts as invariant.
    pub(crate) fn locked(
        &self,
        roots: &[(Term, Variance)],
    ) -> Result<HashSet<InferVarId>, Residual> {
        let mut locked = HashSet::new();
        let mut seen = HashSet::new();
        let mut visit = |id: InferVarId, variance: Variance| {
            if !seen.insert((id, variance)) {
                return Vec::new();
            }
            if variance != Variance::Covariant {
                locked.insert(id);
            }
            let bounds = self.bounds(id);
            bounds.lower().chain(bounds.upper()).collect()
        };
        for &(term, variance) in roots {
            self.variances(term, variance, true, &mut visit, 0, 0)?;
        }
        Ok(locked)
    }

    /// Walk a term's unsolved variables with the variance of each position they
    /// occur at, composed from `variance`. `visit` gives the terms to walk next,
    /// at the same variance. A function's parameters and channels are walked
    /// contravariantly if `inputs`, and otherwise not at all.
    fn variances(
        &self,
        term: Term,
        variance: Variance,
        inputs: bool,
        visit: &mut impl FnMut(InferVarId, Variance) -> Vec<Term>,
        local: u32,
        depth: usize,
    ) -> Result<(), Residual> {
        self.depth(depth)?;
        let view = match term {
            Term::Infer(id) => {
                if self.assignment(id).is_some() {
                    return Ok(());
                }
                for next in visit(id, variance) {
                    self.variances(next, variance, inputs, visit, 0, depth + 1)?;
                }
                return Ok(());
            }
            Term::Skolem(_) => return Ok(()),
            Term::View(view) => view,
        };
        let mut walk = |ty: TypeId, variance: Variance, groups: u32| {
            let child = view.child(ty);
            self.variances(child, variance, inputs, visit, local + groups, depth + 1)
        };
        match *self.db.ty(view.ty) {
            Type::Bound { reference, kind } => {
                if u32::from(reference.depth) < local {
                    return Ok(());
                }
                let value = self.lookup(
                    view.environment,
                    (u32::from(reference.depth) - local) as u16,
                    reference.slot,
                    kind,
                );
                self.variances(value, variance, inputs, visit, 0, depth + 1)
            }
            Type::Top
            | Type::Unknown(_)
            | Type::Unsupported { .. }
            | Type::Literal(_)
            | Type::Fresh(_)
            | Type::Decl(_)
            | Type::Rigid { .. } => Ok(()),
            Type::Apply { base, ref args, .. } => {
                let binders = match *self.db.ty(base) {
                    Type::Decl(id) if self.db.declaration(id).source.kind.nominal() => {
                        match self.db.ty(self.db.declaration(id).ty) {
                            Type::Quantified { binders, .. } if binders.len() == args.len() => {
                                Some(binders)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                for (index, arg) in args.iter().enumerate() {
                    let (Argument::Positional(ty)
                    | Argument::Keyword(_, ty)
                    | Argument::Expand(ty)) = *arg;
                    let binder = match (binders, arg) {
                        (Some(binders), Argument::Positional(_)) => binders[index].variance,
                        _ => Variance::Invariant,
                    };
                    walk(ty, variance.compose(binder), 0)?;
                }
                Ok(())
            }
            Type::Function(ref function) => {
                if inputs {
                    let flipped = variance.compose(Variance::Contravariant);
                    walk(function.params, flipped, 0)?;
                    for channel in [function.input, function.output].into_iter().flatten() {
                        walk(channel, flipped, 0)?;
                    }
                }
                walk(function.result, variance, 0)
            }
            Type::Schema(ref items) => {
                for item in items.iter() {
                    match item.element {
                        Element::Positional(ty) | Element::Include(ty) => walk(ty, variance, 0)?,
                        Element::Keyed { key, value } => {
                            walk(key, variance, 0)?;
                            walk(value, variance, 0)?;
                        }
                    }
                }
                Ok(())
            }
            Type::Union(ref members) => {
                for member in members.iter() {
                    match *member {
                        UnionMember::Type(ty) => walk(ty, variance, 0)?,
                        _ => {
                            walk(member.id(), variance.compose(Variance::Invariant), 0)?;
                            if let Some(key) = member.key() {
                                walk(key, variance.compose(Variance::Invariant), 0)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            // Each pack is where its pattern places its items, and its count
            // only adds items
            Type::Map { ref packs, pattern } => {
                for (index, &pack) in packs.iter().enumerate() {
                    let index = u16::try_from(index).expect("a mapping's packs fit a group");
                    let inner = self.db.slot_variance(pattern, index);
                    walk(
                        pack,
                        variance.compose(inner.unwrap_or(Variance::Covariant)),
                        0,
                    )?;
                }
                walk(pattern, variance, 1)
            }
            Type::Quantified { .. } => {
                let mut children = Vec::new();
                self.db
                    .ty(view.ty)
                    .visit_children(|ty, groups| children.push((ty, groups)));
                for (ty, groups) in children {
                    walk(ty, variance.compose(Variance::Invariant), groups)?;
                }
                Ok(())
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
        // A bound holding a skolem has no closed form. Only identity forces one.
        if bounds
            .lower()
            .chain(bounds.upper())
            .map(|term| self.skolemic(term))
            .collect::<Result<Vec<_>, _>>()?
            .contains(&true)
        {
            let Some(candidate) = self.open_candidate(id)? else {
                return Ok(false);
            };
            for upper in bounds.upper() {
                if self.same(candidate, upper)? {
                    self.commit(id, candidate);
                    return Ok(true);
                }
            }
            return Ok(false);
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
        let Some(candidate) = self.join(inference.kind, &lower) else {
            return Ok(false);
        };
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
        self.commit(id, self.closed(candidate));
        Ok(true)
    }

    /// The candidate of a variable with a lower bound holding a skolem: the one
    /// term every lower bound is, if it holds no unsolved variable. Skolems
    /// aren't joined, since a join would need their canonical forms.
    fn open_candidate(&self, id: InferVarId) -> Result<Option<Term>, Residual> {
        let mut lower = self.bounds[id.0].lower();
        let Some(candidate) = lower.next() else {
            return Ok(None);
        };
        for other in lower {
            if !self.same(candidate, other)? {
                return Ok(None);
            }
        }
        let mut leaves = HashSet::new();
        self.solved_leaves(candidate, &mut leaves)?;
        Ok((!leaves.iter().any(|leaf| matches!(leaf, Term::Infer(_)))).then_some(candidate))
    }

    /// Whether a term holds a skolem, through assignments
    fn skolemic(&self, term: Term) -> Result<bool, Residual> {
        let mut leaves = HashSet::new();
        self.solved_leaves(term, &mut leaves)?;
        Ok(leaves.iter().any(|leaf| matches!(leaf, Term::Skolem(_))))
    }

    /// The skolems and unsolved variables a term holds, through assignments
    fn solved_leaves(&self, term: Term, found: &mut HashSet<Term>) -> Result<(), Residual> {
        let mut leaves = HashSet::new();
        self.leaves(term, 0, 0, &mut leaves)?;
        for leaf in leaves {
            match leaf {
                Term::Infer(id) if let Some(assigned) = self.assignment(id) => {
                    self.solved_leaves(assigned, found)?;
                }
                _ => {
                    found.insert(leaf);
                }
            }
        }
        Ok(())
    }

    /// Assign a candidate and wake what depends on it. A candidate holds no
    /// unsolved variable, so substitution cycles cannot be introduced.
    fn commit(&self, id: InferVarId, candidate: Term) {
        debug_assert!({
            let mut leaves = HashSet::new();
            let scope = self.inference[id.0].scope;
            self.solved_leaves(candidate, &mut leaves).is_err()
                || leaves.iter().all(|leaf| match *leaf {
                    Term::Skolem(skolem) => self.visible(scope, self.skolems[skolem.0].scope),
                    _ => true,
                })
        });
        let bounds = &self.bounds[id.0];
        let inference = &self.inference[id.0];
        for (_, sources) in bounds.lower.iter().chain(bounds.upper.iter()) {
            for &source in sources.iter() {
                let _ = inference.support.try_insert(source);
            }
        }
        inference.assignment.set(Some(candidate));
        trace!(
            self,
            "?{} := {}{}",
            id.0,
            self.render(candidate),
            if inference.defaulted.get() {
                " (default)"
            } else {
                ""
            }
        );
        self.generation.set(self.generation.get() + 1);
        for &obligation in inference.subscribers.iter() {
            self.schedule(obligation);
        }
        // A bound may contain the assigned variable deeply in a contextual view.
        for other in self.inference.iter() {
            if other.assignment.get().is_none() {
                other.dirty.set(true);
            }
        }
    }

    /// Default an unsolved variable to the join of its lower bounds: the least
    /// choice, not one its constraints force. The caller decides which variables
    /// to default and in what order, defaulting a variable's lower bounds first,
    /// then solves again. `Unknown` among the lower bounds makes the default
    /// `Unknown`.
    ///
    /// The join's literals decay to their classes, so `1` and `2` give `Int`,
    /// unless the decayed join can't be shown to lie above every lower bound and
    /// below every solved upper bound. Then the join stays precise: an upper
    /// bound may require the literal, and a lower bound such as `Array[1 | 2]`
    /// can't widen, since its argument is invariant. A forced assignment keeps
    /// its literals, since its bounds require them.
    ///
    /// A lower bound that isn't yet solved leaves the variable unsolved. So does
    /// a type variable without lower bounds, rather than inventing a type. A
    /// schema variable may take a closed shape from its upper bounds, with its
    /// item types constrained by the other bounds. An upper
    /// bound that isn't yet solved is checked once the default is, through the
    /// obligations that pair it with the lower bounds. Any other upper bound the
    /// default can't be shown to satisfy leaves the variable unsolved; if the
    /// bounds contradict each other, those obligations report it.
    pub(crate) fn default(&mut self, id: InferVarId) -> Result<TypeId, Residual> {
        self.default_with(id, true)
    }

    /// [`Self::default`], keeping the precise join unless `decay`: a caller
    /// decays only the variables whose choice a literal would lock in (see
    /// [`Self::locked`]). A variable standing for the key of an item projection
    /// never decays (see [`Database::item_keys`]).
    pub(crate) fn default_with(&mut self, id: InferVarId, decay: bool) -> Result<TypeId, Residual> {
        let defaulted = self
            .default_term(id, decay)
            .and_then(|term| self.reify(term));
        #[cfg(feature = "debug")]
        if let Err(residual) = defaulted {
            trace!(self, "?{} not defaulted: {residual:?}", id.0);
        }
        defaulted
    }

    /// [`Self::default_with`], committing a term. A variable with a lower bound
    /// holding a skolem takes it if it's the only one, and its upper bounds are
    /// left to the obligations pairing them with it; a skolem can't be probed.
    fn default_term(&self, id: InferVarId, decay: bool) -> Result<Term, Residual> {
        if let Some(term) = self.assignment(id) {
            return Ok(term);
        }
        let bounds = &self.bounds[id.0];
        if bounds
            .lower()
            .map(|term| self.skolemic(term))
            .collect::<Result<Vec<_>, _>>()?
            .contains(&true)
        {
            let candidate = self
                .open_candidate(id)?
                .ok_or(Residual::Unsupported("joining a variable's lower bounds"))?;
            self.inference[id.0].defaulted.set(true);
            self.commit(id, candidate);
            return Ok(candidate);
        }
        let lower = bounds
            .lower()
            .map(|term| self.reify(term))
            .collect::<Result<Vec<_>, _>>()?;
        let kind = self.inference[id.0].kind;
        if lower.is_empty() {
            if kind != Kind::Schema {
                return Err(Residual::Inference);
            }
            let upper = bounds
                .upper()
                .map(|term| self.reify(term))
                .collect::<Result<Vec<_>, _>>()?;
            let candidate = self.upper_schema(&upper)?;
            self.inference[id.0].defaulted.set(true);
            self.commit(id, self.closed(candidate));
            return Ok(self.closed(candidate));
        }
        let unknown = self.db.unknown_of(kind);
        let candidate = if lower.contains(&unknown) {
            unknown
        } else {
            let precise = self
                .join(kind, &lower)
                .ok_or(Residual::Unsupported("joining a variable's lower bounds"))?;
            let decayed = self.db.decay(precise);
            let admitted = || -> Result<bool, Residual> {
                for &ty in &lower {
                    if self.probe(ty, decayed)? != Status::Proven {
                        return Ok(false);
                    }
                }
                self.below_upper(id, decayed)
            };
            let decay = decay && !self.inference[id.0].exact.get();
            if decay && decayed != precise && admitted().unwrap_or(false) {
                decayed
            } else {
                precise
            }
        };
        if !self.below_upper(id, candidate)? {
            return Err(Residual::Unsupported(
                "a default above a variable's upper bounds",
            ));
        }
        self.inference[id.0].defaulted.set(true);
        self.commit(id, self.closed(candidate));
        Ok(self.closed(candidate))
    }

    /// Whether a candidate can be shown to satisfy each of a variable's solved
    /// upper bounds
    fn below_upper(&self, id: InferVarId, candidate: TypeId) -> Result<bool, Residual> {
        for upper in self.bounds[id.0].upper() {
            // An upper bound holding a skolem is checked by the obligations
            // pairing it with the lower bounds
            let upper = match self.reify(upper) {
                Ok(upper) => upper,
                Err(Residual::Inference | Residual::Escape) => continue,
                Err(issue) => return Err(issue),
            };
            if self.probe(candidate, upper)? != Status::Proven {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a variable's solution was chosen by [`Self::default`], not forced
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn defaulted(&self, id: InferVarId) -> bool {
        self.inference[id.0].defaulted.get()
    }

    /// A variable's kind
    pub(crate) fn variable_kind(&self, id: InferVarId) -> Kind {
        self.inference[id.0].kind
    }

    /// The default of the binder a variable instantiates, if it has one
    pub(crate) fn fallback(&self, id: InferVarId) -> Option<Term> {
        self.inference[id.0].fallback.get()
    }

    /// The least candidate above nonempty lower bounds: their join, or for a
    /// schema the one they all are, since there are no schema unions
    fn join(&self, kind: Kind, lower: &[TypeId]) -> Option<TypeId> {
        match kind {
            Kind::Type => lower.iter().copied().reduce(|a, b| self.lub(a, b)),
            Kind::Schema => lower.iter().all(|&ty| ty == lower[0]).then_some(lower[0]),
        }
    }

    pub(crate) fn bounds(&self, id: InferVarId) -> &Bounds {
        &self.bounds[id.0]
    }

    pub(crate) fn obligation(&self, id: ObligationId) -> &Obligation {
        &self.obligations[id.0]
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn provenance(&self, id: ConstraintId) -> &Provenance {
        &self.roots[id.0].provenance
    }

    /// Check the term IDs and return their kind
    fn kind(&self, term: Term) -> Kind {
        match term {
            Term::Infer(id) => self.inference[id.0].kind,
            Term::Skolem(id) => self.skolems[id.0].kind,
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
        trace!(
            self,
            "#{}: {} <: {}",
            self.roots.len(),
            self.render(actual),
            self.render(expected)
        );
        let obligation = self.enqueue(Relation {
            actual,
            expected,
            language: false,
        });
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
        // A type argument's schema is the items a value holds, and a parameter
        // list binds by count. Bounds settle by count, the stricter reading.
        let language = match step {
            Step::Argument { .. } => true,
            Step::Parameters | Step::BoundPropagation | Step::Assignment => false,
            _ => self.obligations[parent.0].relation.language,
        };
        let child = self.enqueue(Relation {
            actual,
            expected,
            language,
        });
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
                if let Some(assigned) = self.assignment(id) {
                    term = assigned;
                    continue;
                }
                return Ok(term);
            }
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

    /// Whether an exposed head is the dynamic type or schema
    fn is_unknown(&self, head: &Head) -> bool {
        matches!(head, Head::Structural(view) if matches!(self.db.ty(view.ty), Type::Unknown(_)))
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
        // An assignment carries its own context, not the caller's local scope
        if let Term::Infer(id) = a
            && let Some(assigned) = self.assignment(id)
        {
            return self.same_scoped(assigned, 0, b, bd, depth + 1);
        }
        if let Term::Infer(id) = b
            && let Some(assigned) = self.assignment(id)
        {
            return self.same_scoped(a, ad, assigned, 0, depth + 1);
        }
        // A fresh literal is the same as its regular twin
        let regular = |term: Term| match term {
            Term::View(view) if matches!(self.db.ty(view.ty), Type::Fresh(_)) => {
                self.closed(self.db.regular(view.ty))
            }
            _ => term,
        };
        let (a, b) = (regular(a), regular(b));
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
        if let (Term::Skolem(_), _) | (_, Term::Skolem(_)) = (a, b) {
            return Ok(a == b);
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

    /// Substitute fixed arguments whose binder bounds the caller has established.
    /// Population places every argument in its binder's slot, whatever the
    /// binder's kind or binding; only arguments after an expansion of unknown
    /// reach stay as written.
    fn instantiate(
        &self,
        binders: &[Binder],
        arguments: &[Argument],
        view: TypeView,
        parent: EnvironmentId,
    ) -> Result<(Vec<Term>, EnvironmentId), Issue> {
        assert!(
            binders.iter().all(|b| b.binding != Binding::Implicit),
            "implicit binder applied"
        );
        if arguments
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
            .zip(binders)
            .map(|(arg, binder)| {
                let Argument::Positional(ty) = *arg else {
                    unreachable!()
                };
                // Resolving keeps environments from nesting through forwarded binders.
                let term = self.resolve(view.child(ty))?;
                assert_eq!(self.kind(term), binder.kind, "argument kind mismatch");
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
                Term::Skolem(id) => return Ok(Head::Skolem(id)),
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
                // A fresh literal relates as its regular twin
                Type::Fresh(_) => term = self.closed(self.db.regular(view.ty)),
                // A projection is evaluated once its schema is substituted
                Type::Union(ref members)
                    if view.environment != self.empty_environment()
                        && members.iter().any(|member| member.projected().is_some()) =>
                {
                    match self.reify(term) {
                        Ok(reified) => term = self.closed(reified),
                        // A skolem's projection is never evaluated, so it relates
                        // only as itself
                        Err(Residual::Escape) => return Ok(Head::Structural(view)),
                        Err(residual) => return Err(residual.into()),
                    }
                }
                // An item projection takes the solver to select by its key. One that
                // can't be evaluated is left for the rules of unions.
                Type::Union(ref members) if members.iter().any(|member| member.key().is_some()) => {
                    match self.evaluate_items(view.ty) {
                        Ok(Some(evaluated)) => term = self.closed(evaluated),
                        Ok(None) | Err(Issue::Residual(_)) => return Ok(Head::Structural(view)),
                        Err(issue) => return Err(issue),
                    }
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
        self.ancestor_in(current, target, false, path, depth)
    }

    /// [`Self::ancestor`], following only the supertypes the runtime inherits
    /// from when `runtime`
    fn ancestor_in(
        &self,
        current: Nominal,
        target: DeclId,
        runtime: bool,
        path: &mut HashSet<DeclId>,
        depth: usize,
    ) -> Result<Option<Nominal>, Issue> {
        self.mro(
            current,
            runtime,
            path,
            depth,
            &mut |visited| match visited {
                Visited::Nominal(nominal) if nominal.declaration == target => {
                    Ok(Some(nominal.clone()))
                }
                Visited::Nominal(_) => Ok(None),
                Visited::Structural => Err(Residual::Unsupported(
                    "an ancestor search through a structural supertype",
                )
                .into()),
            },
        )
    }

    /// Visit `current` and its ancestors in MRO order, left to right and depth
    /// first, until `visit` finds something. A supertype that isn't nominal is
    /// visited as structural, and its ancestors are unknown.
    fn preorder<T>(
        &self,
        current: Nominal,
        path: &mut HashSet<DeclId>,
        depth: usize,
        visit: &mut impl FnMut(Visited<'_>) -> Result<Option<T>, Issue>,
    ) -> Result<Option<T>, Issue> {
        self.mro(current, false, path, depth, visit)
    }

    /// [`Self::preorder`], following only the supertypes the runtime inherits
    /// from when `runtime`
    fn mro<T>(
        &self,
        current: Nominal,
        runtime: bool,
        path: &mut HashSet<DeclId>,
        depth: usize,
        visit: &mut impl FnMut(Visited<'_>) -> Result<Option<T>, Issue>,
    ) -> Result<Option<T>, Issue> {
        self.depth(depth)?;
        self.spend()?;
        if let Some(found) = visit(Visited::Nominal(&current))? {
            return Ok(Some(found));
        }
        if !path.insert(current.declaration) {
            return Err(Residual::Recursive.into());
        }
        let supers = &self.db.declaration(current.declaration).supertypes;
        for supertype in supers
            .iter()
            .filter(|supertype| supertype.runtime || !runtime)
        {
            let head = self.head(self.view(supertype.ty, current.environment))?;
            let found = match head {
                Head::Nominal(next) => self.mro(next, runtime, path, depth + 1, visit)?,
                _ => visit(Visited::Structural)?,
            };
            if found.is_some() {
                return Ok(found);
            }
        }
        path.remove(&current.declaration);
        Ok(None)
    }

    /// Whether a skolem of `skolem`'s scope is visible in `scope`: `skolem` is it
    /// or a scope it's inside
    fn visible(&self, mut scope: ScopeId, skolem: ScopeId) -> bool {
        loop {
            if scope == skolem {
                return true;
            }
            if scope == ScopeId(0) {
                return false;
            }
            scope = self.scopes[scope.0].parent;
        }
    }

    /// Process queued obligations to quiescence or exhaustion and report each submitted root.
    pub(crate) fn solve(&mut self) -> Vec<Outcome> {
        self.quiesce();
        let outcomes: Vec<Outcome> = (0..self.roots.len())
            .map(|index| self.outcome(ConstraintId(index)))
            .collect();
        #[cfg(feature = "debug")]
        for (index, outcome) in outcomes.iter().enumerate() {
            // The first diagnostic that explains the status
            let issue = (outcome.diagnostics.iter()).find_map(|diagnostic| {
                match (outcome.status, diagnostic.issue) {
                    (Status::Contradicted, Issue::Contradiction(contradiction)) => {
                        Some(format!(" ({contradiction:?})"))
                    }
                    (Status::Unresolved, Issue::Residual(residual)) => {
                        Some(format!(" ({residual:?})"))
                    }
                    _ => None,
                }
            });
            let issue = issue.unwrap_or_default();
            trace!(self, "#{index}: {:?}{issue}", outcome.status);
        }
        outcomes
    }

    /// Process queued obligations to quiescence or exhaustion. Trials then judge
    /// alternatives, and settling a scope's variables commits choices, so solving
    /// resumes after each one that changes something.
    fn quiesce(&mut self) {
        loop {
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
            if self.exhausted.get() {
                break;
            }
            if !self.try_alternatives() && !self.settle() {
                break;
            }
        }
    }

    /// Commit a choice for one variable of a skolem scope, which nothing outside
    /// its judgment sees, or of the root scope in a closed solver: a default,
    /// innermost scope first and in creation order, so each follows what an
    /// earlier one forces. Without one, a variable nothing is below takes bottom,
    /// the least choice. Whether one was committed.
    fn settle(&self) -> bool {
        let mut pending: Vec<InferVarId> = (0..self.inference.len())
            .map(InferVarId)
            .filter(|&id| {
                let inference = &self.inference[id.0];
                (self.closed || inference.scope != ScopeId(0))
                    && inference.assignment.get().is_none()
            })
            .collect();
        pending
            .sort_by_key(|id| std::cmp::Reverse(self.scopes[self.inference[id.0].scope.0].depth));
        for &id in &pending {
            if self.default_term(id, false).is_ok() {
                return true;
            }
        }
        for &id in &pending {
            let inference = &self.inference[id.0];
            if inference.kind == Kind::Type && self.bounds[id.0].lower().next().is_none() {
                inference.defaulted.set(true);
                self.commit(id, self.closed(self.db.bottom()));
                return true;
            }
        }
        false
    }

    /// Trace a root through its dependencies to collect issues and determine its status.
    fn outcome(&self, constraint: ConstraintId) -> Outcome {
        let root = self.roots[constraint.0].obligation;
        let mut diagnostics = vec![];
        // A contradiction under a callable's signature leaves it unresolved
        let soften = |issue, soft| match issue {
            Issue::Contradiction(_) if soft => callable::UNFIT.into(),
            _ => issue,
        };
        let softens = |soft, step: &Step| soft || matches!(step, Step::Callable(_));
        // Iterative DFS keeps reporting safe even when the obligation graph is deep.
        let mut stack = vec![(root, false, false)];
        let mut active = HashSet::new();
        let mut visited = HashSet::new();
        let mut path = vec![];
        while let Some((id, exit, soft)) = stack.pop() {
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
                    issue: soften(issue, soft),
                    path: path.clone(),
                }),
                State::Reduced => {}
            }
            stack.push((id, true, soft));
            for dependency in self.obligations[id.0].dependencies.iter().filter(|d| {
                self.obligations[id.0]
                    .active
                    .borrow()
                    .contains(&(d.obligation, d.step.clone()))
            }) {
                stack.push((
                    dependency.obligation,
                    false,
                    softens(soft, &dependency.step),
                ));
            }
        }
        // Historical edges explain contradictions, but are not current proof
        // premises. A path through an assignment explains a contradiction only by
        // what the assignment was drawn from, so one is taken only where no other
        // reaches it.
        for assignments in [false, true] {
            let mut history = vec![(root, vec![root], false)];
            let mut seen = HashSet::new();
            while let Some((id, path, soft)) = history.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if let State::Issue(issue @ Issue::Contradiction(_)) =
                    self.obligations[id.0].state.get()
                    && let issue = soften(issue, soft)
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
                    if !assignments && dependency.step == Step::Assignment {
                        continue;
                    }
                    let mut next = path.clone();
                    next.push(dependency.obligation);
                    history.push((dependency.obligation, next, softens(soft, &dependency.step)));
                }
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

/// A hole at depth 0 in `group`, filled by `term`
fn hole(db: &Database, group: &mut Vec<Term>, term: Term, kind: Kind) -> TypeId {
    group.push(term);
    db.intern(Type::Bound {
        reference: BoundRef::new(0, group.len() - 1),
        kind,
    })
}

mod alternatives;
mod callable;
mod conform;
mod item;
mod lattice;
mod member;
mod narrow;
mod reduce;
mod schema;
mod unpack;

use alternatives::Alternatives;
pub(crate) use callable::{Constructor, Signature, bound_method};
pub(crate) use conform::{Inheritance, Requirement, RequirementKind};
pub(crate) use lattice::Widening;
pub(crate) use member::{FoundKind, Lookup, Signatures};
pub(crate) use narrow::Target as NarrowTarget;
pub(crate) use unpack::PatternShape;

#[cfg(test)]
mod tests;
