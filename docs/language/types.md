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
  ...args @ Str
do -> Array[Str]
  [tag, ...args.pos_only()]
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
variadic binder must come last and cannot have a default. An unbounded binder
is provisionally a type binder; a schema bound such as `S @ {...}` explicitly
makes it a schema binder.

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

`let @` declares a name for a type or schema. An alias is visible in types
after its declaration and has no runtime binding. It may declare binders and
may be exported.

```
pub let @Pair[T] = Tuple[T, T]
let @Options = {name: Str, ?port: Int}
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
strings, as in dict literals. `?` marks an optional key. `...T` allows further
items with values of type `T`, or splices `T` when it is a schema. `...K: V`
allows further keyed entries whose keys have type `K` and values have type `V`.
Schemas are closed unless they contain a rest item. `{...}` is the universal
schema, shorthand for `{...std.Value}`:

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
let headers @ Dict[{...Str: Str}] = {}
let anything @ Dict[{...}] = {}
```

`Dict[K, V]` is shorthand for the keyed-rest form, roughly
`Dict[{...K: V}]`.

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
