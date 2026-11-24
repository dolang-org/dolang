# Aliases and Imports

## Type Aliases

`@let` declares a reusable name for a type or schema. An alias has no runtime
binding, and is visible in types throughout its block, so it may refer to itself
or to an alias declared later. It may declare [binders](./generics.md) and may
be exported.

```
pub @let Pair[T] = Tuple[T, T]
@let Options = {name: Str, ?port: Int}
@let Json = (Scalar | Array[Json] | Dict[Str, Json])
@let Scalar = (Str | Int | Float | Bool | nil)
```

A long alias body can be written in
[vertical layout](./schemas.md#vertical-layout).

## Recursive Aliases

Recursive aliases must not reference themselves (including through other
aliases) without that reference being behind a function type, a class's type
arguments, or a schema. This property is known as "contractiveness".

A recursive generic alias must pass its binders unchanged wherever it
references itself. For example, `@let Tree[T] = (T | Array[Tree[T]])` is
allowed, but `@let Tree[T] = (T | Tree[Array[T]])` is an error.  This property
is known as "regularity".

Recursive aliases are considered compatible according to their structure.
A class is compared with a
[protocol that refers to itself](./classes.md#protocols) the same way.

## Checker-Only Imports

`@` before an item in an import's item list imports it for type checking
only. No binding is created, and the module is not loaded at runtime if only
types are imported from it.

```
import geometry:
  - distance
  - @Point
  @Vector: Offset

def shift p@Point by@Offset -> Point
  (p + by)
```

`@` before a whole module imports its name for types without loading it at
runtime:

```
import @geometry

def draw point @ geometry.Point
  echo $point
```

`@import` makes every module and item in the statement checker-only. Individual
`@` markers remain valid but are redundant:

```
@import geometry: g
@import
  geometry.plane
  geometry.solid:
    - Shape
    Vector: Offset

def draw point @ g.Point
  echo $point
```
