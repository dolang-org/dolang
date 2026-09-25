# Compiler Frontend Architecture (dolang-compile)

The `dolang-compile` crate transforms Do source code into bytecode through a
multi-phase pipeline: lex → parse → elaborate → lower → emit. Lexical analysis
converts indentation into explicit indent/dedent tokens. Parsing uses recursive
descent with Pratt expression parsing to build an AST. Elaboration performs name
resolution and validates control flow. Lowering converts the AST to a control
flow graph. Emission translates the CFG into bytecode. The `Compiler` type
orchestrates the process and maintains shared tables (source lines, symbols,
interned strings, constant pool, diagnostics).

Elaboration stores binding provenance directly in variables. Resolutions use
lexical scope depth and variable index; compilation needs no document table.
With `Config::document(true)`, a separate pass annotates the elaborated AST with
document node IDs and builds a table retained by `Unit`. Borrowed scope frames
follow the recursive traversal and annotate the AST's variable storage directly;
lexical scope depth is independent of document parentage. Token visits read the
annotations; without indexing they emit no node IDs. Lowering does not consume
document metadata.

Type names resolve apart from values, when documenting, to a `TypeRes`: a depth
counting binder groups and lexical scopes outward, and an entry of the frame it
reaches. A lexical scope numbers its type-only declarations (type-only imports,
`@let` aliases and `@class` protocols) separately from its variables, as
`Stmt::type_decls` enumerates them. Type resolution and document indexing push
the same frames, so a resolution means the same to both.

## Type checking foundations

`typeck/type.rs` holds the canonical type and declaration database. It is not
yet connected to AST elaboration or bytecode compilation. The database lives
inside this crate while its interfaces develop; its only compiler dependency
is the source span representation.

`DeclId` identifies an allocated source occurrence. Allocation reserves an empty
slot in a `Vec<Option<Declaration>>`, allowing forward references before the
complete declaration is populated. Population is one-time. Sealing asserts that
all slots are populated and converts the table to `alias::Box<[Declaration]>`,
preventing further declaration or unit changes. Sealing twice panics. Kind
checks involving empty slots are retained until sealing, including those from
normalized-away nodes. Types and symbols can still be interned after sealing.
Source spans pair a unit ID with byte offsets, and binder names/spans are
parallel metadata for the definition's outer structural binder group. Unit IDs
are allocated from a counter and checked when declarations are populated.
Filenames and local symbol mappings belong to upper layers. Ordinary symbols are
interned by spelling; callers can allocate fresh symbols separately when source
identity must be preserved.

`TypeId` identifies a normalized structural expression in one database. Ordinary
types such as `Int` and `Sym` are declaration references, not builtin nodes.
Literal nodes describe exact values only. Optional intrinsic slots associate
`Union`, `Int`, `Bool`, `Sym`, `Nil`, and `Str` with their stub types. Each slot
may be set once before sealing; missing associations are allowed. Elaboration
supplies these associations, and the solver can use the literal backing types to
enter the declared supertype hierarchy. Schemas and ordinary types share the ID
domain but carry distinct kinds; packs are schemas, not a third kind. There are
no solver variables, skolems, flow variables, or missing-annotation nodes. Later
elaboration will decide how checker strictness interprets omissions.

Quantifiers own structural binder groups. References use relative group depth
and declaration-order slot, each a checked `u16`. The whole group is in scope in
its bounds, defaults, and body. Empty groups normalize away. Each bound has the
same kind as its binder. Elaboration resolves kinds before database
construction: an explicit schema bound makes a variable a schema, propagating
through aliases; otherwise an unbounded variable or one bounded by a type is a
type. Rest binders carry schemas, and elaboration converts their element bounds
to schema bounds according to the rest mode (positional, keyed, or both). Open
references carry their expected kind; checking them against an environment and
checking generic argument matching remain consumer responsibilities.

Database construction, lifecycle, kind, and representation-limit violations
panic as compiler invariant failures. Parser limits must reject excessive source
complexity before database construction; this layer does not enforce those
limits. Exposure cycles and references preventing scope removal remain
recoverable, with operation-specific error types.

Interning performs local shape/kind checks and structural union normalization,
not subtype reasoning. Bottom is the empty union; top has an explicit node.
Both are interned and cached when the database is created. Type interning and
shifting accept shared database references; arena storage keeps borrowed types
stable while the interning index uses interior mutability. Future elaboration
will recognize `std.Value` as judgmentally equal to top; `Empty` needs only its
ordinary alias to `Union[]`.
Union expansions can remain symbolic until a consumer
supplies their schema arguments. Declaration wrappers are not normalized away.
Exposure follows transparent head references and reports direct cycles, stopping
at nominal declarations, quantifiers, applications, and other structural forms.
Recursive graphs are representable; this does not establish recursive typing
rules. Generic exposure returns the definition intact in its defining scope.

Structural walking and rebuilding report quantifier boundaries and visit
bounds/defaults along with all other children. Declaration references are
leaves; definitions and declared supertypes must be visited explicitly. A
nominal definition's supertypes are interpreted inside its outer structural
binder group. Shifting inserts/removes groups at a cutoff and refuses to remove
a group that is referenced. Memoization includes the scope as well as the node
identity.

All IDs are database-local and must not be mixed between databases. Structural
equality of open types does not imply equality of their interpretations. Future
solver and dataflow consumers must retain interpretation environments for open
types during exposure and substitution. Intrinsic recognition for `std.Value`
and `std.Union`, source elaboration, and solver judgments are follow-up work.

## Standalone subtype solver

`typeck/solver.rs` consumes a sealed database through a shared reference. It
accepts synthetic subtype constraints without depending on AST elaboration,
runtime types, or dataflow. Canonical types can still be interned during
solving.

A solver term is an inference-variable ID or a `TypeView`: a canonical type ID
paired with a solver-owned environment ID. Immutable, interned environment
frames each supply one binder group's substitutions and point to an outer
frame. Substitutions retain their own environments. Resolving a free reference
switches to its replacement's environment; structural quantifiers protect their
local references. This permits open arguments and nested quantifiers without
putting inference IDs into canonical types or eagerly rebuilding types.

Elaboration lambda-lifts captured binders of nested declarations, so declaration
definitions and supertypes are closed outside their own generic group. Exposure
resets to the declaration's empty defining environment; generic instantiation
extends it with the supplied arguments. The solver does not scan declarations
for free references on exposure. Local telescope operations assert that binder
references have an interpretation when they are used.

The supported subtype fragment includes top and empty-union bottom, contextual
structural identity, transparent declaration exposure, literal identity, and
declared nominal inheritance. Literal-to-nominal judgments enter the hierarchy
through the corresponding registered intrinsic type; a missing registration
is residual. Distinct singleton literals and exhausted, concrete nominal
searches can establish contradictions.

Generic applications currently require all arguments explicitly, with fixed
positional ordinary-type binders. Subtype judgments assume well-formed inputs:
callers establish argument bounds and validate declaration bodies and supertypes
under their binder assumptions. Exposure substitutes arguments without
generating binder-bound obligations, including on unselected inheritance paths.
A separate well-formedness checker remains future work. Matching constructors
decompose according to declared variance. Inheritance walks left-to-right,
depth-first, carrying substitutions through each edge. The first matching
declaration wins, even if its arguments contradict the expected arguments or
remain unresolved. An earlier incomplete branch cannot be skipped to find a
later match. This follows runtime member lookup's left-wins ordering and avoids
speculative inference or combining bounds from alternative paths.

Monomorphic functions support required positional parameters, contravariant
parameter types, covariant results, and arity checks. Ambient input/output
declarations must match in presence and contextual structural identity;
other channel judgments remain residual and are retried when their inference
variables receive assignments. Union-left judgments require every member;
union-right judgments accept a member proved by an isolated, closed subtype
query. Alternative queries cannot add inference bounds or diagnostic edges to
the calling solver. Expanded union packs and alternatives that cannot be proved
remain residual. Schema inclusion, optional/keyed/variadic matching, higher-rank
rules, and generic keyword/default/rest argument matching remain deferred.
Contextual identity and top/bottom rules can still settle some judgments
involving otherwise unsupported forms.

### Assignments and fixed point

Each variable retains append-only lower/upper bound terms and all introducing
obligations. Opposite bounds generate ordinary subtype obligations, propagating
constraints through variable relationships. Assignments are stored separately
from these sets; neither bounds nor canonical types are rewritten. A stored
`Array[?A]` view is interpreted through `?A`'s assignment when used.

The assignment policy commits only forced, fully resolved solutions. The union
of the currently reifiable lower bounds is a candidate `C`. Every reifiable
upper bound must admit `C`, and at least one must also be proved a subtype of
`C`. Thus the constraints force equivalence to `C`; a one-sided lower bound or
an arbitrary satisfiable interval does not select a solution. Bounds containing
unsolved variables remain obligations and are revisited after commitments.
Unsupported concrete compatibility checks defer commitment. There are no
intersection nodes, speculative assignments, rollback, or defaults to top or
bottom. Closed proof queries reuse the subtype engine and charge their work to
the caller's lifetime budget.

Exact candidate dependencies receive a scope-aware occurs check. Recursive
substitutions remain recursive residuals; variable-only cycles remain unsolved
unless concrete bounds force them. Assignments contain only closed canonical
types, so they cannot introduce assignment cycles. Declaration wrappers remain
opaque to this check: supported recursion through declarations is distinct from
substitution recursion.

Obligations are interned by their original operands, including environments,
but processing is repeatable. Each has a queued flag and a replaceable current
reduction state. Scope-aware traversal subscribes obligations to variables
throughout contextual types, including nested binders, function channels, and
replacement environments. Assignments queue subscribers and conservatively mark
all unsolved variables' candidates dirty. New bounds also mark their variable
dirty. Reduction and candidate evaluation alternate until neither produces work.
Adding constraints resumes the fixed point; an unchanged solve does no work.
This fixed point remains independent of CFG/dataflow analysis.

### Reporting and reification

Each submitted root retains actual/expected source spans. Obligations retain
original operands and historical labeled edges for arguments, parameters,
returns, union members, bound propagation, and assignments. Current proof
premises are tracked separately and replaced on reprocessing. Historical edges
explain contradictions to every contributing root, but historical cycles and
obsolete residuals do not prevent a current proof. Cycles in current proof
premises remain unresolved. Reports distinguish proven, contradicted, and
unresolved roots; quiescence alone is not proof.

`solution` exposes a committed canonical type, `solution_sources` exposes its
supporting obligations, and `unresolved` enumerates variables available for
later inference or generalization. `reify` rebuilds a fully resolved contextual
view using the database's scope-aware child mapping. It preserves local binder
coordinates, bounds/defaults, declaration wrappers, and each replacement's own
environment. All existing structural forms can be reconstructed; reconstruction
does not imply subtype support for those forms. An unsolved variable produces an
inference residual. Solver IDs never enter the canonical database.

Work and depth limits produce residuals. The work budget applies to the solver's
lifetime; exhaustion prevents a complete proof. Invalid IDs, environments,
substitution kinds, and unsealed databases are API errors and panic. Transparent
declaration exposure cycles and generic arity mismatches also panic;
well-formedness checking must eliminate them before solving. Omitted arguments
with binder defaults remain residual.

Environments use an immutable `intern::Table`; obligations use stable `MonoVec`
storage and a `MonoHashMap` relation index. Bound terms, sources, and
subscribers use monotonic collections. Assignments and scheduling flags use
interior mutability; current proof premises are replaced between reductions.
Setup needs exclusive access, while reduction and insertion preserve borrowed
solver state.

`Intrinsic::Func` associates the runtime nominal function supertype with the
checker. Structural function types, including quantified function signatures,
are subtypes of this registered type and its declared supertypes. This rule does
not desugar function syntax into a nominal application or assign generic
semantics to `Func`; those remain undecided. A missing registration is residual.

## Checking units and diagnostic locations

`typeck::Builder` collects the units to check together. A unit must have
compiled without failure and with `Config::typecheck`, so that elaboration and
type resolution have run; anything else is rejected. `Config::typecheck` runs
type resolution without building the document index. Adding a unit returns its
`UnitId`, which is meaningful only within that check; paths do not identify
units. A module name may be added only once. `Builder::check` consumes the
builder and returns a `Check`, which yields the type checker's diagnostics. The
units' own diagnostics stay with the units.

A `SourceSpan` is a span with an optional unit. A unit's own diagnostics have
no unit and are relative to that unit. Type checker diagnostics name a unit in
every location, so one diagnostic can point into several units. Mapping a unit
to its file and rendering diagnostics belong to callers. Token and document
spans remain local.

No semantic checking runs yet, so a `Check` has no diagnostics. Callers
assemble modules and stubs without executing imports. Embedding-provided
ambient endpoint types will require an explicit caller-selected environment on
the builder.
