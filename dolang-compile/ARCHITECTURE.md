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
declaration. A private member is named apart from public ones, as the runtime
gives it a symbol of its class's own, and instance and type-object members are
separate namespaces; otherwise the first member of a name wins. A method's
decorators decide what it becomes. Only `class` and `static` mean something
fixed by syntax; until decorator applications are evaluated, std's `getter` and
`setter`, found by what their names resolve to, make a method half of a computed
field's property, and any other decorator leaves a member of unknown type. Each
implementation of a method name is a function of its own, so a getter and a
setter are two. An overloaded function records its `@def` signatures, which are
declarations of their own; its own ID is its implementation, and without one,
which name resolution reports, its first `@def`. Unit IDs
are allocated from a counter and checked when declarations are populated.
Filenames and local symbol mappings belong to upper layers. Ordinary symbols are
interned by spelling; callers can allocate fresh symbols separately when source
identity must be preserved.

`TypeId` identifies a normalized structural expression in one database. Ordinary
types such as `Int` and `Sym` are declaration references, not builtin nodes.
Literal nodes describe exact values only. A literal is regular when written in
a type, or fresh when a literal term gave it, as TypeScript's widening literal
types are. Only a fresh literal decays to its class, so a signature's or a
schema's literals are never lost. A union keeps a regular twin over a fresh one;
otherwise the two relate alike, since the solver exposes a fresh literal as its
regular twin, and `same` treats them as one. Optional intrinsic slots associate
`Union`, `Keys`, `Values`, `Entries`, `Tuple`, `Int`, `Bool`, `Sym`, `Nil`,
`Str` and a few more with their stub types. Each slot
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
finding `Unknown`. A written type the database can't represent yet, such as a
rest pattern mapped over packs, is an `Unsupported` stand-in instead: each is
unique, since what it stands for can't be compared, and the solver reports a
judgment reaching one as unsupported rather than consistent.

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
`std.Value` as top and `std.Never` as bottom, so a union absorbs a written
`Never` as it does an empty union.
An application of the `Union` intrinsic interns as a union expanding its
schema, and an expanded schema of positional items contributes their types as
members, so `Union[...Ts]` becomes an ordinary union once `Ts` is substituted.
Other expansions remain symbolic until a consumer supplies their schema
arguments. The projections `Keys`, `Values` and `Entries` intern as unions the
same way, each a member projecting its schema. Once the schema is known,
`Values` folds every item's value, whatever its multiplicity, projecting an
included schema in turn. `Keys` and `Entries` fold the schema's keyed view, as a
collection indexes it (`Database::promoted`): each position is keyed by its
index, a literal `Int` while the positions before it are all required, and `Int`
from the first that may be missing or repeated on. `Keys` takes each key, and
`Entries` each `Tuple[key, value]`. An included schema not yet known stays a
member beside what is known, unless positions, or keys that may be indexes, lie
beside it: their indexes, or collisions, depend on it, so the whole projection
waits. A key that may be a position's index conflicts with it: a non-negative
`Int` literal a position may have, or a domain of `Int` alongside positions.
A conflicting projection stays unevaluated, and a judgment exposing it is
contradicted. The dynamic schema projects to `Unknown`. Without a designated
`Tuple`, `Entries` stays whole, and so does a projection of a varying position
without a designated `Int`. The item projections `IndexItem[S, K]` and
`AssignItem[S, K]` intern as union members of a schema and a key, and only the
dynamic schema reduces them there: selecting by a key takes the solver.
`Database::normalize` gives a type the canonical form interning
would, for callers that need it before interning. Declaration wrappers are not
normalized away.
Exposure follows transparent head references and reports direct cycles, stopping
at nominal declarations, quantifiers, applications, and other structural forms.
Recursive graphs are representable; this does not establish recursive typing
rules. Generic exposure returns the definition intact in its defining scope.

Structural walking and rebuilding report quantifier boundaries and visit
bounds/defaults along with all other children. Declaration references are
leaves; definitions and declared supertypes must be visited explicitly. A
nominal definition's supertypes are interpreted inside its outer structural
binder group. Each records whether the runtime inherits from it: a class's `@`
supertypes, and all of a protocol's, are only claims, even one naming a class.
Shifting inserts/removes groups at a cutoff and refuses to remove
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

Generic applications require every argument in its binder's slot, as population
places them: a keyword argument in its binder's, a variadic binder's arguments
as one schema, and omitted arguments as their defaults. Binders of either kind
and any binding but an implicit one are applied this way. Arguments left as
written after an expansion of unknown reach are residual. Subtype judgments
assume well-formed inputs: callers establish argument bounds and validate
declaration bodies and supertypes under their binder assumptions. Exposure
substitutes arguments without generating binder-bound obligations, including on
unselected inheritance paths; well-formedness checking establishes them.
Matching constructors decompose according to declared variance.
Inheritance walks left-to-right, depth-first, carrying substitutions through
each edge. The first matching declaration wins, even if its arguments contradict
the expected arguments or remain unresolved. An earlier incomplete branch cannot
be skipped to find a later match. This follows runtime member lookup's left-wins
ordering and avoids speculative inference or combining bounds from alternative
paths.

A function type is a subtype of another when its parameter list includes the
other's (see [Schemas](#schemas)), its result is a subtype of the other's, and
its ambient channels are supertypes of the other's. Channels are implicit
arguments, so both are contravariant; since `Sink` is contravariant in its
element type, a function that writes `Int`s can be given a `Sink[Num]`. An
omitted channel stands for its default bound, `Iter[Unknown]` or
`Sink[Unknown]`, or `Unknown` when `std` doesn't designate one. Union-left
judgments require every member; union-right judgments accept a member proved by
an isolated, closed subtype query. Alternative queries cannot add inference
bounds or diagnostic edges to the calling solver. Expanded union packs and
alternatives that cannot be proved remain residual. The exception is a literal
on the left, or `Int`, `Str` or `Sym`, which have infinitely many: when every
member refutes it, apart from a class's literals, which are finitely many, it
contradicts the union. A projection is exposed by reifying it once its schema's
environment is substituted, which waits on the schema's variables. One of a
rigid's schema that is left on the left reduces to the same projection of the
rigid's bound. An item projection is evaluated once its schema and key are
closed, by exposing or reifying it (`solver/item.rs`). The key selects items of
the schema's keyed view: each member of the key goes to the literal item it is,
or else to the literal items inside it and the domains that own it, as an actual
keyed item goes to an expected schema's domains. `IndexItem` joins the selected
values. `AssignItem` meets them without intersection types: the lower of two
ordered values, the bottom type for two literals or classes that can't share a
value, and otherwise `Unknown`, which leaves the write unchecked. A key member
the schema doesn't admit whole contradicts the projection, as a conflicting
schema does. A rigid key selects only by its bound, so exactly only where each
item the bound selects has the same value; otherwise the projection is residual.
One left unevaluated is below an item projection of the same kind and schema on
the right whose key is proven wider for `IndexItem`, or narrower for
`AssignItem`. A function's result that is an item projection is exposed where
the function is related, so a call reports a key its schema doesn't admit even
when nothing uses the result. Quantified types on the right are related through
skolems (see [Skolems and scopes](#skolems-and-scopes)). Contextual identity and
top/bottom rules can still settle some judgments involving otherwise unsupported
forms: anything is below top and `Unknown`, even a type that can't be exposed.

### Schemas

`typeck/solver/schema.rs` relates schemas. A schema admits item sequences whose
positional and keyed items are independent. Positional items are distributed by
count, as the runtime binds positional arguments: each required item takes one,
optional items take what is left over from left to right, and a repeated item
takes the rest. Keyed items are unordered across keys. Within a key, a literal
key takes as many of its items as its multiplicity admits, in order, as a named
parameter does, and the rest go to a key domain `(K): V`.

A parameter list binds by count. The schema of a type argument, as `Unpack`'s
or `Tuple`'s, is the items a value holds, which may fill its multiplicities any
way that fits: `(1, true)` is below `Unpack[{Int, ?Str, *Bool}]` but isn't
accepted by `(Int, ?Str, *Bool) -> nil`. A relation records which reading
applies. A type argument starts the second; a parameter list and a variable's
bounds start the first, which is the stricter. Anything else inherits it.

Both sides flatten into lanes of positional and keyed atoms. A required
inclusion splices its items; an optional or repeated one gives its single item
that multiplicity, and correlating several items' counts is residual. A schema
that can't be exposed stays opaque, occupying the lanes its bound allows. The
same rigid on both sides pairs up and splits the positional lanes into segments.
Only the last segment's expected items may vary in count, since counts are
distributed over the whole lane. An actual rigid without a counterpart stands
for its bound, and an expected one contradicts the judgment. `Unknown` leaves
the lanes it occupies unchecked, except that the actual side's items with
literal keys must still fit the expected side's items with those keys.

Positional inclusion tries every way of filling the actual side's
multiplicities, up to one overflow past the expected items. Each actual atom
must be a subtype of every expected atom its items can land on. Where the
expected side is the items a value holds and a filling's count distribution is
refuted, the filling still fits when the item types, compared without adding
bounds, prove some path through the expected multiplicities. Otherwise too few
or too many items contradict the judgment, naming the expected item that can go
missing or the actual item that can be excess. So does a literal key whose count
can fall outside its item's multiplicity, where a domain on the actual side may
hold the key any number of times, unless a repeated domain beside the literal
admits the key. An actual item that may come before the literal is full must fit
it, and one that may come after must fit that domain. Keys not named on the
expected side go to its single repeated domain. Several repeated domains own
keys as a literal key does: each member of an actual item's key goes to the
narrowest domain admitting it, and to any domain lying inside it, and its value
must fit each. So a lookup bound `{*(K): V, ...}` isn't vacuous, though its
`...` admits every key. A domain key still to be inferred waits for its
solution, and one whose relation to an item's key can't be decided is residual.
When several expected repeated items could take the overflow, the judgment is
residual.

Subtyping never admits a positional item as an `Int`-keyed item; only the
projections key positions by their indexes. Positional items against an expected
schema with none but a domain that might admit `Int` are residual rather than
contradictions.

An expected schema whose items are all repeated, with at most one positional
item `*P` and one keyed item `*(K): V`, as in `{*T}`, `{**V}` and `{...}`,
admits each actual item independently. Each positional item's type must be a
subtype of `P`, and each keyed item's key of `K` and value of `V`. An included
schema must itself be included in the whole shape, through the ordinary rules
for rigids and `Unknown`. This decides schema binder bounds, symbol keys in
parameter lists (`<: {*Value, **Value}`), and packs expanded into a
positional-only rest (`<: {*Value}`) without flattening.

A schema variable is opaque like a rigid. The same variable on both sides pairs
up. So does a variable with a rigid or skolem across from it, when both sides
have their opaques in the same places, as a quantified signature related to its
own instantiation does: the variable is bounded by the rigid. An expected
variable without a counterpart takes what the actual side has left: it must end
its positional lane, after required items only, and it takes the keyed items the
expected side doesn't name, or is residual beside a key domain. What it takes
becomes a schema built around the atoms' solver terms, which is its lower bound.
An actual variable without a counterpart is bounded only by a rest-shaped
expected schema; otherwise the judgment is residual.

A call is checked as an ordinary judgment: the callee's type must be a subtype
of the function type the call expects, `(args) <input >output -> result`.
`Solver::call` builds that type around solver terms, since canonical types
can't hold them. Its parameter list has a required item for each positional or
keyword argument, whose key is the literal name, and includes each spread
value's schema. Contradictions and derivations under the parameter list name the
argument by its index, so the caller can point at it. Omitting an optional
argument adds no item, while passing `nil` is checked like any other value.

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
judgment. Forwarding an omitted ambient channel to a callee is identity and
never consults its default bound; using its elements reduces through the bound,
so a strict mode can reject proofs carrying that label and ask for the channel
to be annotated. A union on the right is proved by a member identical to the
left side before alternatives are probed. A positional pack's items are each
below a union that expands the same pack, so `{...Ts}` is below
`{*Union[...Ts]}` without the pack's bound. Rigids are closed, so they reify to
themselves and assignments may contain them. A rigid of a declaration not
assumed has escaped its own check: it is related only to itself and top, and
reifying it is residual.

`reach` walks a term to a target declaration through the substitution-carrying
inheritance walk, continuing through an assumed rigid's bound, and returns the
target's arguments. It reports a term that doesn't reach the target, and
`Unknown` as reaching anything. `inherits` walks only the supertypes the runtime
inherits from.

### Instantiation

A quantified function type on the left of a function type is instantiated: each
binder gets a fresh variable of its kind, and a schema variable records the
lanes its rest mode allows. Each variable must be below its binder's bound,
interpreted in the instantiation's environment, and the body below the expected
function. Implicit ambient binders and lifted binders are instantiated the same
way; a call's channels bound a callee's channel variables from below, and
defaulting settles them on the caller's channels. The environment is recorded by
obligation, so reprocessing derives the same obligations without creating
variables. Variables never leave the solver: flow analysis creates a solver per
step and exports only reified types. Each variable belongs to the innermost
scope among the variables and skolems of the obligation that created it, so a
quantified type instantiated against a skolemized body may take its skolems.

### Skolems and scopes

A quantified type on the right of a structural type is skolemized: each binder
becomes a skolem, `Term::Skolem`, and the actual side is related to the body
under an environment of them. The body must hold for every choice of the
binders, so it must hold for these. A skolem is solver-local and never enters a
canonical type; reifying one is an escape. Its bound is the binder's bound read
in the skolemization's environment, which carries F-bounds and outer
substitutions, and a rest binder without one is bounded by its shape. Skolems
follow the rules of rigids: a skolem is below itself, top and `Unknown`, and
bottom and `Unknown` are below it; on the left it reduces to its bound, labeled
as a rigid's is, and otherwise it contradicts the judgment. One without a bound
is below a union only through a member that is itself, top or `Unknown`. The
skolems are created once per obligation, as instantiations are. Contextually
identical quantified types are proved before either rule applies.

Each skolemization opens a scope inside the obligation's innermost one. A
variable sees the skolems of its scope and the scopes around it, and a bound
holding any other has escaped. A skolem that is a whole lower bound is promoted:
the variable is bounded below by the skolem's bound instead, or by top if it has
none, which is the least type above it without it. Any other escaping bound is
not recorded, and its judgment is residual; if the variable is solved
otherwise, its solution is related to the skolem directly, which can contradict
it.

A variable's solution may hold skolems it sees. Such a solution is chosen only
by identity: every lower bound must be the same term, holding no unsolved
variable. A bound the same as it forces it; otherwise it is a default. Skolems
are never joined or probed. A scope's variables are invisible outside its
judgment, so the solver settles them itself: at quiescence, it defaults one at a
time, innermost scope first and in creation order, and solves again. A variable
nothing is below takes bottom. Variables of the root scope are left to the
caller, unless the solver is closed: a judgment between declarations' types,
whose variables no caller sees, settles them last in the same way.

The supported higher-rank fragment is a prenex quantifier on either side of any
obligation, including quantifiers reached through function parameters and
results, which the rules reach recursively: a quantified parameter becomes a
quantifier on the right by contravariance. Residual forms are:

- a quantified type on the right of a variable, which would need impredicative
  instantiation; the bound is not recorded;
- a quantified type on the right of a class instance;
- a projection whose schema or key holds a skolem, since projections are
  evaluated by reifying them. It stays unevaluated and relates only to an
  identical projection, a member of a union on the left proved by the same
  member on the right.

### Member lookup

`typeck/solver/member.rs` finds a receiver's member as the runtime does. It is a
query on the solver, not a judgment: it adds no bounds, but the receiver is a
term, possibly holding inference variables, and its class is reached through the
same substitution-carrying walk as subtyping. A rigid in scope is looked up
through its bound, and a literal or function through its intrinsic class. An
unsolved variable is residual, a union is unsupported until alternatives are
judged, `Unknown` is dynamic, and `Value` has no members.

The class and its ancestors are searched in MRO order, left to right and depth
first, and the first member of the name wins. A supertype that isn't nominal
makes a member not yet found dynamic. Instance members and type-object members
are separate namespaces. An instance with no member of an ordinary name falls
back to its class's `(get)` and `(set)` methods. A class object has type
`Type[C]`: its members are `C`'s class members, its static members only on `C`
itself, and then `C`'s instance methods, unbound. A private member is its
class's own, so private access names that class, and the receiver is walked to
it.

A found member is interpreted with the arguments its class is reached with. A
method is lifted over its class's binders, so `Database::split` separates them
from its own and the class's arguments are applied, leaving it quantified over
the rest: `map[U] self f @ (T -> U) -> U` found through `Box[Int]` is
`[U] (Box[Int], (Int -> U)) -> U`. Its receiver parameter stays, so a call
passes the receiver as its first argument, and each signature of an overloaded
method, its overloads and its implementation, is applied alike. A property's
getter and setter are methods. A result says whether the member is public, since
only a public member can be replaced in a subclass, so only access to one may
dispatch.

A search can follow only the supertypes the runtime inherits from. A class's `@`
supertypes and all of a protocol's are claims, so what a class reaches only
through one is not an implementation of its own.

### Conformance

`typeck/solver/conform.rs` states what a supertype's members require of a class
or protocol. Its answer is a set of ordinary subtype judgments for the caller
to constrain, so conformance can become a subtyping rule for structural
protocols (#828) without caching a verdict. For each public, non-static member
of the supertype's MRO, `(init)` aside, the member the supertype has is
required, and the declaration's own is provided: the first in its runtime MRO
for a class, or in its whole MRO for a protocol. Members compare by kind:

- fields invariantly, both ways, as they are mutable;
- methods by their implementations, the provided below the required. The
  required side's overloads are not compared. On the provided side, an overload
  below the required implementation satisfies it in place of the implementation
  (found by a probe). An overload is an unchecked assertion narrowing its
  implementation, so it can state what the solver can't prove, such as
  `Tuple.(index)`'s precise result for `Index[{...Ts}]`;
- properties accessor by accessor, and a property may not drop an accessor the
  required one has;
- a protocol's field by a property with both accessors, the getter's result
  below the field's type and the setter accepting it.

Any other change of kind is reported, as the runtime refuses a class that
replaces a field with a method or property. A protocol's member that nothing
provides is missing; a claimed class's member is covered by `claimed_classes`,
which reports each class a claim names, down its ancestry, that the declaration
doesn't inherit at runtime, or inherits with arguments the claim doesn't allow.

A required method is called on the declaration's instances, so its receiver is
narrowed to them. A receiver `C[a…]` becomes the declaration's class applied to
the arguments that make it reach `C[a…]` along the supertype the member was
found through: a default receiver becomes the declaration's own type, and
`chomp[U] self @ Iterable[U]` checked for `Iter[T]` takes `self @ Iter[U]`. The
provided method keeps its receiver, so one callable on fewer instances fails.
A receiver that isn't a class application, or can't be matched, stays as
written.

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
an arbitrary satisfiable interval does not select a solution. Consistency is not
antisymmetric, so a bound containing `Unknown` must still admit `C` but is never
part of `C` and never forces it; this also keeps a variable from being assigned
`Unknown`, which would chain consistency transitively. Bounds containing
unsolved variables remain obligations and are revisited after commitments.
Unsupported concrete compatibility checks defer commitment. There are no
intersection nodes, speculative assignments, rollback, or defaults to top or
bottom. Closed proof queries reuse the subtype engine and charge their work to
the caller's lifetime budget.

Forcing alone rarely settles a call: its result variable and most of its
callee's variables have only lower bounds and binder bounds. `default` is a
separate, caller-driven choice: it assigns the join of a variable's lower
bounds, all of which must be solved, or `Unknown` if one of them is. The default
must satisfy every solved upper bound; the obligations pairing lower and upper
bounds check the rest once it commits. A variable without lower bounds is never
defaulted. The caller defaults a variable's lower bounds before it, such as a
call's binders before its result, and solves between defaults so that their
consequences can force later variables. Defaulted assignments are marked as
such, so a contradiction reached through one can be reported as an inference
choice. A default can decay the join's fresh literals to their classes, except
in exact schema keys, the keys of item projections, and binder bounds, unless
the decayed join violates a bound. Nor does a variable instantiating a binder
that an item projection in its signature selects by decay, since a decayed key
selects differently (`Database::item_keys`). A forced assignment keeps its
literals, since its bounds require them. A rule decays only where a literal
would lock in: `locked` finds the variables that its outputs (its results, and
as inputs the parameters of the `do` blocks it passes values) reach at a
position that isn't covariant, through variables' bounds, where a later value
couldn't widen them; the rest keep their precise join, which subsumption widens
as needed. To help a caller choose, `raised` finds the variables that raising
given terms could raise: those at a covariant or invariant position in them, or
in a raised variable's upper bounds. A function's parameters and channels don't
count, and a form it can't see into counts in full.

Joins, for defaults and for flow state, drop union members proven below another
member. A member containing `Unknown` neither subsumes nor is subsumed, since
consistency isn't antisymmetric, and `Unknown` itself absorbs the join. A join
alone doesn't converge on a type that keeps growing, so a widening point counts
a variable's increases and widens in two stages: first to the least ancestor the
union's members share, found through the first member's MRO with arguments
combined by variance, then to `Unknown`. Sharing only `Value` widens to
`Unknown`, since a static top would make every later use a contradiction.

Narrowing a flow type by an `Assume` works member by member against a class `C`
or a literal, and stays above each member's true intersection with the target. A
member that `type x C` can't prove disjoint or already reaching `C` becomes `C`,
with the member's arguments where they carry down by variance and `Unknown`
otherwise. A reach that can't be proven keeps a member under a negative relation
and makes it `C` under a positive one. A class's `(==)` may be user-defined, so
a literal comparison strips only other literals, except that `Nil` and `Bool`
members lose the literal they're unequal to. An empty result is bottom, making
the edge unreachable.

Exact candidate dependencies receive a scope-aware occurs check. Recursive
substitutions remain recursive residuals; variable-only cycles remain unsolved
unless concrete bounds force them. Assignments hold no unsolved variables, so
they cannot introduce assignment cycles. Declaration wrappers remain
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
explain contradictions to every contributing root, preferring a path without an
assignment edge, which explains only by what the assignment was drawn from.
Historical cycles and
obsolete residuals do not prevent a current proof. Cycles in current proof
premises remain unresolved. Reports distinguish proven, contradicted, and
unresolved roots; quiescence alone is not proof.

`solution` exposes a committed canonical type, or nothing for a solution
holding skolems, `solution_sources` exposes its
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
well-formedness checking diagnoses what causes them. Omitted arguments with
binder defaults remain residual.

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

## Typing CFG

`typeck::cfg` is the graph that type flow analyzes. It is separate from the
bytecode CFG: it keeps semantic structure and ignores the runtime's
implementation of control flow. The design, including the flow analysis over
the graph, is recorded in issue #734.

A module is one analysis region. Its top-level code is the entry function, and
every def, method implementation, lambda and field initializer is a function
nested in it, identified by its declaration. Each function's locals are hoisted
to the function. A function's variables of its enclosing functions are its
captures. A
`do` block's unannotated parameters, omitted channels and omitted return type
are its signature: variables its parent owns and it captures, starting as
bottom. The call it's passed to joins its expectations into them, and the
block's exit joins its result in, so each side sees the other's changes as it
would a capture's. Every function has an exit block, the only one that returns,
and a result variable. A return assigns the result and continues to the exit,
through any `finally`; a variable survives the empty stack that a `finally` is
entered with.

A block owns its steps and ends in a terminal. A step is a statement whose
expressions stay trees. Expressions mirror the AST. Checking rules are calls,
method invocations (lookup and call in one rule), member accesses, subscripts,
operators and collection literals. `&&` and `||` are
control flow, so a narrowing test's successors begin with `Assume` steps. A
pattern binds without its defaults; each default is a `Default` step after the
binding step, or on the success edge of the terminal that binds it, which joins
it into the variable. An interpolation is a `FmtValue`, which a string formats
in place, and a `t"..."` sequence is a `Fmt` of text, `FmtValue`s and
`FmtParam`s; std's classes of those names are designated for their types.

Only short circuits cross blocks mid-expression. A short circuit's left operand
goes on an operand stack, and its result is there at the join; the rest of the
expression stays a tree with an `Operand` hole in its place. This is a
post-order linearization frozen partway: the stack holds completed subtrees and
the remaining tree pops them in order. A step or terminal pops one entry per
hole, in evaluation order; below those lie only the pending results of the
statement's earlier short circuits. `If` pops its condition, so a short circuit
`Dup`s its left operand, and its long path `Pop`s it before pushing the right
operand. A rule evaluated before a short circuit is thus judged after it; flow
state can't observe the difference, since expressions neither assign nor bind,
narrowing in the right operand is rejoined at the join, and calls change only
volatile variables, which are never narrowed.

Comprehensions also cross blocks mid-expression. A `for` item's iteratee and an
`if` item's condition are lowered to blocks before the collection or call that
holds them. In the items' values, only what depends on where it's evaluated is
assigned to synthetic variables in those blocks: variable reads, which see the
body's narrowing, and short circuits, whose results can't cross to the
collection's operand stack. Everything else stays in the tree, where the rule's
expected type reaches it. A read happening before the calls around it makes no
difference, since calls change only volatile variables. Items can't assign, so
no state crosses iterations: a `for` item is a `Next` whose body continues to
its exit, with no back edge. The
collection keeps a tree of `For` and `If` items, which says only how often each
value occurs, and flow builds the rule's schema from it. The variables, and the
comprehension's own bindings, start as bottom
rather than unassigned, so the loop's exit edge and an `if`'s other branch add
nothing to them. Statements are never
nested in expressions, so an exceptional edge discards the whole stack. A
handler is an ordinary block entered with the exception alone on the stack;
`Catch` dispatches it to clauses by class. A class that is a name or a dotted
path stays in the `Catch` that tries it; any other is evaluated in blocks of its
own, above the exception, and begins the next `Catch`, which the previous one's
`otherwise` reaches.

`Leave` enters a `finally` with a tag saying how to continue after
`EndFinally`: at a block, or by rethrowing. Lowering routes each exit once,
through trampoline blocks where `finally`s nest. A block records how many
`finally` bodies of its function enclose it. During flow the tags form a stack
of that depth, which keys the block's state, so a `finally` is analyzed once
per continuation rather than joining them.

A `break`, `continue` or `return` in a `do` block happens during the call that
the closure is an argument of. In the closure, `Escape` ends a `break` or
`continue`'s path: whatever it changed in enclosing functions' variables reaches
them through captures. `ReturnFrom` merges only the returned value, as the def's
result, into the def's exit block. The rest comes from a `Guard` in the
enclosing function, placed before the call. Besides continuing normally, it has
phantom edges, which discard the stack, to each target the closures in the
statement may jump to. A return's phantom target assigns `Never` to the result,
then continues to the exit, through `finally` blocks as any return would.

`Ir::validate` checks structure: edges and handlers stay in their function,
depths only grow by `Leave`, returns come from exit blocks, non-local terminals
leave for enclosing functions, variables are owned or captured, closures are
instantiated by their parent, and rules are unique. Stack depths are checked by
flow analysis.

`typeck::lower` builds a unit's graph from its elaborated syntax tree, following
the bytecode lowerer's shape: a focused block is extended and switched as
control flow requires, and function bodies and statement blocks are queued, each
with its context. Lexical frames mirror the resolver's scopes, so a variable's
`(index, depth)` resolution decodes directly, and each allocates its variables
in the enclosing function. A `try`'s parts and an `NlGuard`, closures only at
runtime, are lowered inline, so a jump out of one is local. Only lambdas, defs,
methods and field initializers are functions, and captures are found by
comparing a variable's owner with the function reading it. Declarations are
found by the address of their node. Imports and prelude names resolve to
`Import` expressions, including a dotted path through a module. A statement's
value, needed by `let x = if …` or as a function's implicit result, goes in a
variable rather than on the stack, so every statement starts and ends with an
empty one. Where a statement would need a value twice, the value is bound to a
synthetic variable first, so that no step needs an entry buried below another
step's.

## Type flow

`typeck/flow.rs` runs over each unit's graph once it is lowered, iterating to a
fixed point with one work queue for the region, ordered by reverse postorder so
the result is deterministic. A block's state holds a fact for each variable its
function owns (the join of the types it may hold, and whether it may be
unassigned) and the operand stack's types. It is stored per context, the stack
of `finally` tags the block was entered with, so a `finally`'s normal and
rethrow entries are never joined. States only grow: they join where control
merges, and at the target of an edge that retreats in the queue's order the
join widens (`solver::Widening`) once it has grown too often, or goes to the
local's annotation.

An assignment is a strong update, and a fresh literal assigned to a declared
local decays to its class when that fits the annotation. A parameter's or
pattern item's default is evaluated expecting the variable's annotation, and its
fresh literals decay. Constants and a dict literal's keys are fresh; exact keys,
such as a keyword's or one passed as a pair, are regular. It keeps the
annotation when it fits. A `nil` or symbol literal that doesn't is a sentinel,
joined into the variable's type for the body to narrow away, while callers see
only the annotation. Any other default is reported. An `Assume` narrows with
`Solver::narrow`, and an edge left with nothing is unreachable. Flow state marks
a stack entry that a `Dup` copied from the one below it, and any other step
clears the mark. An `If` on a marked copy narrows the original on its `then`
edge to its truthy values, dropping `nil` and `false`, so a short circuit's
result is narrowed by the test it passed. A step that can throw joins its prior
state into its handler; a `Catch` narrows the exception by each clause's class.
Parameters are bound at the entry block: a def's from its signature under its
group's rigids (`Tables::group_rigids`, which also closes `Var.annotation`
during lowering), a `do` block's from its signature variables.

State shared between functions is flow-insensitive. An ivar, a variable that a
function other than its owner reads or writes, has an accumulator: every
assignment to it, in any function, joins into one type, which widens to its
annotation if it has one. A non-local return joins its value into its def's
result the same way. Its owner caches its type in its flow state, narrowing it
there, unless it's volatile: assigned by another function, and so changed by any
call that may run that function. A volatile variable's owner reads the
accumulator too, and keeps only whether it may be unassigned. Only a cached type
is narrowed; to narrow an accumulator, a program copies the variable to a local
first. Reading an accumulator makes the block depend on it, and it is queued
again when the type grows.

A `do` block's signature variables are joined the same way. Its exit joins its
result into its result variable. A call it's an argument of types it: it enters
the call's solve as its declared function type (`Tables::group_rigids` closes
lifted binders), with a fresh variable for each parameter and channel it leaves
open. Parameters are contravariant, so those variables' lower bounds are what
the callee passes. Once the rule is concluded, the variables are solved by
defaulting, and their solutions join into the signature variables. The block's
result enters as its joined type, or, while that is still bottom, as a fresh
variable that keeps the call undecided. The call's block reads the result
variable, so it runs again when the block's analysis grows the result. A block
anywhere else gets `Unknown` joined into its parameter and channel variables,
and its value is its declared type with the joined result. A block's omitted
channels aren't quantified as a def's are: calls in the block pass its channel
variables' joined types, and depend on them.

Checking rules (`flow/rule.rs`) are solved by a fresh solver on each run, and
only reified types leave it:

- A call constrains its callee below `Solver::call_items` of its arguments,
  passing the caller's declared channels. A callee that isn't a function type
  or a union of them gives `Unknown`. What its arguments
  are expected to be comes from the callee's parameters. A parameter that
  mentions the callee's binders gives an expectation only once the call is
  solved, so a collection literal or call passed to it is held back: a
  pre-solve stands a fresh variable for it and solves the call without it. A
  variable the held arguments can't raise takes its least solution
  (`Solver::raised`), since whatever supplies a function takes its parameters
  and channels from what's expected of it. Then each held argument is evaluated
  expecting its parameter, if what's forced or chosen solves it. The pre-solve
  never makes a variable dynamic, so a binder only a held argument determines
  gives it no expectation.
- A call through an overloaded function, a method or a def, chooses among its
  `@def` overloads, a stopgap until union calls are solved (#742). Its
  arguments are evaluated once, each one that takes an expectation held back,
  and each overload is pre-solved without the expected result, then defaulted
  as the call's own solve would be, so a key still to be solved can't hide
  one the overload can't select by. A `do` block's result is a fresh variable
  there even once it's known, so a block never rejects an overload. The one
  overload not contradicted, if exactly one is, is the callee; otherwise the
  implementation is, or `Unknown` without one. Only the implementation's own
  check reports a call no overload takes (#821). An overloaded def's value is
  `Type::Decl` of it, which the solver relates as its implementation's type.
- A comprehension's items are passed as often as its tree says. The items of an
  outermost `for`, with everything nested in it, join into one repeated item of
  each kind: `*T` for positional items, `*k: V` for each literal key, and
  `*(K): V` for the rest. An `if` outside every `for` passes its branches'
  items once if the branches are alike (the same kinds and keys, each required),
  and otherwise makes them optional; the solver's limit on alignments bounds
  what that costs. A spread that's repeated or optional gives its solved
  schema's items, so that the solver never has to repeat or leave out an
  inclusion of several items.
- An array, dict, tuple or record literal builds its designated class over
  inference variables for its items, a spread through `Spread[S]`. An array
  joins every item into its element type, however often it occurs, and expects
  each item to be the element of an expected `Array[E]`. A dict joins its items
  into `Dict[{*(K): V}]`, so that a local it's assigned to can gain entries,
  unless it's expected to be a `Dict[S]` or a `BaseDict[S]` (alone or as one
  member of a union): then its items' own schema, built as a call's arguments
  are, must be below `S`, and it's `Dict[S]`. `std.BaseDict` is `Dict`'s
  covariant, read-only half, a separate class only to the checker: the runtime
  exports `Dict` under both names. Tuples and records have no vertical form, so
  they hold no comprehension.
- A `do` block among a call's arguments, or a collection's items when the
  collection has an expected type, is typed by the rule. A rule with a check it
  couldn't resolve counts as undecided, so defaulting rounds reach it. Once no
  undecided rule is left, a block's parameters and channels that nothing gave
  anything become `Unknown`, which may start more rounds.
- A `for` item is `T` of `iteratee <: BaseIterable[T]`. An unpacking pattern
  requires `value <: Unpack[...]` and is walked (`solver/unpack.rs`) against
  the `S` each member of the value's solved type reaches, as the runtime binds
  it: positional items by count, keyed items by key. An item takes the join of
  what it can bind. A rest is the `Rest` of the supertype edge reaching
  `Unpack`, with that edge's class binders solved again by matching its `S`
  against the tail, if that is within `Rest`'s bound, or else `Rest`'s default
  for the tail, which only spreads. A pattern no filling of `S` matches
  is diagnosed as impossible, and its bound edge is unreachable. A member whose
  `S` can't be found, as for a structural conformance, leaves the whole pattern
  to the earlier rule: every item optional, anything else admitted, and rests
  and constant keys `Unknown`.
- A binary string's parts must be `Bin`, and an interpolation's width and
  precision `Int`.
- A member use (`flow/member.rs`) looks its member up (see "Member lookup") and
  is checked as the runtime makes it, as a call through the member where there
  is one. A method call passes the receiver first to an instance's method, and
  calls a field's value or a getter's result as it is. A read gives a field's
  type, a getter's or `(get)`'s result, or a method bound to its receiver, which
  is its signature without the receiver parameter unless that mentions the
  method's own binders. A write must fit a field's type, or calls the setter or
  `(set)`. Indexing calls `(index)`, and an index target `(assign)`. An operator
  calls its special method on its left operand, or, as the runtime does, when
  that lacks it, on its right: the same method with the operands swapped for a
  commutative operator, and the reflected one (`(rsub)`, `(rdiv)`, `(rediv)`,
  `(rmod)`) otherwise. `==`, `!=` and `!` are `Bool`, and the comparisons
  require `(lt)` and are `Bool`. A missing member, and a
  read or write its kind doesn't allow, are reported. A lookup that can't
  decide, such as on a union receiver, is an unresolved check. An overloaded
  method is dynamic except where it's called.
- A class object is called as its class-level `(call)`, if it has one, and
  otherwise as its constructor: `(init)`, looked up on the class applied to its
  rigids, without its receiver and giving the instance, with the rigids
  abstracted again and the class's binders merged into `(init)`'s own
  (`Database::merge_groups`, the inverse of `split`). A class without `(init)`
  takes no arguments. A range constructs `Range` from its bounds.

A type fixed before the fixed point is pre-seeded as an upper bound on a rule's
result: a local's annotation, a def's declared result, or a parameter type that
doesn't mention the callee's binders. A rule contributes only once decided:
without contradiction, with its results solved without defaulting. Until then it
contributes bottom, and a rule with a bottom input doesn't run. A rule's results
are its latest run's, which needn't be monotone: the analysis converges because
states and accumulators widen. Each (block, context) records whether its
latest run left a rule undecided. When the queue empties, each function's
earliest such block is marked and requeued, and its next run, once, defaults the
variables of each rule it leaves undecided, a variable without lower bounds
becoming `Unknown`. A marked block that reruns later, because its inputs grew,
waits for another stuck point, so it never defaults on inputs still settling.
Rounds repeat until no block is undecided.

Lowering copies a variable's value into a statement's destination, such as a
function's result, as `ExprKind::Copy`, which flow reads without recording it as
a reference.

Once the rounds end, a final pass reruns every block over its final state,
defaulting any rule it leaves undecided, as the last run of a marked block did.
It records what each variable reference and binding saw, which the `flow`
judgment reports, and reports each problem once per span: contradicted rules,
values that don't fit a local's annotation or a function's declared result, and
reads that may be unassigned. Checks the solver can't decide join
`Check::undecided`.

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
expanding as any number of it. A declaration whose only positional binder is
a schema, with only keyword binders besides, takes `Foo[T]` and `Foo[K, V]`
for its items. Applying a schema, a binder or a
declaration without binders is an error, as is naming a value or module as a
type.

Signature completion fills each def and method signature with the defaults for
what it omits, the same for public and private definitions. An omitted
parameter, rest or return annotation is `Unknown`, a rest's as each of its
items. An omitted ambient channel is an implicit binder following the
signature's written binders. It is gradual, bounded by `Iter[Unknown]` or
`Sink[Unknown]` when `std` designates them, and unbounded otherwise. A method's
unannotated receiver is its class applied to its own binders, except on a
`class` or `static` method. A function type written without channels in a def's
signature or body, but not in a nested class or alias, shares that def's
channels; elsewhere they are `Unknown`. A closure is populated with its
annotations and `Unknown` for what it omits, channels included; CFG flow infers
the omissions separately, without changing the database. Top-level declarations
of a checked `std` module named `Value`, `Phantom`, `Union`, `Keys`, `Values`,
`Entries`, `Func`, `Int`, `Bool`, `Sym`, `Nil`, `Str`, `Iter` and `Sink` are
designated for special treatment; the same name in another module is only a
lookalike. So are the classes that literal and constructor expressions produce,
`Float`, `Bin`, `Array`, `Dict`, `Tuple`, `Record`, `Range` and the `Fmt`
classes, which the check tables record without the database needing them, except
`Tuple`, which `Entries` builds. A checked `strand`
module's opaque `PipeSender` and `PipeReceiver` are designated too: each
stands for the class the `Builder` nominates, resolved as if the placeholder
imported it, and is populated as a transparent alias of that class applied to
its binders, whose variance it takes. A nominee in no checked module leaves the
placeholder `Unknown`; one that isn't a class, or can't take the placeholder's
type arguments positionally, is diagnosed on the placeholder. The `kind`,
`sig`, `ambient` and `designated` judgments report these results.

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
Rust's `PhantomData` does, so `Phantom[(T -> nil)]` marks a class contravariant.
A transparent alias uses its body covariantly; `Union`, the projections and
`Phantom` take their binders covariantly, and any other opaque alias uses none.
A bound of a binder's own group is a covariant position for it, since widening
the binder widens the bound, which the other arguments then still meet. An outer
binder used in the bound of a nested group is used contravariantly there.
Defaults and bodies do not count. A type argument is used as the binder it fills
varies, matched as kind checking matches it, and one whose binder is unknown is
invariant. A type declared within a generic declaration takes the outer binders
it is lifted over as implicit arguments. These equations are solved by a
worklist for their least solution, which is unique whatever the order. A binder
with no use, including one used only through itself, is then invariant, as is
any use through it, and a second round propagates that. The `variance` judgment
reports a binder's variance, and `captured` a nested declaration's outer
binders.

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
solver assumes. It is sealed but not validated until well-formedness is
checked. `Check`'s hidden `smoke` method relates every type the database
holds to itself and to top, to show that the solver judges it without
panicking. The `quantifier`, `decl`, `member` and `type` judgments report what
was interned.

Sealing closes the set of declarations, but a declaration can still be retyped
through `&mut Database`, which validates it as population does. Right after
sealing, each instance method with an annotated receiver `self @ U` is
specialized. A solver assuming the method runs `reach` from `U` to the method's
class, walking a rigid through its bound. The class arguments it reaches with,
reified over the method's rigids, replace the class's binders throughout the
method's type, which is abstracted back to its group:
`def int self @ Box[Int] -> T` in `class Box[T]` becomes `(Box[Int]) -> Int`.
`self` keeps `U` verbatim. The method stays lifted over every class binder; a
replaced one is unused, which affects neither variance, computed long before,
nor member lookup, which stays positional. Walks read only class supertypes and
the method's own bounds, so the order of methods doesn't matter. A receiver that
doesn't reach its class, or whose walk is undecided, is diagnosed and keeps the
unspecialized type.

Well-formedness is checked last. Each check holds the binders of the declaration
it is written in as rigids, assuming only their bounds, and every application of
a declaration is checked against that declaration's bounds. So validation is
local, rely-guarantee: if every check passes, every assumption is backed by one,
whatever order checks run in, and an invalid declaration elsewhere can only add
diagnostics, never make the whole validate. No verdict is cached or fed into
another check. A check the solver can't decide doesn't pass. `Unknown` passes
vacuously, but its site was already diagnosed.

Written types are checked where they are written, found by span in the
applications and function types population records, so diagnostics point at the
offending argument. Each argument of an application, including a keyword
argument, a variadic binder's items, a default filling an omitted argument and a
leading lifted binder, must satisfy its binder's bound with the application's
arguments substituted. An unbounded rest binder is bounded by its mode's shape,
so a pack expanded into `Tuple[*Ts]` has no keyed items. Arguments left as
written after an expansion of unknown reach are undecided. A function type's
parameters must have symbol keys, and its written channels must reach `Iter` and
`Sink` when `std` designates them. `Phantom` only marks variance, so its
arguments satisfy no shape, and function types within them may have any keys.
Class supertypes, which have no type expression of their own, are checked the
same way. Each declaration's binder defaults must satisfy their bounds, and a
def or method signature's parameters and written channels are checked as
declared.

A bare `**` or `...` in a schema admits any keyed item, so `Dict[Str, Int]`
satisfies `S @ {...}`; in a parameter list it admits only named ones. A written
`**T` item, and every rest binder's shape, has symbol keys.

Recursion among transparent aliases must be contractive and regular. Every cycle
of aliases must pass through a reference guarded by a class's arguments, a
function type or a schema's items: a union member, an argument of a transparent
alias and a schema inclusion don't guard, since each is flattened into its
surroundings. So `Item = Leaf | Node` with `Node = Dict[{*Item}]` is accepted,
though `Item`'s reference to `Node` is bare. Each reference within a cycle must
pass the referring alias's binders unchanged, so `E[T] = nil | Box[E[Array[T]]]`
is rejected, as OCaml rejects irregular abbreviations. Recursion through class
supertypes is left to the solver, which reports expanding inheritance as
residual.

Overrides and protocol conformance are checked after well-formedness, in
`elab/overrides.rs`. Each class and protocol is checked against each supertype
it names, under its own rigids, in one closed solver whose variables it settles
itself. Every requirement the solver states is constrained there, and a
contradicted one is reported: an override where it is declared, and an
inherited member, a missing one or a class a claim needs at the supertype
reference. The checks are local, as well-formedness's are.

`Check::validated` holds when the checker reported no errors and decided every
check, except those that need a form it doesn't support yet, which are
provisionally accepted. Otherwise the result is partial: usable for diagnostics
and tooling, but checking code against it proves nothing. Undecided checks are
not diagnosed until a strictness policy decides how, but a `wf` judgment
reports each.
