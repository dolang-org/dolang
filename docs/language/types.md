# Type Annotations

Type annotations record what a binding is expected to hold. The type checker
uses them to check calls, assignments, returns, and class members. Annotations
do not enforce checks at runtime; a class's runtime supertype still determines
its inheritance.

## Checking Types

Run the type checker without executing the script:

```bash
dolang --check script.dol
```

The checker follows imports using the ordinary module search paths and uses
bundled type libraries when no source module is found. Add
`--module-path DIRECTORY` to search another directory. `--strict` makes warnings
fail the check too. See [Running Scripts](../shell/index.md#running-scripts)
for the shell's options.

Checking is gradual: unknown information leaves operations unchecked rather
than making every unannotated program an error. An imported module with neither
source nor a bundled type library is unknown to the checker. Some unsupported
type forms are provisionally accepted, so a successful check is not a guarantee
that every operation has been checked.

### Inference

A local binding without an annotation takes its type from its assigned value.
Assignments update that type, and convergent program flow (for example, the
statement after an if/else block) takes the union of types that reach it. An
annotation constrains the values assigned to the binding, which are checked
against it.

```
let count = 1
let label @ Str = "ready"
```

A named function's omitted parameter and return annotations remain unknown; the
checker does not infer its public signature from its body. Annotate the
signature to check callers and returned values. An unannotated instance-method
receiver takes the enclosing class's type, including its binders.

A `do` block's result is inferred from its body. When passed as an argument, its
unannotated parameters can take their types from the signature of the function
it is passed to.

Unknown type information is distinct from [`Value`](std.Value). `Value` admits
every runtime value, but few operations are available on it. Unknown types
allows uses that the checker cannot verify.

### Narrowing

Conditions refine a local variable's type on the branches they select. The
checker recognizes comparisons with literals. It also recognizes
`type value Class` tests and comparisons of `type value` with a class object.

```
def describe value @ (Str | nil) -> Str
  if (value == nil)
    "missing"
  else
    value
```

A truthy test removes `nil` and `false`, but its false branch does not assume
the value is one of those: other values can be falsy. Short-circuit operators
also carry narrowing into the operand that runs after the test.

Reassignment replaces the local's current type, and branches join it again.
A variable assigned by another function is not narrowed, since a call may
change it. Copy such a variable to a local before testing and using that value.

## Type Compatibility

A subtype can be used where its supertype is expected. A union accepts values
of any of its alternatives. Function compatibility reverses the direction for
parameters: a replacement must accept everything the caller may pass, and
return a value the caller expects. Ambient input and output types are also
checked as parameters.

### Generic Variance

The checker infers how each binder affects compatibility from the declaration's
members and supertypes. A covariant binder allows a subtype argument where a
supertype argument is expected; a contravariant binder reverses that direction.
An invariant binder requires compatibility in both directions.

```
class Reader[T]
  pub def read _self -> T
    throw "not implemented"

class Cell[T]
  pub field value @ T = nil
```

`Reader` is covariant in `T` because it only returns `T`. `Cell` is invariant
because callers can both read and write its public field. A callable's
parameters use a binder contravariantly and its result covariantly; using it in
both positions makes it invariant. Constructors and ordinary private members
do not determine a class's variance.

[`Phantom`](std.Phantom) marks a binder's use without storing a corresponding
value. A field annotated `Phantom[T]` contributes a covariant use even when
private; `Phantom[(T -> nil)]` contributes a contravariant use.

## Annotation

`@` after a binding (parameter, let, etc.) and before any default value
annotates that binding. Whitespace is optional on either side of the `@`. By
convention, space is used on both sides except where several bindings are
introduced on the same line, in which case no space is used.

```
let count @ Int = 0
let :name@Str :age@Int = record

for key@Sym value@Int = scores
  echo "$key: $value"

def connect :host@Str = "localhost" :port@Int = 8080
  echo "Connecting to $host:$port"
```

### Rest Bindings

The annotation on a rest parameter gives the type of each item it collects. A
schema instead describes the complete argument pack:

```
def log level@Sym ...parts@Str
  echo "[$level]" ...parts

def tag name@Str *children@Str **attrs@Str
  [name, children, attrs]

def configure ...options@{name: Str, ?port: Int}
  apply ...options
```

A leading `...` after `@` expands a type pattern over a pack:

```
def fork[*Rs] *thunks @ ...(() -> Rs) -> Tuple[...Rs]
  ...
```

The expansion marker is accepted only on rest bindings.

### Fields

A field declaration that names several fields gives them all its annotation,
just as it gives them all its default value:

```
class Point
  pub field x y @ Int = 0
```

## Return Types

`->` followed by whitespace and a type gives a function's return type. It comes
after parameters for single-line `def`s, or after the `do` for vertical
parameters:

```
def add a@Int b@Int -> Int
  (a + b)

def greeting() -> Str
  "hello"

def build
  :tag @ Str
  *args @ Str
do -> Array[Str]
  [tag, ...args]
```

A `do` block's return type follows its parameters:

```
let double = do |x @ Int| -> Int (x * 2)
let halve = (do |x @ Int| -> Int x // 2)
```

## Binders

`[]` directly after the name of a `def` or `class` declares binders: names that
stand for types within the declaration.

```
def first[T] items @ Array[T] -> T
  items[0]

class Table[K, V]
  pub field rows @ Dict[K, V] = {}
```

### Binder Forms

| Binder    | Binds                                                       |
| --------- | ----------------------------------------------------------- |
| `name`    | A positional type argument                                  |
| `:name`   | A keyword type argument                                     |
| `...name` | Any number of further positional and keyword type arguments |
| `*name`   | Any number of further positional type arguments             |
| `**name`  | Any number of further keyword type arguments                |

The last three are variadic binders. Binders of any form may appear in any
order, and a declaration may have several variadic binders:

```
def apply[R, *Ps, **Ks] func@((*Ps, **Ks) -> R) *args@Ps **kw@Ks -> R
  func ...args ...kw
```

### Bounds and Defaults

`@` gives a binder a bound and `=` gives it a default; both are type
expressions. Variadic binders cannot have defaults.

```
def lookup[K @ Hashable, V = nil] key@K -> V
  nil
```

### Schema Binders

A variadic binder stands for the remaining arguments, so it is always a schema
binder. Any other binder is a type binder unless a schema bound such as
`S @ {...}` makes it a schema binder.

### Supertype Arguments

A superclass can take type arguments:

```
class Registry[V]: Table[Sym, V]
```

## Type Aliases

`@let` declares a name for a type or schema. An alias has no runtime binding,
and is visible in types throughout its block, so it may refer to itself or to an
alias declared later. It may declare binders and may be exported.

```
pub @let Pair[T] = Tuple[T, T]
@let Options = {name: Str, ?port: Int}
@let Json = (Scalar | Array[Json] | Dict[Str, Json])
@let Scalar = (Str | Int | Float | Bool | nil)
```

Recursive aliases must describe a value through a class's type arguments, a
function type, or a schema item on every cycle. A union or another alias alone
does not provide that structure: `@let Loop = (Int | Loop)` is rejected.

A recursive generic alias must pass its binders unchanged within the cycle.
For example, `@let Tree[T] = (T | Array[Tree[T]])` is allowed, but replacing
`Tree[T]` there with `Tree[Array[T]]` is not.

### Vertical Layout

`$` followed by an indented body defines an alias or schema in vertical layout.
Positional items may be bin-packed on an undashed line. A dash introduces one
positional item, and a keyed line introduces one keyed item. Quantifiers and
schema includes have the same meaning as in `{}`.

```
@let Arguments = $
  Str Int
  - (Path | nil)
  ?verbose: Bool
  ...Options
```

If the first item starts with `|`, the body instead defines a union. Every
alternative starts with `|` and contains one type.

```
@let Json = $
  | Scalar
  | Array[Json]
  | Dict[Str, Json]
```

`Type $` applies a vertical schema as one type argument. If the type already
has arguments in `[]`, the schema is appended to them. `Type ...$` instead
expands the schema into type arguments, as `Type[...{...}]` does.

```
@let Pair = Tuple ...$
  Str Int

@let Config = Dict $
  host: Str
  ?port: Int
  credentials:
    | nil
    | Dict $
        user: Str
        token: Str
```

An indented value under a key defines a union without another `$`. Its first
`|` must be on an indented line. A union may also start directly after a dash:

```
@let Input = $
  - | Str
    | Bin
  ?encoding: Str
```

The first token after `-` or `|` establishes an implicit indentation level,
as it does in vertical arguments or data literals.

## Type-Only Imports

`@` before an item in an import's item list imports it for type annotation
only. No binding is created, and the module is not loaded at runtime if only
types are imported from it.

```
import geometry:
  - distance
  - @Point
  - @Vector: Offset

def shift p@Point by@Offset -> Point
  (p + by)
```

`@` before a whole module imports its name for types without loading it at
runtime:

```
import @geometry
let point @ geometry.Point = nil
```

`@import` makes every module and item in the statement type-only. Individual `@`
markers remain valid but are redundant:

```
@import geometry: g
@import
  geometry.plane
  geometry.solid:
    - Shape
    Vector: Offset

let point @ g.Point = nil
```

## Overloads

`@def` refines the signature of a function a body based on the type and shape
of arguments passed to it.

```
@def double x @ Int -> Int

@def double x @ Float -> Float

pub def double x @ (Float | Int) -> (Float | Int)
  (x + x)
```

An overload has no runtime binding. It doesn't take `pub`; it's exported if
its implementation is. A method, including a special method such as `(init)`,
takes overloads in its class body the same way, and so does a protocol's
method.

A method's overload may narrow its receiver to the instances it applies to. The
class's binders in the overload take the arguments the receiver reaches the
class with, so `shout` below returns `Str`. An implementation's receiver
annotation can't narrow the class this way.

```
class Box[T]
  pub field item @ T = nil

  @def shout self @ Box[Str] -> T
  pub def shout self
    self.item.upper()
```

Overloads have relaxed parameter shape requirements: rest parameters may appear
anywhere and more than once, and a required parameter may follow an optional
one.

```
@def pipeline[*Rs, R] *stages@...(() -> Rs) last@(() -> R) -> R
```

At a call, the checker uses an overload if exactly one is compatible with the
arguments. If none or several are, it falls back to the implementation's
signature. A function with a single overload always uses it, and checks the
arguments against it.

Overloads do not dispatch at runtime; every call runs the same implementation.
Their signatures are trusted assertions about the implementation's behavior
that the checker does not verify.

## Protocols

`@class` declares a protocol, which describes the fields and methods exposed by
implementing types. Its methods have no bodies, and its fields have no defaults.
A method's overloads are declared with `@def`, as a class's are:

```
pub @class Shape
  pub field name @ Str
  @def area self -> Int
  @def area self scale @ Int -> Int
  pub def area self scale @ Int = 1 -> Int
```

Protocols have no corresponding type objects at runtime. A class can indicate
it implements a protocol by naming it as a supertype with `@`:

```
class Square: @Shape
  pub field name = "square"
  pub def area _self
    1
```

The type checker holds the class to the claim: it must provide each of the
protocol's members with a compatible type, and a field may be provided by a
getter and a setter.

A protocol's own supertypes are type-only already, so they are written without
`@`:

```
pub @class Solid: Shape
  pub def volume self -> Int
```

A protocol may name a runtime class as a supertype, but claiming the protocol
doesn't inherit it: a class claiming the protocol must inherit that class
itself.

## Type Syntax

An annotation or return type is a compact type expression which admits only
dotted type names and application of generic arguments. More complex type
expressions require parentheses for grouping. This applies
*even in contexts that are otherwise space-insensitive*: in
`(do |x @ Int| -> Array[Int] [x])`, the space after `Array[Int]` ends the type.
Within a type's own `()`, `[]`, and `{}`, whitespace is insignificant.

| Syntax                                | Meaning                   |
| ------------------------------------- | ------------------------- |
| `Str`, `time.Duration`                | Named type                |
| `:SYM:`, `"str"`, `42`, `true`, `nil` | Constant                  |
| `Array[Int]`                          | Apply generic arguments   |
| `(Str \| Path)`                       | Union                     |
| `{name: Str, ?port: Int}`             | Schema                    |
| `(Int, ?Int) -> Int`                  | Function                  |

### Names

A name is an identifier, or a module name followed by `.`-separated names, such
as `time.Duration`.

A name refers to a binder, or to what the same identifier would refer to as a
variable where the type is written. A `class`, protocol, alias, or import can
be named anywhere in its block; any other binding must come before the type. A
dotted name must begin with an import.

The compiler does not consider types when it warns about unused variables, so a
binding named only in types is still reported as unused unless its name begins
with `_`. A [type-only import](#type-only-imports) binds no runtime variable,
so it is not reported.

### Constants

A constant type is a symbol, string, integer, boolean, or `nil`. A string
cannot contain interpolations.

```
let mode @ (:TARGET: | :LINK:) = :TARGET:
```

### Type Arguments

`[]` directly after a type applies type arguments to it. Each argument is one
of:

| Argument  | Meaning                                         |
| --------- | ----------------------------------------------- |
| `T`       | A positional argument                           |
| `name: T` | A keyword argument                              |
| `...S`    | Expands the schema `S` into further arguments   |

Expansion of a plain type is shorthand for `...{*T}`

```
let names @ Array[Str] = []
let index @ Dict[Str, Array[Int]] = {}
let row @ Tuple[...Str] = Tuple ["id", "name"]
let result @ Record[value: Int, error: Error | nil] = nil
let open @ Record[...{name: Str, ...}] = nil
```

### Unions

`|` separates the members of a union. A union may begin with `|`, which lets
a long one break across lines:

```
let target @ (
  | Str
  | fs.Path
  | nil
) = nil
```

### Schemas

A schema is not itself a type, but a description of positional and keyed items
and their types which can parameterize a `Dict`, argument pack, etc. Schemas are
closed: they admit only the items they list, unless a
[quantifier](./types.md#quantifiers) or an [open item](./types.md#open-items)
admits more.

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
```

Each item is an element, optionally preceded by a quantifier saying how many of
that element the schema admits:

| Element  | Is                               |
| -------- | -------------------------------- |
| `T`      | A positional item                |
| `key: T` | A keyed item                     |
| `(K): T` | A keyed item whose keys type `K` |
| `...S`   | The items of the schema `S`      |

#### Positional Items

A type alone is a positional item:

```
let pair @ Dict[{Str, Int}] = {"a", 1}
```

#### Keyed Items

`key: T` is a keyed item. Bare keys are literal symbols; a key given as a type
describes the keys it admits, and a name must be parenthesized so that it is not
taken for a symbol.

```
let headers @ Dict[{host: Str, "x-custom": Str}] = {}
let codes @ Dict[{(Tuple[Int, Int]): Str}] = {}
```

#### Inclusions

`...S` includes the items of the schema `S`, which must be a schema rather than
a type:

```
@let Colors = {?fg: Color, ?bg: Color}
let style @ Dict[{?width: Int, ...Colors}] = {}
```

#### Quantifiers

A quantifier before an item says how many of the element the schema admits.
Without one it admits exactly one:

| Quantifier | Admits                                         |
| ---------- | ---------------------------------------------- |
| `?`        | Zero or one                                    |
| `*`        | Zero or more                                   |
| `**`       | Zero or more keyed items; `**V` is `*(Sym): V` |

An item takes at most one quantifier, so `?` cannot also repeat.

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
let headers @ Dict[{*(Str): Str}] = {}
let cookies @ Dict[{*set_cookie: Str}] = {}
let pairs @ Dict[{*...{Str, Int}}] = {}
```

Since a `Dict` is a multi-map, a quantifier gives a literal key an exact
multiplicity: `key: V` admits one, `?key: V` at most one, and `*key: V` any
number.

#### Open Items

`...` alone admits any further item, and is shorthand for `*, **`. A quantifier
alone admits any further item of its own kind:

| Item  | Admits                       |
| ----- | ---------------------------- |
| `...` | Any further items            |
| `*`   | Any further positional items |
| `**`  | Any further keyed items      |

```
let anything @ Dict[{...}] = {}
let positional @ Dict[{Str, *}] = {}
```

#### Types as Schema Arguments

A type whose only positional parameter is a schema, such as `Dict`, also
accepts types in its place. Any other parameters must be keyword parameters,
which are passed by name as usual:

| Shorthand    | Stands for          |
| ------------ | ------------------- |
| `Dict[T]`    | `Dict[{*T}]`        |
| `Dict[K, V]` | `Dict[{*(K): V}]`   |

An argument that is already a schema, including a binder bounded by one, is
passed as is:

```
let headers @ Dict[Str, Str] = {}
class Table[S @ {...}]
  pub field rows @ Dict[S] = {}
```

### Functions

`->` separates a function's parameters from its return type. Parameters are
written in `()`, with `?` before an item marking it optional. Parentheses can be
omitted for a single parameter. `->` groups to the right:

```
let add @ ((Int, ?Int) -> Int) = do |a b = 0| (a + b)
let address @ ((Str, ?port: Int) -> Str) = do |host :port = 80| "$host:$port"
let curried @ (Int -> Int -> Int) = do |a| do |b| (a + b)
let thunk @ (() -> Str) = do "hello"
```

A parameter list is a schema, so its items take the same elements and
quantifiers, and a repeating one may appear anywhere and more than once. A
parameter's key must be a name, so a key given as a type is not allowed; write
`**V` for a list that takes any keyword.

```
let log @ ((Sym, *Str, **Str) -> nil) = do |level *parts **opts| echo "[$level]" ...parts ...opts
let shout @ ((...) -> nil) = do |...args| nil
```

#### Implicit Parameters

Every function also takes the strand's ambient input and output, which nothing
passes explicitly. `<T` gives the type a function reads from and `>T` the type
it writes to. Both are items of the parameter list, so they may appear in any
order among the others, but a list takes at most one of each:

```
def collect count @ Int <Iter[Int] >Sink[Str] -> nil
  for value = strand.input()
    strand.put $ str $value
let runner @ ((Int, <Iter[Int], >Sink[Str]) -> Int) = nil
```

Omitting one says nothing about that channel, which is what most functions
want. In a declaration the type is compact, as an annotation's is, so a union
needs parentheses: `<(Iter[Int] | nil)`.

A lambda's parameter list is delimited by `|`, so its implicits go inside:

```
let double = do |x <Iter[Int] >Sink[Int]| (x * 2)
```

Schemas have no implicits, since an ambient channel is not data, and neither do
`bind` arms or `let` patterns, which bind values.

## Type Utilities

The standard module provides aliases for computing types from packs and schemas:

| Alias                          | Purpose                                                         |
| ------------------------------ | --------------------------------------------------------------- |
| [`Union`](std.Union)           | Forms a union from type arguments, including expanded packs     |
| [`Never`](std.Never)           | Describes no values; a result for a function that never returns |
| [`Keys`](std.Keys)             | Collects a schema's key types                                   |
| [`Values`](std.Values)         | Collects a schema's value types                                 |
| [`Entries`](std.Entries)       | Keeps each schema key paired with its value type                |
| [`IndexItem`](std.IndexItem)   | Computes the value type a read at a key may produce             |
| [`AssignItem`](std.AssignItem) | Computes the value type a write at a key must accept            |

```
@import std:
  - Keys
  - Values
  - IndexItem

@let Fields = {name: Str, age: Int}
@let FieldName = Keys[Fields]
@let FieldValue = Values[Fields]
@let NameValue = IndexItem[Fields, :name:]
```

Here `FieldName` is `(:name: | :age:)`, `FieldValue` is `(Str | Int)`, and
`NameValue` is `Str`. See each alias's API reference for its handling of
repeated items and key selection.
