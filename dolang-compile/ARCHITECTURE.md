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

`typeck/type.rs` holds the canonical type and declaration database. Elaboration
populates it (see [Elaborating declarations](#elaborating-declarations)); it is
not connected to bytecode compilation. The database lives inside this crate
while its interfaces develop; its only compiler dependency is the source span
representation.

`DeclId` identifies an allocated source occurrence. Allocation reserves an empty
slot in a `Vec<Option<Declaration>>`, allowing forward references before the
complete declaration is populated. Population is one-time. Sealing asserts that
all slots are populated and converts the table to `alias::Box<[Declaration]>`,
preventing further declaration or unit changes. Sealing twice panics. Kind
checks involving empty slots are retained until sealing, including those from
normalized-away nodes. Types and symbols can still be interned after sealing.
Source spans pair a unit ID with byte offsets, and binder names/spans are
parallel metadata for the definition's outer structural binder group, including
whether each slot was lifted from an enclosing declaration, written, or an
implicit ambient binder. A class or protocol also records its members by name:
fields with their type, scope and visibility, and methods by their function
declaration. An overloaded function records each of its signatures, which are
declarations of their own. Unit IDs
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
no solver variables, skolems, or flow variables. A rigid names one slot of a
declaration's binder group, held abstract while that declaration is checked. It
is closed, since a declaration has exactly one group, and is interned on demand
after sealing, but a declaration never contains one. `abstract_rigids` turns a
declaration's rigids back into references to its group, and reports any other
declaration's rigid as having escaped. `Unknown` is the dynamic type
that an omitted `def` annotation stands for, and what an erroneous site is
interned as. It is interned once like top, with a schema-kinded twin for
erroneous schema positions, and a union keeps it as an ordinary member. How
checker strictness treats an omission is decided where it was written, not by
finding `Unknown`.

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
stable while the interning index uses interior mutability. Elaboration interns
`std.Value` as top; `Empty` needs only its ordinary alias to `Union[]`.
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
types during exposure and substitution.

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

`Unknown` is consistent with every type or schema of its kind, in either
direction, wherever a judgment meets it: at the root, under nominal arguments
of any variance, in function parameters and results, and as a union member. The
judgment is proven; the checker's strictness flags omissions where they were
written, not where `Unknown` is found. Consistency is not transitive: `Int` is
consistent with `Unknown` and `Unknown` with `Str`, but `Int` is not a subtype
of `Str`, so no rule chains through it. A union keeps `Unknown` as an ordinary
member, so every other member of a union on the left must still hold.

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
declarations must match in presence and either contextual structural identity
or `Unknown` on one side; other channel judgments, including `Unknown` nested
within a channel, remain residual and are retried when their inference
variables receive assignments. Union-left judgments require every member;
union-right judgments accept a member proved by an isolated, closed subtype
query. Alternative queries cannot add inference bounds or diagnostic edges to
the calling solver. Expanded union packs and alternatives that cannot be proved
remain residual. Schema inclusion, optional/keyed/variadic matching, higher-rank
rules, and generic keyword/default/rest argument matching remain deferred.
Contextual identity and top/bottom rules can still settle some judgments
involving otherwise unsupported forms.

### Rigids

A solver checks declarations it is told to assume. `rigid_environment` assumes a
declaration and interprets its group as its rigids, so its type, supertypes and
members viewed there are what is checked. Only assumed declarations' bounds are
facts: a rigid's bound is its binder's bound with the declaration's rigids
substituted, and a rest binder without one is bounded by its mode's shape,
`{*Value}`, `{**Sym: Value}` or both. Exposure and instantiation never assume
the bounds of anything else. Probes inherit the assumed declarations.

A rigid is a subtype of itself, top and `Unknown`, and bottom and `Unknown` are
subtypes of it. Otherwise, an assumed rigid on the left reduces to its bound,
labeled so a strictness policy can find reductions through an omitted ambient
channel's default bound. An unbounded one, or one on the right, contradicts the
judgment. A union on the right is proved by a member identical to the left side
before alternatives are probed. Rigids are closed, so they reify to themselves
and assignments may contain them. A rigid of a declaration not assumed has
escaped its own check: it is related only to itself and top, and reifying it is
residual.

`reach` walks a term to a target declaration through the substitution-carrying
inheritance walk, continuing through an assumed rigid's bound, and returns the
target's arguments. It reports a term that doesn't reach the target, and
`Unknown` as reaching anything.

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
an arbitrary satisfiable interval does not select a solution. Consistency is
not antisymmetric, so a bound containing `Unknown` must still admit `C` but is
never part of `C` and never forces it; this also keeps a variable from being
assigned `Unknown`, which would chain consistency transitively. Bounds containing
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

Callers assemble modules and stubs without executing imports.
Embedding-provided ambient endpoint types will require an explicit
caller-selected environment on the builder.

## Elaborating declarations

`Builder::check` allocates each unit in the database, in the order units were
added, then runs the passes in `typeck/elab` over common tables and the frozen
syntax trees. The tables refer to declaration nodes in place. Passes visit units
in a fixed order, modules by name and then scripts by path, so declarations and
diagnostics do not depend on the order units were added.

Collection walks each unit with the frames type resolution pushed, binder groups
and lexical scopes, so each type name's `TypeRes` nominates its target without a
side table. Entering a scope allocates a `DeclId` for each class, protocol,
alias and function its statements declare; a def's `@def` overloads join its
function, as the methods of one name do in a class body. Lambdas and field
initializers are closure declarations, and each declaration records the one
enclosing it.

A module's exports are its root block's `pub` declarations and bindings, and the
names its `pub` imports re-export. Once every unit is collected, each type name
that goes through an import follows the exports of the units checked, and the
referent of every type name is recorded by the span of its head. Imports and
renames are chased away but aliases are not, so `Pair[Int]` refers to the
`Pair` alias; an item of a module no unit provides is external. Each transparent
alias then has its underlying head found by following alias chains, in
declaration order. Import cycles, alias cycles and imports of names a checked
module does not export are diagnosed.

`Check` keeps the tables. Its hidden `judgments` method reports what they
concluded about each span of a unit, such as a type name's referent or an
alias's head, as text naming declarations by qualified name rather than by ID.
The type-checking tests in `language-regression/tests/typeck` assert these with
annotations in fixture source.

Kinds come from declarations only, never from uses. A variadic binder is a
schema and a keyword binder a type; any other binder has its bound's kind, and a
transparent alias its body's. These equations are solved by union-find over one
variable per binder and alias, so alias chains and cycles across units need no
ordering. A kind nothing determines is a type. One that only an external or
erroneous name could have determined, including an alias on a cycle, is
flexible: it is recorded as a type but never reported as a mismatch. Every type
expression is then checked against the kind its use requires. A rest binding's
annotation is either kind, giving each item's type or the whole pack, and a
variadic binder's bound likewise. Type arguments are matched to the binders
they fill: positional arguments in order and then to a variadic binder,
keyword arguments by name, and an expansion `...X` of either kind, a type
expanding as any number of it. A declaration whose only binder is a schema
takes `Foo[T]` and `Foo[K, V]` for its items. Applying a schema, a binder or a
declaration without binders is an error, as is naming a value or module as a
type.

Signature completion fills each def and method signature with the defaults for
what it omits, the same for public and private definitions. An omitted
parameter, rest or return annotation is `Unknown`, a rest's as each of its
items. An omitted ambient channel is an implicit binder with no bound,
following the signature's written binders. A method's unannotated receiver is
its class applied to its own binders, except on a `class` or `static` method.
A function type written without channels in a def's signature or body, but not
in a nested class or alias, shares that def's channels; elsewhere they are
`Unknown`. A closure is populated with its annotations and `Unknown` for what
it omits, channels included; CFG flow infers the omissions separately, without
changing the database.
Top-level declarations of a checked `std` module named `Value`, `Phantom`,
`Union`, `Func`, `Int`, `Bool`, `Sym`, `Nil` and `Str` are designated for
special treatment; the same name in another module is only a lookalike. The
`kind`, `sig`, `ambient` and `designated` judgments report these results.

Variance is inferred for every binder, and for each outer binder a nested
declaration uses, before anything is interned, since a quantified type's binders
carry it. A def or method uses its parameters, rest parameters and ambient
channels contravariantly and its result covariantly; an instance method's
receiver, annotated or not, does not count. A class or protocol uses its
supertypes covariantly, its public fields invariantly, since they are mutable,
and each binder as its public and special methods use it. Private members and
`(init)` do not count, as Scala's `private[this]` members and constructors do
not: a private member is reached only through a `self` parameter, so whatever it
stores or returns passes through a method that counts, and `(init)` runs only on
an object being constructed. This relies on elaboration refusing `.#` on
anything but a `self` parameter, and `.(init)` calls outside an `(init)` body or
on anything but its `self`. A field whose type is an application of `Phantom`
always counts, whatever its visibility, using its arguments covariantly as
Rust's `PhantomData` does, so `Phantom[(T -> nil)]` marks a class
contravariant. A transparent alias uses
its body covariantly; `Union` and `Phantom` take their binders covariantly, and
any other opaque alias uses none. A binder used in a bound of its own group is
invariant, and an outer binder used in the bound of a nested group is used
contravariantly there. Defaults and bodies do not count. A type argument is used
as the binder it fills varies, matched as kind checking matches it, and one
whose binder is unknown is invariant. A type declared within a generic
declaration takes the outer binders it is lifted over as implicit arguments.
These equations are solved by a worklist for their least solution, which is
unique whatever the order. A binder with no use, including one used only through
itself, is then invariant, as is any use through it, and a second round
propagates that. The `variance` judgment reports a binder's variance, and
`captured` a nested declaration's outer binders.

Every declaration is closed. Before variance, the captures pass finds the outer
binders each is lifted over: those it names anywhere, in its signature, members
or body, including the implicit binders a function type written without channels
takes, and those that what it names, or what is nested in it, is lifted over. A
method is lifted over all of its class's binders, and a lifted binder keeps its
bound, so a declaration also takes what its lifted binders' bounds name.

Population then interns each declaration signature over one flat group: the
binders it is lifted over, outermost first, then its written binders, then its
implicit ones. A lifted binder takes the variance its declaration uses it with,
or is invariant. A reference to a declaration passes the binders it is lifted
over as leading arguments, then one argument per written binder as type argument
matching places them. A variadic binder's arguments become a schema. An omitted
argument takes its binder's default, substituted with the arguments before it,
since a later binder is not yet known there. Arguments after an expansion whose
reach is unknown stay as written, for the solver to leave residual. Each written
type is also interned in its group, by its span, for flow analysis. Classes
record their members, the signatures of an overloaded def or method are
declarations of their own, and designated declarations set the database's
intrinsics.

Population diagnoses what needs no solver: missing type arguments, a generic
name used without them, an inheritance cycle, and a binder group or type too
large to represent. Each erroneous site, whenever it was diagnosed, is interned
as `Unknown` of the kind its position requires, and an alias on a cycle gets an
`Unknown` body, so the sealed database keeps every structural invariant the
solver assumes. It is sealed but not validated; checking well-formedness is a
separate step. `Check`'s hidden `smoke` method relates every type the database
holds to itself and to top, to show that the solver judges it without
panicking. The `quantifier`, `decl`, `member` and `type` judgments report what
was interned.
