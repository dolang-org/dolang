# Type Declarations

These declarations name and parameterize types for the
[type checker](./type-checking.md): binders for generic functions and classes,
type aliases, checker-only imports, overloads, and protocols. None of them
changes runtime behavior.

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
def lookup[K @ (Str | Sym), V = nil] key@K -> V
  nil
```

### Schema Binders

A binder other than a variadic one takes a single argument: a type, unless a
schema bound such as `S @ {...}` makes it take a schema. A variadic binder
instead collects the remaining arguments into a schema. The two differ only in
the arguments they accept; within the declaration, either binder is a schema.
Like any schema, it becomes type arguments only through
[expansion](./type-syntax.md#expanding-schemas), so `class Tuple[*Ts]` is passed
on as `Tuple[...Ts]`.

### Mapped Rests

A leading `...` after a rest binding's `@` maps a type over a schema binder,
giving each collected argument its own type. Here each block returns the
corresponding item of `Rs`:

```
def fork[*Rs] *blocks @ ...(() -> Rs) -> Tuple[...Rs]
  ...
```

The mapping marker is accepted only on rest bindings.

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

A long alias body can be written in
[vertical layout](./type-syntax.md#vertical-layout).

Recursive aliases must describe a value through a class's type arguments, a
function type, or a schema item on every cycle. A union or another alias alone
does not provide that structure: `@let Loop = (Int | Loop)` is rejected.

A recursive generic alias must pass its binders unchanged within the cycle.
For example, `@let Tree[T] = (T | Array[Tree[T]])` is allowed, but replacing
`Tree[T]` there with `Tree[Array[T]]` is not. See
[Recursive Types](./type-checking.md#recursive-types) for how they are
compared.

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

## Overloads

`@def` declares an overload: a signature for a function that the checker uses
for calls whose arguments match it.

```
@def double x @ Int -> Int

@def double x @ Float -> Float

pub def double x @ (Float | Int) -> (Float | Int)
  (x + x)
```

An overload has no runtime effect. It doesn't take `pub`; it's exported if
its implementation is. A method, including a special method such as `(init)`,
takes overloads in its class body the same way, and so does a protocol's
method.

A method's overload may annotate its receiver with a narrower type, so that it
applies only to those instances. The class's binders in the overload then stand
for the receiver's type arguments, so `shout` below returns `Str`. An
implementation's receiver annotation can't narrow the class this way.

```
class Box[T]
  pub field item @ T

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
arguments; the implementation is not considered. Failure to select an overload
is an error.

Overloads do not dispatch at runtime; every call runs the same implementation.
Their signatures are trusted assertions about the implementation's behavior
that the checker does not verify.

## Protocols

`@class` declares a protocol, which describes the fields and methods exposed by
implementing types. Its methods have no bodies, and its fields have no defaults.

```
pub @class Shape
  pub field name @ Str
  pub def area self -> Int
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
protocol's members with a compatible type; a field may be provided by a
getter and a setter.

A protocol's own supertypes are checker-only already, so they are written
without `@`:

```
pub @class Solid: Shape
  pub def volume self -> Int
```

A protocol may name a runtime class as a supertype, but claiming the protocol
doesn't inherit it: a class claiming the protocol must inherit that class
itself. Protocols are never [disjoint](./classes.md#disjoint-classes).
