# Type Syntax

An annotation or return type is a compact type expression which admits only
dotted type names, application of generic arguments; whitespace terminates the
type expression. More complex type expressions require delimiters for grouping.
This applies *even in contexts that are otherwise space-insensitive*: in `(do
|x @ Int| -> Array[Int] [x])`, the space after `Array[Int]` ends the type and
begins the lambda body. Within a type's own `()`, `[]`, and `{}`, whitespace is
insignificant.

| Syntax                                | Meaning                    |
| ------------------------------------- | -------------------------- |
| `Str`, `time.Duration`                | Named type                 |
| `:SYM:`, `"str"`, `42`, `true`, `nil` | Constant                   |
| `Array[Int]`                          | Apply generic arguments    |
| `(Str | Path)`                        | Union                      |
| `(Int, Str)`, `(name: Str, Int)`      | Tuple or `Pack`            |
| `{name: Str, ?port: Int}`             | [Schema](./schemas.md)     |
| `((Int, ?Int) -> Int)`                | [Function](./functions.md) |

## Names

A name is an identifier, or a `.`-separated name such as `time.Duration`.

A name may refers to a [binder](./generics.md), or to what the same identifier
would refer to as a variable where the type is written. A `class`, protocol,
alias, or import can be named anywhere in its block; any other binding must come
before the type. A dotted name must begin with an import.

The compiler does not consider types when it warns about unused variables, so a
binding named only in types is still reported as unused unless its name begins
with `_`. A [checker-only import](./aliases.md#checker-only-imports) can be
used to avoid binding a runtime variable.

## Constants

A constant type is a symbol, string, integer, boolean, or `nil`. A string
cannot contain interpolations.

```
let mode @ (:TARGET: | :LINK:) = :TARGET:
```

## Type Arguments

`[]` directly after a type applies type arguments to it. An argument is a type
or schema, passed positionally or by key:

```
let names @ Array[Str] = []
let index @ Dict[Str, Array[Int]] = {}
let result @ Pack[value: Int, error: std.Error | nil] = (value: 1, error: nil)
```

As in a call, `:Name` is short for `Name: Name`:

```
class Keyed[T, :Key = Int]
  pub field key @ Key = nil

def key_of[:Key] x @ Keyed[Str, :Key] -> K
  x.key
```

An argument may also [expand a schema](./schemas.md#expanding-schemas) into
further arguments.

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
[Unions](./checking.md#unions) for how the checker decides.

## Collections

Parenthesized forms write the types of the matching literals. Each stands for a
standard class:

| Syntax             | Stands for             |
| ------------------ | ---------------------- |
| `(Int, Str)`       | `Tuple[Int, Str]`      |
| `(name: Str, Int)` | `Pack[name: Str, Int]` |

Other collection types name their class. A parameter that only reads should name
a less specific type than `Array[T]` or `Dict[S]`, such as `Seq[T]`,
`Iterable[T]`, or `BaseDict[S]`; see [Standard Types](./standard.md).

```
let pair @ (Int, Str) = (1, "a")
let names @ Array[Str] = []
let options @ Dict[{name: Str, ?port: Int}] = {name: "db"}
def count rows @ Seq[Dict[{id: Int}]] -> Int
  rows.len
```

### Tuples and Packs

A parenthesized form with an explicitly keyed item is a `Pack`: `key: T`, `(K):
T`, or an item quantified `**`. Otherwise it is a tuple. A single item without
a quantifier or trailing comma is merely a grouping.

| Syntax      | Is                  |
| ----------- | ------------------- |
| `(Int)`     | `Int`               |
| `(Int,)`    | A one-item tuple    |
| `()`        | The empty tuple     |
| `(*Int)`    | A tuple of any size |
| `(Int, **)` | A `Pack`            |

In parentheses, `**V`, `**`, and `...` admit only symbol keys; see [Keys by
Delimiter](./schemas.md#keys-by-delimiter).

An inclusion `...S` never makes a form a `Pack`. In a tuple, including a schema
that may admit keyed items is an error; write `Pack[...S]` instead. For a
tuple of any items, write `(*)` rather than `(...)`.
