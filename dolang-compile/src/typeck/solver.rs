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
//! are the only binder bounds taken as facts. Those include the bounds its item
//! projections imply (see [`Database::implied_bounds`]), which instantiation
//! establishes as it does declared ones. Any other declaration's rigid has
//! escaped its check.

use std::cell::{Cell, RefCell};

use dolang_util::{
    alias, intern,
    mono::{MonoHashMap, MonoHashSet, MonoVec},
};
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};

use crate::typeck::r#type::UnitSpan;

use super::r#type::{
    Argument, Binder, Binding, BoundRef, Database, DeclId, DeclKind, Element, Function, Intrinsic,
    Kind, Literal, MemberKey, Multiplicity, Promotion, Rest, Restriction, SchemaItem, SymbolId,
    Type, TypeId, UnionMember, Variance,
};

/// A member of a generic class's object: its class, the class a private one is
/// looked up through, and its key
type GenericMember = (DeclId, Option<DeclId>, MemberKey, member::Access);

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
pub(crate) enum Relation {
    /// `actual <: expected`
    Subtype {
        actual: Term,
        expected: Term,
        /// How each side's schemas fill their multiplicities
        fill: Fill,
    },
    /// A call of a callee (see [`Solver::constrain_call`])
    Call(Call),
}

/// How the schemas of a subtype relation fill their multiplicities. A parameter
/// list binds by count: each required item takes one, optional items take what
/// is left over from left to right, and a repeated item takes the rest. Other
/// schemas may fill any way that fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Fill {
    /// Both sides are parameter lists, as when functions relate
    Parameters,
    /// Items passed on the actual side bound to a parameter list on the
    /// expected side, as at a call
    Arguments,
    /// The items values hold on both sides, as in a type argument
    Contents,
}

impl Relation {
    /// What the relation checks: a subtype's actual side, or a call's callee
    pub(crate) fn checked(self) -> Term {
        match self {
            Relation::Subtype { actual, .. } => actual,
            Relation::Call(call) => call.callee,
        }
    }

    /// A subtype's sides, `(actual, expected)`
    pub(crate) fn sides(self) -> Option<(Term, Term)> {
        match self {
            Relation::Subtype {
                actual, expected, ..
            } => Some((actual, expected)),
            Relation::Call(_) => None,
        }
    }
}

/// A call: `callee` takes `arguments`, a schema of the items passed, and the
/// ambient channels, and gives `result`. A callee that is a function relates its
/// parameters to the arguments, which fill them any way that fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Call {
    pub(crate) callee: Term,
    pub(crate) arguments: Term,
    pub(crate) result: Term,
    pub(crate) input: Option<Term>,
    pub(crate) output: Option<Term>,
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
    /// An inference variable has no committed solution. A limitation: an
    /// annotation supplies one.
    Inference,
    /// No implemented rule handles this combination of exposed type forms. It
    /// names what wasn't handled. A limitation of the rules implemented.
    Unsupported(&'static str),
    /// Generic application has arguments after an expansion, whose binders
    /// aren't known. A limitation: source can write them.
    GenericArguments,
    /// A type form that population never produces: a generic declaration left
    /// unapplied, an application missing defaulted arguments, or one whose
    /// constructor isn't a declaration's type. Internal, as only a typelib
    /// written without being checked, or a checker bug, can supply one.
    Malformed,
    /// An intrinsic subtype rule needs a backing type that the database has not
    /// registered. A cascade: `std` checked from source reports what it lacks
    /// once.
    MissingIntrinsic(Intrinsic),
    /// Exposing declarations, or walking supertypes, revisited a declaration,
    /// or closed proof dependencies cycle without a guard. Internal, as
    /// well-formedness and population recover from recursion of each of these
    /// kinds.
    Recursive,
    /// A candidate contains a recursive substitution, or proof dependencies
    /// cycle through a judgment that is not closed. None establishes a proof. A
    /// limitation of inference: an annotation closes the judgment.
    Cycle,
    /// A work or traversal-depth limit prevented completion. Also used for
    /// obligations left pending when solving exhausted its work budget. A
    /// limitation: an annotation may simplify the work.
    Limit,
    /// A rigid of a declaration this solver does not check has escaped its own
    /// check. Internal, as each declaration's rigids stay in its own check.
    Escape,
    /// Positional schema items can't be matched up by count: several expected
    /// items repeat, or an opaque schema precedes items of varying count. A
    /// limitation of schema matching.
    Alignment,
    /// Several alternatives of a judgment that needs one remain possible, so
    /// none is chosen (see [`Solver::trial`]). A limitation: an annotation may
    /// pick one.
    Ambiguous,
}

/// What a [`Residual`] says about the code it leaves undecided
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResidualClass {
    /// The checker can't decide checked code; a reader may work around it.
    Limitation,
    /// It shouldn't arise for checked code: it comes from a typelib that is
    /// inconsistent, or was written without being checked, or otherwise from a
    /// checker bug.
    Internal,
    /// An error reported elsewhere explains it, so it isn't reported.
    Cascade,
}

impl Residual {
    pub(crate) fn class(self) -> ResidualClass {
        match self {
            Self::Recursive | Self::Escape | Self::Malformed => ResidualClass::Internal,
            Self::MissingIntrinsic(_) => ResidualClass::Cascade,
            Self::Inference
            | Self::Unsupported(_)
            | Self::GenericArguments
            | Self::Cycle
            | Self::Limit
            | Self::Alignment
            | Self::Ambiguous => ResidualClass::Limitation,
        }
    }
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
    /// A restricted schema's item admits only one, where the restriction
    /// requires it to admit none, by the key that selects it
    Required(TypeId),
    /// An `InsertItem`'s key selects an item that doesn't admit repetition
    Unrepeatable(TypeId),
    /// A mapping's packs have items that don't correspond, by count,
    /// multiplicity or form
    MappedPacks,
    /// No overload of an overloaded function, or no signature of a callable, fits
    /// the function type it's related to (see [`Solver::rejections`] for why)
    NoOverload,
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
    /// A call's arguments below its callee's parameter list
    Arguments,
    Return,
    /// A function's ambient input channel, related contravariantly
    Input,
    /// A function's ambient output channel, related contravariantly
    Output,
    IntrinsicBacking(Intrinsic),
    BoundPropagation,
    Assignment,
    UnionMember(usize),
    /// A rigid reduced to its written bound, or a rest's shape
    RigidBound,
    /// An omitted ambient channel's rigid reduced to a gradual unit's `Unknown`
    ImplicitBound,
    /// A schema item's positional type, keyed value or included schema
    Item(usize),
    /// A schema item's key
    Key(usize),
    /// An item an `AssignItem`'s key selects, by its index among them
    Selected(usize),
    /// A quantified type's body under fresh variables for its binders
    Instantiation,
    /// A fresh variable below its binder's bound
    InstantiationBound(usize),
    /// A quantified type's body under skolems for its binders
    Skolemization,
    /// A skolem reduced to its binder's bound
    SkolemBound,
    /// A rigid or skolem reduced to a bound implied by an item projection
    /// selecting by it
    ImpliedBound,
    /// A skolem outside a variable's scope replaced by its bound, as the
    /// variable's lower bound
    Promotion,
    /// A callable value's signature by its index among those it's called with,
    /// below a function type or called in its place, chosen by trials
    Callable(usize),
    /// An overloaded function's signature by its index among its overloads,
    /// chosen by trials
    Overload(usize),
    /// An overloaded function related as its implementation
    Implementation,
    /// A metatype related as the `Type[C]` it reaches
    Metatype,
    /// A restriction that can't be evaluated reduced to its schema, which it
    /// is if it holds
    Restricted,
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

/// What a walk of [`Solver::variances`] is for, which decides where it looks
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    /// What raising a term could raise: not a function's inputs, which whatever
    /// supplies the function takes from its expected type, nor an item
    /// projection's key, since a value can't choose the key that selects it
    Raised,
    /// What a literal would lock in: a function's inputs too, and an item
    /// projection's key
    Locked,
}

impl Walk {
    /// Whether a function's parameters and channels are walked, contravariantly
    fn inputs(self) -> bool {
        self == Self::Locked
    }

    /// Whether an item projection's key is walked, at its variance
    fn keys(self) -> bool {
        self != Self::Raised
    }
}

/// What a solver records of an obligation's processing. The relation is the
/// obligation's identity, kept apart.
struct ObligationLayer {
    state: Cell<State>,
    queued: Cell<bool>,
    // Child obligation IDs and labeled steps used as current proof premises.
    // Replaced on reprocessing; `dependencies` retains historical edges for diagnostics.
    active: RefCell<Vec<(ObligationId, Step)>>,
    dependencies: MonoVec<Dependency>,
}

#[derive(Clone, Debug)]
pub(crate) struct Diagnostic {
    pub(crate) issue: Issue,
    /// Root-to-leaf path; each node retains its relation and reduction metadata.
    pub(crate) path: Vec<ObligationId>,
}

/// A contradiction and the derivation path to it, resolved where it was found
#[derive(Clone, Debug)]
pub(crate) struct Contradicted {
    pub(crate) contradiction: Contradiction,
    pub(crate) path: alias::Box<[Node]>,
}

/// An obligation on a contradiction's path, as diagnostics read it
#[derive(Clone, Debug)]
pub(crate) struct Node {
    /// How this node derives from the previous one: `None` at the root, or where
    /// no dependency edge was recorded
    pub(crate) step: Option<Step>,
    /// A subtype's sides, each `None` where it doesn't reify; `None` for a call
    pub(crate) sides: Option<(Option<TypeId>, Option<TypeId>)>,
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

#[derive(Debug)]
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
type BoundSet = MonoHashMap<Term, MonoHashSet<ObligationId, FxBuildHasher>, FxBuildHasher>;

/// What an inference variable is, fixed when it's created
struct Variable {
    kind: Kind,
    /// For a schema variable, the lanes its items can occupy
    lanes: Rest,
    /// The scope it was created in, which bounds the skolems it may take
    scope: ScopeId,
}

/// A variable's resolution, independent of accumulated bounds
#[derive(Clone, Default)]
struct VariableState {
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
}

/// What a solver records of a variable: its resolution, and what it adds to
/// the sets that only grow
#[derive(Default)]
struct VariableLayer {
    state: VariableState,
    bounds: BoundSets,
    support: MonoHashSet<ObligationId, FxBuildHasher>,
    subscribers: MonoHashSet<ObligationId, FxBuildHasher>,
}

/// A rigid or skolem, known only by its bounds
#[derive(Clone, Copy)]
enum Abstract {
    Rigid(TypeId),
    Skolem(SkolemId),
}

/// A binder of a quantified type on the right, held abstract while its body is
/// related (see [`Solver::skolemization`])
struct Skolem {
    kind: Kind,
    binding: Binding,
    /// The binder's bound in the skolemization's environment, or a rest binder's
    /// shape. Set once the environment exists.
    bound: Cell<Option<Term>>,
    /// The bounds the quantified body's item projections imply (see
    /// [`Database::implied_bounds`]). Never declared, so they don't affect
    /// variance.
    implied: MonoVec<Term>,
    scope: ScopeId,
}

/// A skolemization's extent: its skolems are visible to variables of this scope
/// and the scopes inside it. The root scope has no skolems.
struct Scope {
    parent: ScopeId,
    depth: usize,
}

/// Accumulated constraints on one inference variable `V`, as one solver
/// records them
#[derive(Default)]
struct BoundSets {
    /// Each key `L` imposes `L <: V`.
    lower: BoundSet,
    /// Each key `U` imposes `V <: U`.
    upper: BoundSet,
}

impl BoundSets {
    fn side(&self, lower: bool) -> &BoundSet {
        if lower { &self.lower } else { &self.upper }
    }
}

/// A variable's bounds, in the order they were first imposed
#[derive(Default)]
pub(crate) struct Bounds {
    lower: Vec<Term>,
    upper: Vec<Term>,
}

impl Bounds {
    pub(crate) fn lower(&self) -> impl Iterator<Item = Term> + '_ {
        self.lower.iter().copied()
    }

    pub(crate) fn upper(&self) -> impl Iterator<Item = Term> + '_ {
        self.upper.iter().copied()
    }
}

/// Where each table's own entries begin: the IDs below are the parent's
/// (#900). A solver that isn't nested owns them all.
#[derive(Clone, Copy, Default)]
struct Bases {
    variables: usize,
    obligations: usize,
    environments: usize,
    skolems: usize,
    scopes: usize,
}

/// How deeply a solver's traversals may recurse, unless a caller says otherwise
const DEPTH: usize = 256;

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
    /// A class's metatype, with the class applied to its arguments
    Meta(Nominal),
}

/// All solver IDs are local to this solver and those nested in it, just as type
/// IDs are database-local. A solver nested in another sees its parent's tables
/// and records its own changes, so nothing solved on it reaches the parent (see
/// [`Solver::trial`]).
pub(crate) struct Solver<'db> {
    db: &'db Database,
    /// Where each table's own entries begin. Every table is reached through
    /// accessors that account for it.
    bases: Bases,
    environments: intern::Table<Environment, EnvironmentId>,
    variables: MonoVec<Variable>,
    layers: MonoVec<VariableLayer>,
    /// What it records of its parent's variables, by ID
    overlays: MonoHashMap<usize, VariableLayer, FxBuildHasher>,
    /// The first variable it sees: a closed solver sees only its own
    first_variable: usize,
    /// The variables whose bounds may have changed since solving last tried
    /// to assign them, by ID from the first it sees
    dirty: RefCell<Vec<bool>>,
    /// The environment each quantifier instantiation created, by the obligation
    /// that instantiated it, so reprocessing reuses its variables
    instantiations: RefCell<FxHashMap<ObligationId, EnvironmentId>>,
    /// The environment of skolems each skolemization created, by the obligation
    /// that skolemized, so reprocessing reuses its skolems
    skolemizations: RefCell<FxHashMap<ObligationId, EnvironmentId>>,
    skolems: MonoVec<Skolem>,
    scopes: MonoVec<Scope>,
    // Intern only the relation and its guard bit; processing state and
    // diagnostic edges do not participate in identity and can grow while
    // existing nodes are borrowed.
    relations: MonoVec<(Relation, bool)>,
    obligations: MonoVec<ObligationLayer>,
    /// What it records of its parent's obligations' processing, by ID
    obligation_overlays: MonoHashMap<usize, ObligationLayer, FxBuildHasher>,
    obligation_index: MonoHashMap<(Relation, bool), ObligationId, FxBuildHasher>,
    queue: MonoVec<ObligationId>,
    roots: Vec<Root>,
    /// The work budget, shared with the solvers nested in this one
    gas: &'db Gas,
    /// How deeply traversals may recurse
    depth: usize,
    /// The declarations being checked, whose rigids' bounds are assumptions
    scope: FxHashSet<DeclId>,
    /// The rigids an assumed declaration's group is checked under, where they
    /// aren't its own: a binder lifted from an enclosing declaration is that
    /// declaration's rigid (see [`Solver::assume_group`])
    groups: FxHashMap<DeclId, Vec<TypeId>>,
    /// Whether the declarations being checked are of a gradual unit, whose body
    /// sees an omitted channel as `Unknown` rather than `Value`. Such a solver
    /// also makes up `Unknown` where a strict unit's takes a sound type or leaves
    /// the judgment unresolved: in widening, `meet` and narrowing.
    gradual: bool,
    /// Each rigid's bounds, once computed
    rigid_bounds: RefCell<FxHashMap<TypeId, Vec<(TypeId, Step)>>>,
    /// Whether the judgments own the root scope's variables, so solving settles
    /// them as it settles a skolem scope's
    closed: bool,
    /// The judgments that need one of their alternatives to hold, by obligation
    alternatives: RefCell<FxHashMap<ObligationId, Alternatives>>,
    /// How each class's object is called, once found: its exact object, or a
    /// `Type` of the class
    constructors: RefCell<FxHashMap<(DeclId, bool), Constructor>>,
    /// Each class's default construction, once found
    default_constructions: RefCell<FxHashMap<DeclId, Option<Option<Signature>>>>,
    /// Each member of a generic class's object, once found
    generic_members: RefCell<FxHashMap<GenericMember, Result<Lookup, Issue>>>,
    /// Whether it makes the assumptions its parent does, so their caches
    /// hold for it too
    inherits: bool,
    /// Whether it's nested in a probe, so its obligations' guard bits count
    /// (see [`Solver::guarded`])
    tracks: bool,
    /// Counts changes to bounds and assignments, so trials rerun only after
    /// what they saw has grown
    generation: Cell<usize>,
    /// The first obligation whose alternatives a trial's fork judges: the
    /// judgments it was forked with are its parent's to judge
    trials_from: usize,
    /// The first variable a trial's fork owns, if it settles the variables it
    /// creates as a closed solver settles the root scope's: nothing outside the
    /// trial sees them
    owned: Option<usize>,
    /// What trials judge in place of a term, as a `do` block whose result
    /// mustn't choose an alternative (see [`Solver::blind`])
    blinded: FxHashMap<Term, Term>,
    /// The solver this one is nested in, if any (#900)
    parent: Option<Parent<'db>>,
    /// Whether a solver nested in this one is live, which nothing may change
    /// this one under
    child: Cell<bool>,
    /// How traces render a type (see [`Solver::named`])
    #[cfg(feature = "debug")]
    names: Option<(&'db dyn super::r#type::Names, super::r#type::Style)>,
    /// How many trials and side queries the solver is nested in, which indents
    /// its traces
    #[cfg(feature = "debug")]
    indent: usize,
}

/// `Solver` is covariant in its database lifetime, which nested solvers that
/// borrow their parent rely on (#900)
fn _covariant<'a, 'b: 'a>(solver: Solver<'b>) -> Solver<'a> {
    solver
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
    /// A solver whose work is charged to `gas`
    pub(crate) fn new(db: &'db Database, gas: &'db Gas) -> Self {
        Self::with_depth(db, gas, DEPTH)
    }

    /// [`Self::new`], with traversals recursing at most `depth` deep
    pub(crate) fn with_depth(db: &'db Database, gas: &'db Gas, depth: usize) -> Self {
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
            bases: Bases::default(),
            environments,
            variables: MonoVec::new(),
            layers: MonoVec::new(),
            overlays: MonoHashMap::default(),
            first_variable: 0,
            dirty: RefCell::new(Vec::new()),
            instantiations: RefCell::new(FxHashMap::default()),
            skolemizations: RefCell::new(FxHashMap::default()),
            skolems: MonoVec::new(),
            scopes,
            relations: MonoVec::new(),
            obligations: MonoVec::new(),
            obligation_overlays: MonoHashMap::default(),
            obligation_index: MonoHashMap::default(),
            queue: MonoVec::new(),
            roots: Vec::new(),
            gas,
            depth,
            scope: FxHashSet::default(),
            groups: FxHashMap::default(),
            gradual: false,
            rigid_bounds: RefCell::new(FxHashMap::default()),
            closed: false,
            alternatives: RefCell::new(FxHashMap::default()),
            constructors: RefCell::new(FxHashMap::default()),
            default_constructions: RefCell::new(FxHashMap::default()),
            generic_members: RefCell::new(FxHashMap::default()),
            inherits: true,
            tracks: false,
            generation: Cell::new(0),
            owned: None,
            trials_from: 0,
            blinded: FxHashMap::default(),
            parent: None,
            child: Cell::new(false),
            #[cfg(feature = "debug")]
            names: None,
            #[cfg(feature = "debug")]
            indent: 0,
        }
    }

    /// Render types in traces with `names` in `style`, if `typeck.solver` is traced
    #[cfg(feature = "debug")]
    pub(crate) fn named(
        &mut self,
        names: &'db dyn super::r#type::Names,
        style: super::r#type::Style,
    ) {
        if dolang_util::debug_enabled!("typeck.solver") {
            self.names = Some((names, style));
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
                let Some((names, style)) = self.names else {
                    return format!("{view:?}");
                };
                // Rendering mustn't spend the work it limits
                let reified = self.gas.refunded(|| self.reify(term));
                if let Ok(ty) = reified {
                    return self.db.render(ty, names, style);
                }
                let group = self.frame(view.environment).group.clone();
                let group: Vec<String> = group.into_iter().map(|term| self.render(term)).collect();
                let ty = self.db.render(view.ty, names, style);
                format!("{ty} with [{}]", group.join(", "))
            }
        }
    }

    /// A relation for a trace: `actual <: expected`, or a call as
    /// `callee(arguments) -> result`
    #[cfg(feature = "debug")]
    fn render_relation(&self, relation: Relation) -> String {
        match relation {
            Relation::Subtype {
                actual, expected, ..
            } => format!("{} <: {}", self.render(actual), self.render(expected)),
            Relation::Call(call) => {
                let channel = |sigil, term: Option<Term>| {
                    term.map(|term| format!(" {sigil}{}", self.render(term)))
                        .unwrap_or_default()
                };
                format!(
                    "{}({}){}{} -> {}",
                    self.render(call.callee),
                    self.render(call.arguments),
                    channel('<', call.input),
                    channel('>', call.output),
                    self.render(call.result)
                )
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
        self.inherits = false;
        self.constructors.get_mut().clear();
        self.default_constructions.get_mut().clear();
        self.generic_members.get_mut().clear();
    }

    /// Assume `decl` with its group checked as `rigids`, as a body is: a binder
    /// lifted from an enclosing declaration is that declaration's rigid, so its
    /// own binders' bounds refer to the rigids its body sees
    pub(crate) fn assume_group(&mut self, decl: DeclId, rigids: Vec<TypeId>) {
        self.assume(decl);
        self.groups.insert(decl, rigids);
        self.rigid_bounds.get_mut().clear();
    }

    /// Check the assumed declarations as a gradual unit's: an omitted channel's
    /// rigid is bounded by `Unknown`, so the body's use of it is dynamic
    pub(crate) fn gradual(&mut self) {
        self.gradual = true;
        self.inherits = false;
        self.rigid_bounds.get_mut().clear();
    }

    /// Whether it checks a gradual unit's declarations (see [`Self::gradual`])
    pub(crate) fn is_gradual(&self) -> bool {
        self.gradual
    }

    /// A solver nested in this one, whose work is charged to `gas`. Nothing
    /// solved on it reaches this one.
    pub(crate) fn fork<'a>(&'a self, gas: &'a Gas) -> Solver<'a> {
        self.child(Nesting::Fork, gas)
    }

    /// A closed solver for a side query about `class`, its rigids assumed,
    /// spending this one's budget
    pub(super) fn side_query(&self, class: DeclId) -> Solver<'_> {
        let mut solver = self.child(Nesting::Side, self.gas);
        solver.depth = DEPTH;
        solver.assume(class);
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

    /// A rigid's bounds, with the rigids its declaration's group is checked
    /// under for its group (see [`Self::assume_group`]), each with the step
    /// that reduces it to the bound: its binder's bound, then those the item
    /// projections in its declaration's type imply. A rest binder without a
    /// bound is bounded by its rest mode's shape, and an omitted channel by
    /// `Unknown` when checking a gradual unit.
    fn rigid_bounds(&self, ty: TypeId) -> Vec<(TypeId, Step)> {
        if let Some(bounds) = self.cached(|solver| &solver.rigid_bounds, &ty) {
            return bounds;
        }
        let Type::Rigid { decl, slot, .. } = *self.db.ty(ty) else {
            unreachable!()
        };
        let quantified = self.db.declaration(decl).ty;
        let Type::Quantified { binders, .. } = self.db.ty(quantified) else {
            unreachable!("a rigid of a declaration without binders")
        };
        let binder = &binders[usize::from(slot)];
        let rigids = (self.groups.get(&decl).cloned()).unwrap_or_else(|| self.db.rigids(decl));
        let (declared, step) = match binder.binding {
            Binding::Implicit if self.gradual && binder.bound.is_none() => {
                (Some(self.db.unknown()), Step::ImplicitBound)
            }
            Binding::Implicit => (self.db.binder_bound(binder, &rigids), Step::ImplicitBound),
            _ => (self.db.binder_bound(binder, &rigids), Step::RigidBound),
        };
        let implied = (self.db.implied_bounds(quantified).into_iter())
            .filter(|&(implied, _)| implied == slot)
            .map(|(_, bound)| (self.db.substitute(bound, &rigids), Step::ImpliedBound));
        let bounds: Vec<_> = declared
            .map(|bound| (bound, step))
            .into_iter()
            .chain(implied)
            .collect();
        self.rigid_bounds.borrow_mut().insert(ty, bounds.clone());
        bounds
    }

    /// A rigid's or skolem's bounds, each with the step that reduces it to the
    /// bound: its binder's bound, then those its item projections imply
    fn abstract_bounds(&self, of: Abstract) -> Vec<(Term, Step)> {
        match of {
            Abstract::Rigid(ty) => (self.rigid_bounds(ty).into_iter())
                .map(|(bound, step)| (self.closed(bound), step))
                .collect(),
            Abstract::Skolem(id) => {
                let skolem = self.skolem(id);
                let step = match skolem.binding {
                    Binding::Implicit => Step::ImplicitBound,
                    _ => Step::SkolemBound,
                };
                let declared = skolem.bound.get().map(|bound| (bound, step));
                let implied = (skolem.implied.iter()).map(|&bound| (bound, Step::ImpliedBound));
                declared.into_iter().chain(implied).collect()
            }
        }
    }

    /// The first of a rigid's or skolem's bounds, for a rule that reduces it to
    /// just one
    fn abstract_bound(&self, of: Abstract) -> Option<(Term, Step)> {
        self.abstract_bounds(of).into_iter().next()
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
            match self.widened_head(term)? {
                Head::Meta(_) => unreachable!("a metatype is widened"),
                Head::Infer(_) => return Err(Residual::Inference.into()),
                Head::Skolem(id) => match self.abstract_bound(Abstract::Skolem(id)) {
                    Some((bound, _)) => term = bound,
                    None => return Ok(Reach::Unreached),
                },
                Head::Nominal(nominal) => {
                    return Ok(
                        match self.ancestor_in(
                            nominal,
                            target,
                            runtime,
                            &mut FxHashSet::default(),
                            0,
                        )? {
                            Some(found) => Reach::Reached(found.arguments),
                            None => Reach::Unreached,
                        },
                    );
                }
                Head::Structural(view) => match self.db.ty(view.ty) {
                    Type::Unknown(_) => return Ok(Reach::Dynamic),
                    Type::Rigid { .. } => {
                        self.rigid(view.ty)?;
                        match self.abstract_bound(Abstract::Rigid(view.ty)) {
                            Some((bound, _)) => term = bound,
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
        assert!(self.has_environment(parent));
        for &term in &group {
            self.kind(term);
        }
        if group.is_empty() {
            return parent;
        }
        self.intern_frame(Environment { parent, group })
    }

    pub(crate) fn view(&self, ty: TypeId, environment: EnvironmentId) -> Term {
        self.db.ty(ty);
        assert!(self.has_environment(environment));
        Term::View(TypeView { ty, environment })
    }

    pub(crate) fn closed(&self, ty: TypeId) -> Term {
        self.view(ty, self.empty_environment())
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
        Term::Infer(self.push_variable(Variable { kind, lanes, scope }))
    }

    /// A new scope inside `parent`
    fn enter(&self, parent: ScopeId) -> ScopeId {
        let depth = self.extent(parent).depth + 1;
        self.push_scope(Scope { parent, depth })
    }

    /// The innermost scope of the variables and skolems in `terms`, where what
    /// relating them creates belongs. Scopes that meet are nested, since a
    /// variable never takes a skolem from outside its own.
    fn scope(&self, terms: &[Term]) -> Result<ScopeId, Residual> {
        let mut leaves = FxHashSet::default();
        for &term in terms {
            self.leaves(term, 0, 0, &mut leaves)?;
        }
        let scope = |leaf: &Term| match *leaf {
            Term::Infer(id) => self.variable(id).scope,
            Term::Skolem(id) => self.skolem(id).scope,
            Term::View(_) => unreachable!("a view is not a leaf"),
        };
        Ok((leaves.iter().map(scope))
            .max_by_key(|&scope| self.extent(scope).depth)
            .unwrap_or(ScopeId(0)))
    }

    /// A committed, fully resolved solution. Bounds remain available independently.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn solution(&self, id: InferVarId) -> Option<TypeId> {
        match self.assignment(id)? {
            Term::View(view) if view.environment == self.empty_environment() => Some(view.ty),
            _ => None,
        }
    }

    /// A variable's committed solution as a term, which may hold skolems
    fn assignment(&self, id: InferVarId) -> Option<Term> {
        self.state(id).assignment.get()
    }

    /// Obligations that supported the commitment, retained for diagnostic inspection.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn solution_sources(&self, id: InferVarId) -> impl Iterator<Item = ObligationId> {
        self.support(id).into_iter()
    }

    pub(crate) fn unresolved(&self) -> impl Iterator<Item = InferVarId> + '_ {
        self.variable_ids()
            .filter(|&id| self.assignment(id).is_none())
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
                // Item projections and restrictions are evaluated where they're
                // closed
                if local == 0
                    && let Ok(Some(evaluated)) = self.evaluate_items(reified)
                {
                    return Ok(evaluated);
                }
                if local == 0
                    && let Type::Restricted {
                        schema,
                        restriction,
                    } = *self.db.ty(reified)
                    && let Ok(schema) = self.restricted(schema, restriction)
                {
                    return Ok(schema);
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
        found: &mut FxHashSet<Term>,
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
        let mut leaves = FxHashSet::default();
        match self.relation(obligation) {
            Relation::Subtype {
                actual, expected, ..
            } => {
                self.leaves(actual, 0, 0, &mut leaves)?;
                self.leaves(expected, 0, 0, &mut leaves)?;
            }
            // A call waits only on its callee: what it derives waits on the rest
            Relation::Call(call) => self.leaves(call.callee, 0, 0, &mut leaves)?,
        }
        for leaf in leaves {
            let Term::Infer(variable) = leaf else {
                continue;
            };
            self.add_subscriber(variable, obligation);
            if self.assignment(variable).is_some() {
                for source in self.support(variable) {
                    self.link(source, obligation, Step::Assignment);
                }
            }
        }
        Ok(())
    }

    fn schedule(&self, id: ObligationId) {
        if !self.progress_mut(id).queued.replace(true) {
            self.queue.push(id);
        }
    }

    /// A closed solver nested in this one, with its scope, spending its budget
    fn nested(&self, nesting: Nesting) -> Result<Solver<'_>, Residual> {
        self.spend()?;
        if self.depth <= 1 {
            return Err(Residual::Limit);
        }
        let mut nested = self.child(nesting, self.gas);
        nested.depth = self.depth - 1;
        Ok(nested)
    }

    /// Closed proof queries cannot create inference bounds or leak alternative
    /// edges. The answer is a query's (see [`Nesting::Probe`]).
    fn probe(&self, actual: TypeId, expected: TypeId) -> Result<Status, Residual> {
        Ok(self.probe_cycle(actual, expected, None)?.0)
    }

    /// [`Self::probe`], as a premise of `from` if given, and whether an
    /// unresolved result rests on a cycle: a probe of a judgment an enclosing
    /// probe is proving that it may not assume (see [`Self::revisit`]), or a
    /// nested [`Residual::Recursive`] or [`Residual::Cycle`]. Such a cycle is
    /// left unresolved rather than unfolded until the work budget runs out.
    fn probe_cycle(
        &self,
        actual: TypeId,
        expected: TypeId,
        from: Option<ObligationId>,
    ) -> Result<(Status, bool), Residual> {
        let premise = from.map(|obligation| self.guarded(obligation));
        match self.revisit(actual, expected, premise) {
            Revisit::Fresh => {}
            Revisit::Cycle => {
                trace!(self, "probe revisits an enclosing judgment");
                return Ok((Status::Unresolved, true));
            }
            Revisit::Assumed => {
                trace!(self, "probe assumes an enclosing judgment");
                return Ok((Status::Proven, false));
            }
        }
        let mut proof = self.nested(Nesting::Probe {
            actual,
            expected,
            premise,
        })?;
        proof.constrain(
            proof.closed(actual),
            proof.closed(expected),
            Provenance::default(),
        );
        let result = proof.solve().remove(0);
        if self.gas.is_exhausted()
            || result
                .diagnostics
                .iter()
                .any(|d| d.issue == Residual::Limit.into())
        {
            return Err(Residual::Limit);
        }
        let cycle = result.status == Status::Unresolved
            && result.diagnostics.iter().any(|d| {
                matches!(
                    d.issue,
                    Issue::Residual(Residual::Recursive | Residual::Cycle)
                )
            });
        Ok((result.status, cycle))
    }

    /// Follow exact candidate dependencies, without unfolding declaration bodies.
    /// Variable-only cycles are not recursive type substitutions.
    fn occurs(
        &self,
        target: InferVarId,
        term: Term,
        local: u32,
        nested: bool,
        visiting: &mut FxHashSet<InferVarId>,
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
    /// type and never raises. An item projection's key selects its items, and
    /// raising them doesn't choose another. A form the walk can't see into counts
    /// as an output.
    pub(crate) fn raised(&self, terms: &[Term]) -> Result<FxHashSet<InferVarId>, Residual> {
        let mut raised = FxHashSet::default();
        let mut visit = |id: InferVarId, variance: Variance| {
            if variance == Variance::Contravariant || !raised.insert(id) {
                return Vec::new();
            }
            self.bounds(id).upper().collect()
        };
        for &term in terms {
            self.variances(term, Variance::Covariant, Walk::Raised, &mut visit, 0, 0)?;
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
    ) -> Result<FxHashSet<InferVarId>, Residual> {
        let mut locked = FxHashSet::default();
        let mut seen = FxHashSet::default();
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
            self.variances(term, variance, Walk::Locked, &mut visit, 0, 0)?;
        }
        Ok(locked)
    }

    /// Walk a term's unsolved variables with the variance of each position they
    /// occur at, composed from `variance`. `visit` gives the terms to walk next,
    /// at the same variance. What `purpose` is decides whether a function's
    /// parameters and channels, and an item projection's key, are walked.
    fn variances(
        &self,
        term: Term,
        variance: Variance,
        purpose: Walk,
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
                    self.variances(next, variance, purpose, visit, 0, depth + 1)?;
                }
                return Ok(());
            }
            Term::Skolem(_) => return Ok(()),
            Term::View(view) => view,
        };
        let mut walk = |ty: TypeId, variance: Variance, groups: u32| {
            let child = view.child(ty);
            self.variances(child, variance, purpose, visit, local + groups, depth + 1)
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
                self.variances(value, variance, purpose, visit, 0, depth + 1)
            }
            Type::Top
            | Type::Unknown(_)
            | Type::Unsupported { .. }
            | Type::Literal(_)
            | Type::Fresh(_)
            | Type::Decl(_)
            | Type::Meta(_)
            | Type::Rigid { .. } => Ok(()),
            // A metatype's arguments are invariant, as any whose binders aren't known
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
                if purpose.inputs() {
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
            Type::Overloaded {
                ref overloads,
                implementation,
                ..
            } => {
                for &ty in overloads.iter().chain(implementation.iter()) {
                    walk(ty, variance, 0)?;
                }
                Ok(())
            }
            Type::Union(ref members) => {
                for member in members.iter() {
                    match *member {
                        UnionMember::Type(ty) => walk(ty, variance, 0)?,
                        // `IndexItem` joins the values its key selects, which a
                        // wider key raises. `AssignItem` and `InsertItem` meet
                        // them, which a wider key lowers, and their schema is
                        // invariant: wider values raise the meet, but more items
                        // lower it.
                        UnionMember::IndexItem(schema, key)
                        | UnionMember::AssignItem(schema, key)
                        | UnionMember::InsertItem(schema, key) => {
                            let (inner, by_key) = match member {
                                UnionMember::IndexItem(..) => {
                                    (Variance::Covariant, Variance::Covariant)
                                }
                                _ => (Variance::Invariant, Variance::Contravariant),
                            };
                            walk(schema, variance.compose(inner), 0)?;
                            if purpose.keys() {
                                walk(key, variance.compose(by_key), 0)?;
                            }
                        }
                        _ => {
                            walk(member.id(), variance.compose(Variance::Invariant), 0)?;
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
            // A restricted schema is its schema where every item its key
            // removes admits zero, which a wider key removes more of
            Type::Restricted {
                schema,
                restriction,
            } => {
                walk(schema, variance.compose(Variance::Invariant), 0)?;
                if let Some(key) = restriction.key()
                    && purpose.keys()
                {
                    walk(key, variance.compose(Variance::Contravariant), 0)?;
                }
                Ok(())
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
        let bounds = self.bounds(id);
        for lower in bounds.lower() {
            for upper in bounds.upper() {
                if self.same(lower, upper)?
                    && self.occurs(id, lower, 0, false, &mut FxHashSet::default(), 0)?
                {
                    for (_, sources) in (self.bound_sources(id, true).into_iter())
                        .chain(self.bound_sources(id, false))
                    {
                        for source in sources {
                            let state = &self.progress_mut(source).state;
                            if matches!(
                                state.get(),
                                State::Issue(Issue::Residual(Residual::Inference))
                            ) {
                                state.set(State::Issue(Residual::Cycle.into()));
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
            for &term in set {
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
        let Some(candidate) = self.join(self.variable(id).kind, &lower) else {
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
        let bounds = self.bounds(id);
        let mut lower = bounds.lower();
        let Some(candidate) = lower.next() else {
            return Ok(None);
        };
        for other in lower {
            if !self.same(candidate, other)? {
                return Ok(None);
            }
        }
        let mut leaves = FxHashSet::default();
        self.solved_leaves(candidate, &mut leaves)?;
        Ok((!leaves.iter().any(|leaf| matches!(leaf, Term::Infer(_)))).then_some(candidate))
    }

    /// Whether a term holds a skolem, through assignments
    fn skolemic(&self, term: Term) -> Result<bool, Residual> {
        let mut leaves = FxHashSet::default();
        self.solved_leaves(term, &mut leaves)?;
        Ok(leaves.iter().any(|leaf| matches!(leaf, Term::Skolem(_))))
    }

    /// The skolems and unsolved variables a term holds, through assignments
    fn solved_leaves(&self, term: Term, found: &mut FxHashSet<Term>) -> Result<(), Residual> {
        let mut leaves = FxHashSet::default();
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
            let mut leaves = FxHashSet::default();
            let scope = self.variable(id).scope;
            self.solved_leaves(candidate, &mut leaves).is_err()
                || leaves.iter().all(|leaf| match *leaf {
                    Term::Skolem(skolem) => self.visible(scope, self.skolem(skolem).scope),
                    _ => true,
                })
        });
        for (_, sources) in
            (self.bound_sources(id, true).into_iter()).chain(self.bound_sources(id, false))
        {
            for source in sources {
                self.add_support(id, source);
            }
        }
        let state = self.state_mut(id);
        state.assignment.set(Some(candidate));
        trace!(
            self,
            "?{} := {}{}",
            id.0,
            self.render(candidate),
            if state.defaulted.get() {
                " (default)"
            } else {
                ""
            }
        );
        self.generation.set(self.generation.get() + 1);
        for obligation in self.subscribers(id) {
            self.schedule(obligation);
        }
        // A bound may contain the assigned variable deeply in a contextual view.
        for other in self.variable_ids() {
            if self.assignment(other).is_none() {
                self.dirty(other);
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
        let bounds = self.bounds(id);
        if bounds
            .lower()
            .map(|term| self.skolemic(term))
            .collect::<Result<Vec<_>, _>>()?
            .contains(&true)
        {
            let candidate = self
                .open_candidate(id)?
                .ok_or(Residual::Unsupported("joining a variable's lower bounds"))?;
            self.state_mut(id).defaulted.set(true);
            self.commit(id, candidate);
            return Ok(candidate);
        }
        let lower = bounds
            .lower()
            .map(|term| self.reify(term))
            .collect::<Result<Vec<_>, _>>()?;
        let kind = self.variable(id).kind;
        if lower.is_empty() {
            if kind != Kind::Schema {
                return Err(Residual::Inference);
            }
            let upper = bounds
                .upper()
                .map(|term| self.reify(term))
                .collect::<Result<Vec<_>, _>>()?;
            let candidate = self.upper_schema(&upper)?;
            self.state_mut(id).defaulted.set(true);
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
            let decay = decay && !self.state(id).exact.get();
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
        self.state_mut(id).defaulted.set(true);
        self.commit(id, self.closed(candidate));
        Ok(self.closed(candidate))
    }

    /// Default a variable without lower bounds to the meet of its upper bounds,
    /// as the greatest type they show to fit them: for an instantiated binder
    /// nothing else bounds, its bound. A variable with a lower bound, without
    /// upper bounds, or with an upper bound that isn't yet solved is left
    /// unsolved, for its caller to default otherwise. A schema variable's upper
    /// bounds must be ordered, since schemas have no general meet. Only a caller
    /// that has given up waiting for lower bounds uses this: [`Self::default`]
    /// never invents a type from no lower bounds.
    pub(crate) fn default_upper(&mut self, id: InferVarId) -> Result<TypeId, Residual> {
        let bounds = self.bounds(id);
        if self.assignment(id).is_some() || bounds.lower().next().is_some() {
            return Err(Residual::Inference);
        }
        let upper = bounds
            .upper()
            .map(|term| self.reify(term))
            .collect::<Result<Vec<_>, _>>()?;
        let mut upper = upper.into_iter();
        let first = upper.next().ok_or(Residual::Inference)?;
        let candidate = upper.try_fold(first, |met, ty| match self.variable(id).kind {
            Kind::Type => self.meet(met, ty).map_err(|issue| match issue {
                Issue::Residual(residual) => residual,
                Issue::Contradiction(_) => Residual::Unsupported("meeting upper bounds"),
            }),
            Kind::Schema => {
                if met == ty || self.probe(met, ty)? == Status::Proven {
                    Ok(met)
                } else if self.probe(ty, met)? == Status::Proven {
                    Ok(ty)
                } else {
                    Err(Residual::Unsupported("meeting unordered schema bounds"))
                }
            }
        })?;
        self.state_mut(id).defaulted.set(true);
        self.commit(id, self.closed(candidate));
        #[cfg(feature = "debug")]
        trace!(self, "?{} defaulted from its upper bounds", id.0);
        Ok(candidate)
    }

    /// Whether a candidate can be shown to satisfy each of a variable's solved
    /// upper bounds
    fn below_upper(&self, id: InferVarId, candidate: TypeId) -> Result<bool, Residual> {
        for upper in self.bounds(id).upper() {
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
        self.state(id).defaulted.get()
    }

    /// A variable's kind
    pub(crate) fn variable_kind(&self, id: InferVarId) -> Kind {
        self.variable(id).kind
    }

    /// The default of the binder a variable instantiates, if it has one
    pub(crate) fn fallback(&self, id: InferVarId) -> Option<Term> {
        self.state(id).fallback.get()
    }

    /// The least candidate above nonempty lower bounds: their join, or for a
    /// schema the one they all are, since there are no schema unions
    fn join(&self, kind: Kind, lower: &[TypeId]) -> Option<TypeId> {
        match kind {
            Kind::Type => lower.iter().copied().reduce(|a, b| self.lub(a, b)),
            Kind::Schema => lower.iter().all(|&ty| ty == lower[0]).then_some(lower[0]),
        }
    }

    /// A contradicted outcome's contradictions, each with its path from the root
    /// resolved as diagnostics read it. Members of a union that don't fit are the
    /// union's misfit when there are several, which is all the union's relation
    /// says.
    pub(crate) fn explain(&self, outcome: &Outcome) -> alias::Box<[Contradicted]> {
        let step = |parent: ObligationId, child: ObligationId| {
            (self.dependencies(parent).into_iter())
                .find(|dependency| dependency.obligation == child)
                .map(|dependency| dependency.step)
        };
        let member = |path: &[ObligationId]| match path {
            &[.., parent, child] => matches!(step(parent, child), Some(Step::UnionMember(_)))
                .then(|| path[..path.len() - 1].to_vec()),
            _ => None,
        };
        let contradictions: Vec<_> = (outcome.diagnostics.iter())
            .filter_map(|diagnostic| match diagnostic.issue {
                Issue::Contradiction(contradiction) => {
                    Some((diagnostic.path.clone(), contradiction))
                }
                Issue::Residual(_) => None,
            })
            .collect();
        let mut members: FxHashMap<Vec<ObligationId>, usize> = FxHashMap::default();
        for (path, _) in &contradictions {
            if let Some(union) = member(path) {
                *members.entry(union).or_default() += 1;
            }
        }
        let mut causes = Vec::new();
        for (path, contradiction) in contradictions {
            let cause = match member(&path) {
                Some(union) if members[&union] > 1 => (union, Contradiction::Outside),
                _ => (path, contradiction),
            };
            if !causes.contains(&cause) {
                causes.push(cause);
            }
        }
        (causes.into_iter())
            .map(|(path, contradiction)| Contradicted {
                contradiction,
                path: (path.iter().enumerate())
                    .map(|(index, &obligation)| Node {
                        step: index
                            .checked_sub(1)
                            .and_then(|parent| step(path[parent], obligation)),
                        sides: (self.relation(obligation).sides()).map(|(actual, expected)| {
                            (self.reify(actual).ok(), self.reify(expected).ok())
                        }),
                    })
                    .collect(),
            })
            .collect()
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by tests"))]
    pub(crate) fn provenance(&self, id: ConstraintId) -> &Provenance {
        &self.roots[id.0].provenance
    }

    /// Check the term IDs and return their kind
    fn kind(&self, term: Term) -> Kind {
        match term {
            Term::Infer(id) => self.variable(id).kind,
            Term::Skolem(id) => self.skolem(id).kind,
            Term::View(view) => {
                assert!(self.has_environment(view.environment));
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
        let relation = Relation::Subtype {
            actual,
            expected,
            fill: Fill::Arguments,
        };
        let obligation = self.enqueue(relation, false);
        self.root(obligation, provenance)
    }

    /// Register a call of `callee` as a diagnostic root (see [`Call`]).
    /// `arguments` is a schema, as [`Self::arguments_schema`] builds.
    /// Contradictions and derivations under [`Step::Arguments`] name an argument
    /// by its index among the arguments, through [`Step::Item`] and [`Step::Key`].
    /// An overloaded callee's overloads are tried against the arguments' twin
    /// (see [`Self::blind`]), with the result `Value`.
    pub(crate) fn constrain_call(&mut self, call: Call, provenance: Provenance) -> ConstraintId {
        assert_eq!(self.kind(call.callee), Kind::Type, "call kind mismatch");
        assert_eq!(
            self.kind(call.arguments),
            Kind::Schema,
            "call kind mismatch"
        );
        trace!(
            self,
            "#{}: {}",
            self.roots.len(),
            self.render_relation(Relation::Call(call))
        );
        let obligation = self.enqueue(Relation::Call(call), false);
        self.root(obligation, provenance)
    }

    fn root(&mut self, obligation: ObligationId, provenance: Provenance) -> ConstraintId {
        let id = ConstraintId(self.roots.len());
        self.roots.push(Root {
            obligation,
            provenance,
        });
        id
    }

    /// Intern a relation, reached guarded or not, and queue it only on first
    /// insertion.
    fn enqueue(&self, relation: Relation, guarded: bool) -> ObligationId {
        let key = (relation, guarded);
        if let Some(id) = self.interned(&key) {
            return id;
        }
        let id = self.push_obligation(key);
        self.schedule(id);
        id
    }

    /// Add a child subtype obligation and a labeled dependency from its parent.
    fn derive(&self, parent: ObligationId, actual: Term, expected: Term, step: Step) {
        assert_eq!(
            self.kind(actual),
            self.kind(expected),
            "derived constraint kind mismatch"
        );
        // A type argument's schema is the items a value holds, and functions
        // relate parameter lists. Bounds settle with the actual side filling
        // freely, the stricter reading.
        let fill = match (&step, self.relation(parent)) {
            (Step::Argument { .. }, _) => Fill::Contents,
            (Step::Parameters, _) => Fill::Parameters,
            (Step::Arguments | Step::BoundPropagation | Step::Assignment, _) => Fill::Arguments,
            (_, Relation::Subtype { fill, .. }) => fill,
            (_, Relation::Call(_)) => Fill::Arguments,
        };
        let relation = Relation::Subtype {
            actual,
            expected,
            fill,
        };
        self.derive_relation(parent, relation, step);
    }

    /// Add a child call obligation and a labeled dependency from its parent, a
    /// call whose callee it calls in its place
    fn derive_call(&self, parent: ObligationId, call: Call, step: Step) {
        assert_eq!(
            self.kind(call.callee),
            Kind::Type,
            "derived call kind mismatch"
        );
        self.derive_relation(parent, Relation::Call(call), step);
    }

    /// Add a child obligation and a labeled dependency from its parent
    fn derive_relation(&self, parent: ObligationId, relation: Relation, step: Step) {
        // Recursion through variables is the occurs check's, so a step through
        // one starts unguarded
        let guarded = self.tracks
            && !matches!(step, Step::BoundPropagation | Step::Assignment)
            && (self.guarded(parent) || self.guards(&step, self.relation(parent), relation));
        let child = self.enqueue(relation, guarded);
        if step != Step::BoundPropagation && step != Step::Assignment {
            (self.progress_mut(parent).active)
                .borrow_mut()
                .push((child, step.clone()));
        }
        self.link(parent, child, step);
    }

    /// Whether `step` from `parent` to `child` takes a value apart, as a
    /// recursive alias must on each cycle (see `elab::wellformed`): a class's
    /// type argument, a function type's part, or a schema item's type or key,
    /// but not an included schema, whose items are the schema's own
    fn guards(&self, step: &Step, parent: Relation, child: Relation) -> bool {
        match step {
            Step::Argument { .. } => true,
            Step::Parameters | Step::Return | Step::Input | Step::Output => {
                matches!(parent, Relation::Subtype { .. })
            }
            Step::Item(_) | Step::Key(_) => match child {
                Relation::Subtype { actual, .. } => self.kind(actual) == Kind::Type,
                Relation::Call(_) => false,
            },
            _ => false,
        }
    }

    fn link(&self, parent: ObligationId, child: ObligationId, step: Step) {
        self.add_dependency(parent, child, step);
    }

    /// Charge the work budget, permanently marking exhaustion on failure.
    fn spend(&self) -> Result<(), Residual> {
        self.gas.spend()
    }

    /// Reject a traversal that reaches the configured recursion limit.
    fn depth(&self, depth: usize) -> Result<(), Residual> {
        if depth >= self.depth {
            Err(Residual::Limit)
        } else {
            Ok(())
        }
    }

    /// Interpret a binder coordinate in its environment, checking the replacement kind.
    fn lookup(&self, mut environment: EnvironmentId, depth: u16, slot: u16, kind: Kind) -> Term {
        for _ in 0..depth {
            assert_ne!(environment.0, 0, "unbound reference");
            environment = self.frame(environment).parent;
        }
        assert_ne!(environment.0, 0, "unbound reference");
        let value = self.frame(environment).group[usize::from(slot)];
        assert_eq!(self.kind(value), kind, "substitution kind mismatch");
        value
    }

    /// Follow root environment substitutions and committed assignments without exposing declarations.
    pub(crate) fn resolve(&self, mut term: Term) -> Result<Term, Residual> {
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
            return Err(Residual::Malformed.into());
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

    /// Expose the head form of a solver term. Well-formedness rejects cycles of
    /// transparent declarations, but this doesn't rely on it: exposing the same
    /// declaration or application twice is recursive.
    fn head(&self, mut term: Term) -> Result<Head, Issue> {
        let mut exposed = FxHashSet::default();
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
                            return Err(Residual::Malformed.into());
                        }
                        return Ok(Head::Nominal(Nominal {
                            declaration: id,
                            arguments: vec![],
                            environment: EnvironmentId(0),
                        }));
                    }
                    // An overloaded function is its overloads, which its
                    // implementation doesn't describe
                    if decl.source.kind == DeclKind::Function && !self.db.overloads(id).is_empty() {
                        let signature = |id: DeclId| self.db.declaration(id).ty;
                        let overloaded = self.db.intern(Type::Overloaded {
                            overloads: self
                                .db
                                .overloads(id)
                                .iter()
                                .map(|&id| signature(id))
                                .collect(),
                            implementation: self.db.implementation(id).map(signature),
                            function: Some(id),
                        });
                        return Ok(Head::Structural(TypeView {
                            ty: overloaded,
                            environment: EnvironmentId(0),
                        }));
                    }
                    // Declarations are closed, so their environment is irrelevant.
                    if !exposed.insert(self.closed(view.ty)) {
                        return Err(Residual::Recursive.into());
                    }
                    term = self.closed(decl.ty);
                }
                Type::Meta(id) => {
                    if matches!(
                        self.db.ty(self.db.declaration(id).ty),
                        Type::Quantified { .. }
                    ) {
                        return Err(Residual::Malformed.into());
                    }
                    return Ok(Head::Meta(Nominal {
                        declaration: id,
                        arguments: vec![],
                        environment: EnvironmentId(0),
                    }));
                }
                Type::Apply { base, ref args, .. } => {
                    if !exposed.insert(term) {
                        return Err(Residual::Recursive.into());
                    }
                    // Scoped to this application: applications with different
                    // arguments can legitimately expose the same constructor.
                    let mut constructors = FxHashSet::default();
                    let mut base = view.child(base);
                    let (nominal, meta, constructor) = loop {
                        self.spend()?;
                        base = self.resolve(base)?;
                        let Term::View(base_view) = base else {
                            return Err(Residual::Malformed.into());
                        };
                        // A metatype is applied as its class is
                        let (id, meta) = match *self.db.ty(base_view.ty) {
                            Type::Decl(id) => (id, false),
                            Type::Meta(id) => (id, true),
                            _ => break (None, false, base_view),
                        };
                        let decl = self.db.declaration(id);
                        let constructor = TypeView {
                            ty: decl.ty,
                            environment: EnvironmentId(0),
                        };
                        if meta || decl.source.kind.nominal() {
                            break (Some(id), meta, constructor);
                        }
                        if !constructors.insert(id) {
                            return Err(Residual::Recursive.into());
                        }
                        base = Term::View(constructor);
                    };
                    let Type::Quantified { binders, body } = self.db.ty(constructor.ty) else {
                        panic!("generic argument arity mismatch");
                    };
                    let (arguments, environment) =
                        self.instantiate(binders, args, view, constructor.environment)?;
                    if let Some(declaration) = nominal {
                        let nominal = Nominal {
                            declaration,
                            arguments,
                            environment,
                        };
                        return Ok(match meta {
                            true => Head::Meta(nominal),
                            false => Head::Nominal(nominal),
                        });
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
                        // A skolem's projection is evaluated only as an item
                        // projection by a skolem key; otherwise it relates only
                        // as itself
                        Err(Residual::Escape) => match self.evaluate_skolem_items(view)? {
                            Some(evaluated) => term = self.closed(evaluated),
                            None => return Ok(Head::Structural(view)),
                        },
                        Err(residual) => return Err(residual.into()),
                    }
                }
                // A restriction is evaluated once its schema and key are
                // substituted
                Type::Restricted { .. } if view.environment != self.empty_environment() => {
                    match self.reify(term) {
                        Ok(reified) => term = self.closed(reified),
                        // A skolem key is known only by its bounds
                        Err(Residual::Escape) => match self.evaluate_skolem_restriction(view)? {
                            Some(schema) => term = self.closed(schema),
                            None => return Ok(Head::Structural(view)),
                        },
                        Err(residual) => return Err(residual.into()),
                    }
                }
                // A restriction that holds is its schema. One that can't be
                // evaluated relates only as itself.
                Type::Restricted {
                    schema,
                    restriction,
                } => match self.restricted(schema, restriction) {
                    Ok(schema) => term = self.closed(schema),
                    Err(Issue::Residual(_)) => return Ok(Head::Structural(view)),
                    Err(issue) => return Err(issue),
                },
                Type::Union(ref members)
                    if members.iter().any(|member| member.projected().is_some()) =>
                {
                    // Interning evaluates a projection only of a schema written
                    // out, so one of a transparent declaration is evaluated once
                    // exposed
                    if let Some(exposed) = self.exposed_projections(members)? {
                        term = self.closed(exposed);
                        continue;
                    }
                    // An item projection takes the solver to select by its key.
                    // One that can't be evaluated is left for the rules of unions.
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

    /// A closed union with each projection's schema exposed (see
    /// [`Self::exposed_schema`]), or `None` if none has a transparent
    /// declaration to expose
    fn exposed_projections(&self, members: &[UnionMember]) -> Result<Option<TypeId>, Issue> {
        let mut changed = false;
        let mut exposed = Vec::with_capacity(members.len());
        for &member in members {
            match member.projected().map(|schema| self.exposed_schema(schema)) {
                Some(Ok(Some(schema))) => {
                    changed = true;
                    exposed.push(member.with(schema));
                }
                // A projection whose schema can't be exposed stays as it is
                Some(Ok(None) | Err(Issue::Residual(_))) | None => exposed.push(member),
                Some(Err(issue)) => return Err(issue),
            }
        }
        Ok(changed.then(|| self.db.intern(Type::Union(exposed.into()))))
    }

    /// A closed schema with each transparent declaration and restriction it is
    /// or includes replaced by what it stands for, or `None` if it has none. A
    /// restriction that doesn't hold is its issue. A schema
    /// included along its own path of inclusions is recursive, which
    /// well-formedness rejects, but this doesn't rely on it.
    pub(super) fn exposed_schema(&self, schema: TypeId) -> Result<Option<TypeId>, Issue> {
        self.expose_schema(schema, &mut Vec::new())
    }

    fn expose_schema(
        &self,
        schema: TypeId,
        path: &mut Vec<TypeId>,
    ) -> Result<Option<TypeId>, Issue> {
        self.spend()?;
        self.depth(path.len())?;
        if path.contains(&schema) {
            return Err(Residual::Recursive.into());
        }
        // A restriction that holds is its schema
        if let Type::Restricted {
            schema: restricted,
            restriction,
        } = *self.db.ty(schema)
        {
            let held = self.restricted(restricted, restriction)?;
            path.push(schema);
            let exposed = self.expose_schema(held, path);
            path.pop();
            return Ok(Some(exposed?.unwrap_or(held)));
        }
        let mut exposed = None;
        let mut current = schema;
        let transparent = match *self.db.ty(schema) {
            Type::Decl(id) => !self.db.declaration(id).source.kind.nominal(),
            Type::Apply { .. } => true,
            _ => false,
        };
        if transparent {
            let Head::Structural(view) = self.head(self.closed(schema))? else {
                return Ok(None);
            };
            current = self.reify(Term::View(view))?;
            exposed = Some(current);
        }
        let Type::Schema(items) = self.db.ty(current) else {
            return Ok(exposed);
        };
        path.push(schema);
        let mut changed = false;
        let mut rebuilt = Vec::with_capacity(items.len());
        for item in items.iter() {
            let element = match item.element {
                Element::Include(inner) => match self.expose_schema(inner, path) {
                    Ok(Some(inner)) => {
                        changed = true;
                        Element::Include(inner)
                    }
                    Ok(None) => item.element.clone(),
                    Err(issue) => {
                        path.pop();
                        return Err(issue);
                    }
                },
                ref element => element.clone(),
            };
            rebuilt.push(SchemaItem {
                multiplicity: item.multiplicity,
                element,
            });
        }
        path.pop();
        Ok(match changed {
            true => Some(self.db.intern(Type::Schema(rebuilt.into()))),
            false => exposed,
        })
    }

    /// A term's head, with a metatype's widened to `Type[C[A]]`'s, as it
    /// relates to anything but a metatype
    fn widened_head(&self, term: Term) -> Result<Head, Issue> {
        let head = self.head(term)?;
        self.widened(head)
    }

    /// What a metatype's head reaches: `Type[C[A]]`'s. Any other is itself.
    fn widened(&self, head: Head) -> Result<Head, Issue> {
        match head {
            Head::Meta(nominal) => self.head(self.metatype_reach(&nominal)?),
            head => Ok(head),
        }
    }

    /// What a metatype reaches, `Type[C[A]]`, given its class applied to its
    /// arguments
    fn metatype_reach(&self, nominal: &Nominal) -> Result<Term, Issue> {
        // The class's arguments are the environment's, in order
        let args = (nominal.arguments.iter().enumerate())
            .map(|(slot, &argument)| {
                Argument::Positional(self.db.intern(Type::Bound {
                    reference: BoundRef::new(0, slot),
                    kind: self.kind(argument),
                }))
            })
            .collect();
        let meta = self.db.meta(nominal.declaration, args);
        let widened =
            (self.db.widen_meta(meta)).ok_or(Residual::MissingIntrinsic(Intrinsic::Type))?;
        Ok(self.view(widened, nominal.environment))
    }

    /// Walk supertypes in MRO order, carrying substitutions through each edge.
    /// A failed first match is final. Earlier incomplete branches also stop the
    /// search: skipping one could choose an ancestor out of MRO order.
    fn ancestor(
        &self,
        current: Nominal,
        target: DeclId,
        path: &mut FxHashSet<DeclId>,
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
        path: &mut FxHashSet<DeclId>,
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
        path: &mut FxHashSet<DeclId>,
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
        path: &mut FxHashSet<DeclId>,
        depth: usize,
        visit: &mut impl FnMut(Visited<'_>) -> Result<Option<T>, Issue>,
    ) -> Result<Option<T>, Issue> {
        self.mro_flagged(current, runtime, true, path, depth, &mut |visited, _| {
            visit(visited)
        })
    }

    /// [`Self::mro`], also telling `visit` whether every edge on the path to what
    /// it visits is one the runtime inherits, starting from `inherited`
    fn mro_flagged<T>(
        &self,
        current: Nominal,
        runtime: bool,
        inherited: bool,
        path: &mut FxHashSet<DeclId>,
        depth: usize,
        visit: &mut impl FnMut(Visited<'_>, bool) -> Result<Option<T>, Issue>,
    ) -> Result<Option<T>, Issue> {
        self.depth(depth)?;
        self.spend()?;
        if let Some(found) = visit(Visited::Nominal(&current), inherited)? {
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
            let inherited = inherited && supertype.runtime;
            let found = match head {
                Head::Nominal(next) => {
                    self.mro_flagged(next, runtime, inherited, path, depth + 1, visit)?
                }
                _ => visit(Visited::Structural, inherited)?,
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
            scope = self.extent(scope).parent;
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
                while !self.gas.is_exhausted() && !self.queue.is_empty() {
                    let mut batch = std::mem::take(&mut self.queue);
                    for id in batch.drain() {
                        let node = self.progress_mut(id);
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
                        if self.gas.is_exhausted() {
                            break;
                        }
                    }
                }
                if self.gas.is_exhausted() {
                    break;
                }
                let mut assigned = false;
                for id in self.variable_ids() {
                    if self.clean(id) && self.assignment(id).is_none() {
                        match self.try_assign(id) {
                            Ok(changed) => assigned |= changed,
                            Err(Residual::Limit) => {
                                self.gas.exhaust();
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
            if self.gas.is_exhausted() {
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
        let mut pending: Vec<InferVarId> = self
            .variable_ids()
            .filter(|&id| {
                (self.closed
                    || self.variable(id).scope != ScopeId(0)
                    || self.owned.is_some_and(|owned| id.0 >= owned))
                    && self.assignment(id).is_none()
            })
            .collect();
        pending.sort_by_key(|&id| std::cmp::Reverse(self.extent(self.variable(id).scope).depth));
        for &id in &pending {
            if self.default_term(id, false).is_ok() {
                return true;
            }
        }
        for &id in &pending {
            if self.variable(id).kind == Kind::Type && self.bounds(id).lower().next().is_none() {
                self.state_mut(id).defaulted.set(true);
                self.commit(id, self.closed(self.db.bottom()));
                return true;
            }
        }
        false
    }

    /// Whether a cycle of premises proves its judgments coinductively: the
    /// obligations on it in order, each reached by the step before it from the
    /// one before, and the last back to the first by `back`. A closed judgment
    /// is assumed while it's proven, so one revisited past a guarded step (see
    /// [`Self::guards`]) holds. Recursion through variables is the occurs
    /// check's.
    fn coinductive(
        &self,
        cycle: &[ObligationId],
        steps: &[Option<Step>],
        back: Option<&Step>,
    ) -> bool {
        let edges = (cycle.iter().zip(cycle.iter().skip(1).chain(cycle.first())))
            .zip(steps.iter().map(Option::as_ref).chain([back]));
        cycle.iter().all(|&id| self.closed_relation(id))
            && edges.into_iter().any(|((&from, &to), step)| {
                step.is_some_and(|step| self.guards(step, self.relation(from), self.relation(to)))
            })
    }

    /// Whether an obligation relates closed types, as a judgment assumed while
    /// it's proven must
    fn closed_relation(&self, id: ObligationId) -> bool {
        match self.relation(id) {
            Relation::Subtype {
                actual, expected, ..
            } => self.reify(actual).is_ok() && self.reify(expected).is_ok(),
            Relation::Call(_) => false,
        }
    }

    /// Trace a root through its dependencies to collect issues and determine its status.
    fn outcome(&self, constraint: ConstraintId) -> Outcome {
        let root = self.roots[constraint.0].obligation;
        let mut diagnostics = vec![];
        // Iterative DFS keeps reporting safe even when the obligation graph is
        // deep. Each entry carries the step that reached it.
        let mut stack = vec![(root, false, None)];
        let mut active = FxHashSet::default();
        let mut visited = FxHashSet::default();
        let mut path = vec![];
        let mut steps = vec![];
        while let Some((id, exit, step)) = stack.pop() {
            if exit {
                active.remove(&id);
                path.pop();
                steps.pop();
                continue;
            }
            if active.contains(&id) {
                let start = path.iter().rposition(|&on| on == id).unwrap();
                if !self.coinductive(&path[start..], &steps[start + 1..], step.as_ref()) {
                    // A closed cycle needs a guard that well-formedness
                    // ensures; one through an open judgment rests on inference
                    let residual = match path[start..].iter().all(|&on| self.closed_relation(on)) {
                        true => Residual::Recursive,
                        false => Residual::Cycle,
                    };
                    let mut cycle = path.clone();
                    cycle.push(id);
                    diagnostics.push(Diagnostic {
                        issue: residual.into(),
                        path: cycle,
                    });
                }
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            active.insert(id);
            path.push(id);
            steps.push(step);
            match self.progress(id).state.get() {
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
            stack.push((id, true, None));
            let premises = self.progress(id).active.borrow();
            for dependency in (self.dependencies(id).into_iter())
                .filter(|d| premises.contains(&(d.obligation, d.step.clone())))
            {
                stack.push((dependency.obligation, false, Some(dependency.step)));
            }
        }
        // Historical edges explain contradictions, but are not current proof
        // premises. A path through an assignment explains a contradiction only by
        // what the assignment was drawn from, so one is taken only where no other
        // reaches it.
        for assignments in [false, true] {
            let mut history = vec![(root, vec![root])];
            let mut seen = FxHashSet::default();
            while let Some((id, path)) = history.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if let State::Issue(issue @ Issue::Contradiction(_)) = self.progress(id).state.get()
                    && !diagnostics
                        .iter()
                        .any(|d| d.path.last() == Some(&id) && d.issue == issue)
                {
                    diagnostics.push(Diagnostic {
                        issue,
                        path: path.clone(),
                    });
                }
                for dependency in self.dependencies(id) {
                    if !assignments && dependency.step == Step::Assignment {
                        continue;
                    }
                    let mut next = path.clone();
                    next.push(dependency.obligation);
                    history.push((dependency.obligation, next));
                }
            }
        }
        if self.gas.is_exhausted() {
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
mod call;
mod callable;
mod conform;
mod item;
mod lattice;
mod member;
mod narrow;
mod nest;
mod reduce;
mod schema;
mod unpack;

use alternatives::Alternatives;
pub(crate) use callable::{Constructor, Signature, bound_method};
pub(crate) use conform::{Inheritance, Masked, Requirement, RequirementKind};
pub(crate) use lattice::Widening;
pub(crate) use member::{Access, Found, FoundKind, Lookup, Signatures};
pub(crate) use narrow::Target as NarrowTarget;
pub(crate) use nest::Gas;
use nest::{Nesting, Parent, Revisit};
pub(crate) use unpack::{PatternShape, Reason};

#[cfg(test)]
mod tests;
