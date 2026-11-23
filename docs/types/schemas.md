# Schemas

A schema is not itself a type, but a description of positional and keyed items
and their types which can parameterize type, describe an argument pack, etc.
Schemas are closed: they admit only the items they list, unless a
[quantifier](#quantifiers) or an [open item](#open-items) admits more.

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
```

A schema may appear:

- As the argument for a schema parameter, as in `Dict[{name: Str}]`
- [Expanded](#expanding-schemas) among type arguments, as in
  `Record[...{Int, ?tag: Str}]`, or [included](#inclusions) in another schema
- As a binder's bound, as in `S @ {...}`
- As the body of a type alias, as in `@let Colors = {?fg: Color, ?bg: Color}`
- As the annotation of a [rest binding](./annotations.md#rest-bindings), or the
  parameter list of a [function type](./functions.md)

Each item is an element, optionally preceded by a quantifier saying how many of
that element the schema admits:

| Element  | Is                                           |
| -------- | -------------------------------------------- |
| `T`      | A positional item                            |
| `key: T` | A keyed item                                 |
| `(K): T` | A keyed item whose keys type `K`             |
| `**V`    | Any number of keyed items of type `V`        |

## Positional Items

A type alone is a positional item:

```
let pair @ Record[...{Str, Int}] = {"a", 1}
```

## Keyed Items

`key: T` is a keyed item. Bare keys are literal symbols; a key given as a type
describes the keys it admits, and a name must be parenthesized so that it is not
taken for a symbol.

```
let headers @ Dict[{host: Str, "x-custom": Str}] = {}
let codes @ Dict[{(Tuple[Int, Int]): Str}] = {}
```

## Inclusions

The quantifier `...` before a positional element includes the items of the
schema `S` in `...S`, which must be a schema rather than a type:

```
@let Colors = {?fg: Color, ?bg: Color}
let style @ Dict[{?width: Int, ...Colors}] = {}
```

## Quantifiers

A quantifier before an item says how many of the element the schema admits.
Without one it admits exactly one:

| Quantifier | Admits                                                |
| ---------- | ----------------------------------------------------- |
| `?`        | Zero or one                                           |
| `*`        | Zero or more                                          |
| `...`      | The items of a schema, as an [inclusion](#inclusions) |

An item takes at most one quantifier, so `?` cannot also repeat and an
inclusion cannot be repeated. `**V` is shorthand for `*(Value): V`, a repeat of
any key, and takes no quantifier.

```
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
let headers @ Dict[{*(Str): Str}] = {}
let cookies @ Dict[{*set_cookie: Str}] = {}
let flags @ Dict[{**Bool}] = {}
```

Since a `Dict` is a multi-map, a quantifier gives a literal key an exact
multiplicity: `key: V` admits one, `?key: V` at most one, and `*key: V` any
number.

## Open Items

`...` alone admits any further item, and is shorthand for `*, **`. `*` and `**`
alone admit any further item of their own kind:

| Item  | Admits                       |
| ----- | ---------------------------- |
| `...` | Any further items            |
| `*`   | Any further positional items |
| `**`  | Any further keyed items      |

```
let anything @ Spread[{...}] = {}
let positional @ Tuple[...{Str, *}] = ("a", 1)
```

## Keys by Delimiter

The delimiter a list is written in decides the keys of `**V`, `**`, and `...`.
In a schema's braces they admit any key. In parentheses, the parameters of a
[function type](./functions.md) or a [tuple or `Pack`
type](./syntax.md#tuples-and-packs), they admit only symbols. Write `*(Sym): V`
in a schema for symbol keys alone:

```
let counts @ Dict[{**Int}] = {"a": 1, b: 2}
let named @ Dict[{*(Sym): Int}] = {b: 2}
let options @ (**Int) = (b: 2)
```

An alias keeps the meaning of where it is written. Including `@let Counts =
{**Int}` in a function type's parameters, as in `((...Counts) -> nil)`, is an
error, because its keys need not be symbols.

## Expanding Schemas

In a list of [type arguments](./syntax.md#type-arguments), `...S` expands the
schema `S` into further arguments, as an [inclusion](#inclusions) does within a
schema. This lets a type argument list hold items that need a quantifier, like
`?port: Int` below. Expanding a type `T` rather than a schema is shorthand for
`...{*T}`:

```
let row @ Tuple[...Str] = Tuple ["id", "name"]
let conn @ Pack[...{host: Str, ?port: Int}] = (host: "db")
```

## Types as Schema Arguments

A type whose only positional parameter is a schema, such as `Dict` or `Spread`,
also accepts types in its place. Any other parameters must be key
parameters, which are passed by name as usual:

| Shorthand    | Stands for        |
| ------------ | ----------------- |
| `Spread[T]`  | `Spread[{*T}]`    |
| `Dict[K, V]` | `Dict[{*(K): V}]` |

An argument that is already a schema, including a binder bounded by one, is
passed as is:

```
let headers @ Dict[Str, Str] = {}
class Table[S @ {...}]
  pub field rows @ Dict[S] = {}
```

## Computing Types from Schemas

[Projections](./standard.md#projections) such as [`Keys`](std.Keys),
[`Values`](std.Values), and [`IndexItem`](std.IndexItem) compute types from a
schema:

```
@import std:
  - Keys
  - Values
  - IndexItem

@let Person = {name: Str, age: Int}
@let PersonKey = Keys[Person]
@let PersonValue = Values[Person]
@let NameValue = IndexItem[Person, :name:]
```

Here `PersonKey` is `(:name: | :age:)`, `PersonValue` is `(Str | Int)`, and
`NameValue` is `Str`.

## Vertical Layout

`$` followed by an indented body writes a schema in vertical layout, as in an
[alias](./aliases.md). Positional items may be bin-packed on an undashed line. A
dash introduces one positional item, and a keyed line introduces one keyed item.
Quantifiers and inclusions have the same meaning as in `{}`.

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
