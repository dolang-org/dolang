# Type Annotations

Type annotations record what a type a binding is expected to hold. They are
inert at runtime with the exception of class declarations. Type checking is not
presently implemented, so presently they only serve as documentation.

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

The annotation on a rest parameter gives the type of each item it collects. A
schema instead describes the complete argument pack:

```
def log level@Sym ...parts@Str
  echo "[$level]" ...parts

def configure ...options@{name: Str, ?port: Int}
  apply ...options
```

A leading `...` after `@` explicitly expands a type pattern over a pack:

```
def fork[...Rs] ...thunks @ ...(() -> Rs) -> Tuple[...Rs]
  ...
```

The expansion marker is accepted only on rest bindings, including rest items
in destructuring and `bind` patterns. Its operand is a compact type; enclose
function types and unions in parentheses.

A field declaration that names several fields share an annotation, just as
they share a default value:

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
stand for types within the declaration. A binder is a name, `:name` for a
keyword type argument, or `...name` for any number of further type arguments.
`@` gives a bound and `=` gives a default; both are type expressions. A
variadic binder must come last and cannot have a default. A variadic binder
stands for the remaining arguments, so it is always a schema binder. Any other
binder is a type binder unless a schema bound such as `S @ {...}` makes it a
schema binder.

```
def first[T] items @ Array[T] -> T
  items[0]

class Table[K, V]
  pub field rows @ Dict[K, V] = {}

def lookup[K @ Hashable, V = nil] key@K -> V
  nil
```

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

## Type-Only Imports

`@` before an item in an import's item list imports it for type annotation
only. No binding is created, and the module is not imported at all if only
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

Modules are not nested, so a module holds only the types declared in it, and a
dotted type name reaches no further than the module it imports. Modules sharing
a leading name are separate imports, written one per line under `import`:

```
import
  @geometry.plane
  @geometry.solid

let area @ geometry.plane.Area = nil
let volume @ geometry.solid.Volume = nil
```

`@import` makes every module and item in the statement type-only, including
renamed imports. Individual `@` markers remain valid but are redundant:

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

`@def` declares a signature for the function of the same name without giving it
a body. The signatures declared this way are the function's overloads, each a
way it can be called. They may appear anywhere in the block that declares the
function:

```
@def double x @ Int -> Int
@def double x @ Str -> Str
pub def double x
  (x + x)
```

An overload has no runtime binding. It is exported with its implementation, so
it is never written `pub`. A method, including a special method such as
`(init)`, takes overloads in its class body the same way.

## Protocols

`@class` declares a protocol, a type made up of the members a value has. A
protocol has no runtime binding. Its methods have no bodies, its fields have no
defaults, and methods sharing a name are overloads:

```
pub @class Shape
  pub field name @ Str
  pub def area self -> Int
  pub def area self scale @ Int -> Int
```

A protocol is structural: a value with its members is one of its instances,
whatever its class. A class claims a type with a type-only supertype, written
with `@`, which is not inherited at runtime and may name any type:

```
class Square: @Shape
  pub field name = "square"
  pub def area _self
    1
```

A protocol's own supertypes are type-only already, so they are written without
`@`:

```
pub @class Solid: Shape
  pub def volume self -> Int
```

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
variable where the type is written. A `def`, `class`, or import can be named
anywhere in its block; any other binding must come before the type. A dotted
name must begin with an import.

Documentation tools warn about a name that refers to nothing, a dotted name that
does not begin with an import, and a binder or type-only import that is never
used. The compiler does not consider types when it warns about unused variables,
so a binding named only in types is still reported as unused unless its name
begins with `_`. A [type-only import](#type-only-imports) binds no variable, so
it is not reported.

### Constants

A constant type is a symbol, string, integer, boolean, or `nil`. A string
cannot contain interpolations.

```
let mode @ (:TARGET: | :LINK:) = :TARGET:
```

### Type Arguments

`[]` directly after a type applies generic arguments.

The items of `[]`, `()`, and `{}` share positional, symbol-keyed, and
`...type` rest syntax. Keyed and open rest items are allowed in schemas and
generic applications, but not in function parameter lists.

```
let names @ Array[Str] = []
let index @ Dict[Str, Array[Int]] = {}
let row @ Tuple[...Str] = Tuple ["id", "name"]
let result @ Record[value: Int, error: (Error | nil)] = nil
let open @ Record[name: Str, ...] = nil
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

There is no shorthand for a type that also accepts `nil`; write `(T | nil)`.

### Schemas

A schema is not itself a type. It lists the positional and keyed entries of a
`Dict`, argument pack, or similar construct and the type of each value. Put it
inside `Dict[...]` to describe a dict. Bare keys are symbols and quoted keys are
strings, as in dict literals. Any other type may give a key, parenthesized when
it is a name, as in `{(K): V}`. `?` marks an optional key. `...T` allows further
items with values of type `T`, or splices `T` when it is a schema. `...K: V`
allows further keyed entries whose keys have type `K` and values have type `V`.
Schemas are closed unless they contain a rest item. `{...}` is the universal
schema, shorthand for `{...std.Value}`:

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
let headers @ Dict[{...Str: Str}] = {}
let anything @ Dict[{...}] = {}
```

A type whose only parameter is a schema, such as `Dict`, also accepts types in
its place. `Dict[T]` stands for a schema of any number of positional items of
type `T`, and `Dict[K, V]` stands for `Dict[{...K: V}]`. An argument that is
already a schema, including a binder bounded by one, is passed as is:

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

A required positional parameter cannot follow an optional one, and a rest
parameter cannot be optional. Parentheses that `->` does not follow must hold
exactly one type.
