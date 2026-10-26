# Type Syntax

An annotation or return type is a compact type expression which admits only
dotted type names, application of generic arguments, and forms in `[]` or `{}`.
More complex type expressions require parentheses for grouping. This applies
*even in contexts that are otherwise space-insensitive*: in
`(do |x @ Int| -> Array[Int] [x])`, the space after `Array[Int]` ends the type.
Within a type's own `()`, `[]`, and `{}`, whitespace is insignificant.

| Syntax                                | Meaning                   |
| ------------------------------------- | ------------------------- |
| `Str`, `time.Duration`                | Named type                |
| `:SYM:`, `"str"`, `42`, `true`, `nil` | Constant                  |
| `Array[Int]`                          | Apply generic arguments   |
| `(Str | Path)`                        | Union                     |
| `(Int, Str)`, `(name: Str, Int)`      | Tuple or record           |
| `[Int]`                               | Array                     |
| `{name: Str, ?port: Int}`             | Dict or schema            |
| `(Int, ?Int) -> Int`                  | Function                  |

## Names

A name is an identifier, or a module name followed by `.`-separated names, such
as `time.Duration`.

A name refers to a binder, or to what the same identifier would refer to as a
variable where the type is written. A `class`, protocol, alias, or import can
be named anywhere in its block; any other binding must come before the type. A
dotted name must begin with an import.

The compiler does not consider types when it warns about unused variables, so a
binding named only in types is still reported as unused unless its name begins
with `_`. A [checker-only import](./type-declarations.md#checker-only-imports)
binds no runtime variable, so it is not reported.

## Constants

A constant type is a symbol, string, integer, boolean, or `nil`. A string
cannot contain interpolations.

```
let mode @ (:TARGET: | :LINK:) = :TARGET:
```

## Type Arguments

`[]` directly after a type applies type arguments to it. An argument is a type,
passed positionally, or `name: T`, passed by keyword:

```
let names @ Array[Str] = []
let index @ Dict[Str, Array[Int]] = {}
let result @ Record[value: Int, error: std.Error | nil] = (value: 1, error: nil)
```

An argument may also [expand a schema](#expanding-schemas) into further
arguments.

## Unions

`|` separates the members of a union. A union may begin with `|`, which lets
a long one break across lines:

```
let target @ (
  | Str
  | fs.Path
  | nil
) = nil
```

A value fits a union when it fits one of the members; see
[Unions](./type-checking.md#unions) for how the checker decides.

## Schemas

A schema is not itself a type, but a description of positional and keyed items
and their types which can parameterize a `Dict`, a rest parameter, etc. A brace
written where a type is required is a [dict type](#dicts). Schemas are closed:
they admit only the items they list, unless a [quantifier](#quantifiers) or an
[open item](#open-items) admits more.

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
```

Each item is an element, optionally preceded by a quantifier saying how many of
that element the schema admits:

| Element  | Is                                           |
| -------- | -------------------------------------------- |
| `T`      | A positional item                            |
| `key: T` | A keyed item                                 |
| `(K): T` | A keyed item whose keys type `K`             |
| `**V`    | Any number of symbol-keyed items of type `V` |

### Positional Items

A type alone is a positional item:

```
let pair @ Dict[{Str, Int}] = {"a", 1}
```

### Keyed Items

`key: T` is a keyed item. Bare keys are literal symbols; a key given as a type
describes the keys it admits, and a name must be parenthesized so that it is not
taken for a symbol.

```
let headers @ Dict[{host: Str, "x-custom": Str}] = {}
let codes @ Dict[{(Tuple[Int, Int]): Str}] = {}
```

### Inclusions

The quantifier `...` before a positional element includes the items of the
schema `S` in `...S`, which must be a schema rather than a type:

```
@let Colors = {?fg: Color, ?bg: Color}
let style @ Dict[{?width: Int, ...Colors}] = {}
```

### Quantifiers

A quantifier before an item says how many of the element the schema admits.
Without one it admits exactly one:

| Quantifier | Admits                                                |
| ---------- | ----------------------------------------------------- |
| `?`        | Zero or one                                           |
| `*`        | Zero or more                                          |
| `...`      | The items of a schema, as an [inclusion](#inclusions) |

An item takes at most one quantifier, so `?` cannot also repeat and an
inclusion cannot be repeated. `**V` is shorthand for `*(Sym): V` and takes no
quantifier.

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
let headers @ Dict[{*(Str): Str}] = {}
let cookies @ Dict[{*set_cookie: Str}] = {}
let flags @ Dict[{**Bool}] = {}
```

Since a `Dict` is a multi-map, a quantifier gives a literal key an exact
multiplicity: `key: V` admits one, `?key: V` at most one, and `*key: V` any
number.

### Open Items

`...` alone admits any further item, and is shorthand for `*, **`. `*` and `**`
alone admit any further item of their own kind:

| Item  | Admits                       |
| ----- | ---------------------------- |
| `...` | Any further items            |
| `*`   | Any further positional items |
| `**`  | Any further keyed items      |

```
let anything @ Dict[{...}] = {}
let positional @ Dict[{Str, *}] = {}
```

### Expanding Schemas

`...S` among type arguments expands the schema `S` into further arguments. A
schema is needed only for items that type arguments can't write directly, such
as optional ones. Expanding a type `T` rather than a schema is shorthand for
`...{*T}`:

```
let row @ Tuple[...Str] = Tuple ["id", "name"]
let conn @ Record[...{host: Str, ?port: Int}] = (host: "db")
```

### Types as Schema Arguments

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

## Collections

`()`, `[]`, and `{}` write the types of the matching literals. Each stands for a
standard class:

| Syntax                    | Stands for                      |
| ------------------------- | ------------------------------- |
| `(Int, Str)`              | `Tuple[Int, Str]`               |
| `(name: Str, Int)`        | `Record[...{name: Str, Int}]`   |
| `[Int]`                   | `Array[Int]`                    |
| `{name: Str, ?port: Int}` | `Dict[{name: Str, ?port: Int}]` |

Each form stands for the class itself, never a less specific type. A parameter
that only reads should name one such as `Seq[T]`, `Iterable[T]`, or
`BaseDict[S]`.

```
let pair @ (Int, Str) = (1, "a")
let names @ [Str] = []
let options @ {name: Str, ?port: Int} = {name: "db"}
def count rows @ [{id: Int}] -> Int
  rows.len
```

### Tuples and Records

A parenthesized form with an explicitly keyed item is a record: `key: T`, `(K):
T`, or an item quantified `**`. Otherwise it is a tuple. A single item without
a quantifier or trailing comma is merely a grouping.

| Syntax       | Is                  |
| ------------ | ------------------- |
| `(Int)`      | `Int`               |
| `(Int,)`     | A one-item tuple    |
| `()`         | The empty tuple     |
| `(*Int)`     | A tuple of any size |
| `(Int, **)`  | A record            |

An inclusion `...S` never makes a form a record. In a tuple, including a schema
that may admit keyed items is an error; write `Record[...S]` instead. For a
tuple of any items, write `(*)` rather than `(...)`.

### Arrays

`[T]` takes one element type. `[Int, Str]` and `[]` are errors; write a union
element such as `[Int | Str]`, or a tuple type.

### Dicts

A braced type expression is a `Dict` type only where a type is
required, and a schema everywhere else:

| Position                                                    | `{...}` is      |
| ----------------------------------------------------------- | --------------- |
| Annotation, return type, cast, union member                 | `Dict[{...}]`   |
| Argument for a type binder, as in `Array[{a: Int}]`         | `Dict[{...}]`   |
| Argument for a schema binder, as in `Dict[{a: Int}]`        | The schema      |
| Binder bound, as in `S @ {...}`                             | The schema      |
| Alias body, as in `@let Fields = {a: Int}`                  | The schema      |

A [vertical](#vertical-layout) schema written with `$` where a type is required
follows the rule for vertical data: it is a `Dict` if it has an explicitly
keyed item, and otherwise a `Tuple` of its items.

## Functions

`->` separates a function's parameters from its return type. Parameters are
written in `()`, with `?` before an item marking it optional. Parentheses can
be omitted for a single parameter. `->` groups to the right:

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

### Implicit Parameters

Every function also takes the strand's ambient input and output, which nothing
passes explicitly. `<T` gives the type a function reads from and `>T` the type
it writes to. Both are items of the parameter list, so they may appear in any
order among the others, but a list takes at most one of each:

```
def collect count @ Int <Iter[Int] >Sink[Str] -> nil
  for value = strand.input()
    strand.put $ str $value
let runner @ ((Int, <Iter[Int], >Sink[Str]) -> Int) = (do |n| n)
```

In a declaration the type is compact, as an annotation's is.

A def or method that omits one takes its caller's, whatever they are. A
function type in its signature that omits one takes the def's, so a block
passed to it runs with the caller's channels:

```
def with[R] block @ (() -> R) -> R
  block()
```

The body can't rely on an omitted channel being anything in particular: in a
[strict](./type-checking.md#strict-checking) file it is `Value` there, and
otherwise it is unchecked.

Anywhere else, such as a field, a `let`, a cast, an alias or a lambda's
parameter, an omitted channel is `Value`, so a function of that type neither
reads its input nor writes its output. A function that keeps a block to run
after it returns says so with `<Value, >Value`:

```
def on_click block @ ((<Value, >Value) -> nil)
  handlers.push $block
```

A channel may be any type, but only what fits a use of it can be done with it. A
`for` without an iteratee reads the input through its `(next)`, and
[`strand.put`](strand.put) requires a `Sink`.

A lambda's parameter list is delimited by `|`, so its implicits go inside:

```
let double = do |x <Iter[Int] >Sink[Int]| (x * 2)
```

Schemas have no implicits, since an ambient channel is not data, and neither do
`bind` arms or `let` patterns, which bind values.

## Vertical Layout

`$` followed by an indented body writes a schema in vertical layout, as in an
[alias](./type-declarations.md#type-aliases). Positional items may be bin-packed
on an undashed line. A dash introduces one positional item, and a keyed line
introduces one keyed item. Quantifiers and inclusions have the same meaning as
in `{}`.

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

A vertical schema where a type is required is a `Dict` or `Tuple` as described
under [Dicts](#dicts). Here `meta` is a `Dict[{owner: Str}]` and `at` is an
`(Int, Int)`:

```
@let Config = Dict $
  name: Str
  meta: $
    owner: Str
  at: $
    Int Int
```

## Type Utilities

The standard module provides aliases for computing types from schemas:

| Alias                          | Purpose                                                         |
| ------------------------------ | --------------------------------------------------------------- |
| [`Union`](std.Union)           | Forms a union from type arguments, including expanded schemas   |
| [`Never`](std.Never)           | Describes no values; a result for a function that never returns |
| [`Unknown`](std.Unknown)       | Leaves a declaration unchecked                                  |
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
