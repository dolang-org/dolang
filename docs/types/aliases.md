# Aliases and Imports

## Type Aliases

`@let` declares a name for a type or schema. An alias has no runtime binding,
and is visible in types throughout its block, so it may refer to itself or to an
alias declared later. It may declare [binders](./generics.md) and may be
exported.

```
pub @let Pair[T] = Tuple[T, T]
@let Options = {name: Str, ?port: Int}
@let Json = (Scalar | Array[Json] | Dict[Str, Json])
@let Scalar = (Str | Int | Float | Bool | nil)
```

A long alias body can be written in
[vertical layout](./schemas.md#vertical-layout).

## Recursive Aliases

Recursive aliases must describe a value through a class's type arguments, a
function type, or a schema item on every cycle. A union or another alias alone
does not provide that structure: `@let Loop = (Int | Loop)` is rejected.

A recursive generic alias must pass its binders unchanged within the cycle.
For example, `@let Tree[T] = (T | Array[Tree[T]])` is allowed, but replacing
`Tree[T]` there with `Tree[Array[T]]` is not.

Recursive types fit each other when their structures do. For
`@let List[T] = (nil | (T, List[T]))`, `List[Int]` fits `List[Int | Str]`
because `Int` fits `Int | Str`, and two aliases with the same structure fit each
other. A comparison that would hold only because a type has no values, as for
`@let Endless = (Endless, Int)`, is not proven. A class is compared with a
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
