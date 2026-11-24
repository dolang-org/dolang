# Checking

The checker attempts to infer types where they are not given by
[annotations](./annotations.md) and checks types against annotations where they
are provided. A [cast](./annotations.md#casts) can similarly give an expression
a type where inference can't supply one, or override its inference.

Inference is only performed on local variables and blocks/lambdas. `def`s must
have parameters and return types explicitly annotated (with the exception of
method self parameters), or else they are treated as `Unknown`, preventing the
checker from verifying their subsequent use. `Unknown` is separate from
`Value`, the universal supertype of all values, which is an ordinary type
with a small number of supported operations.

## Inference Rules

A local binding without an annotation takes its type from its assigned value.
Assignments update that type, and convergent program flow (for example, the
statement after an if/else block) takes the union of types that reach it. An
annotation constrains the values assigned to the binding, which are checked
against it.

```
let count = 1
let label @ Str = "ready"
```

A call of a [generic](./generics.md) function infers its binders from the
arguments.

### Literal Decay

A scalar literal value has its own type: `1` is `1`, `:a:` is `:a:`, etc. When
such a literal appears in a mutable collection, its type decays
(e.g. to `Int` or `Sym`):

```
# Array is mutable, so literals decay
let xs = [1, 2]       # Array[Int]
# Tuple is immutable; no literal decay
let pair = (1, "a")   # (1, "a")
```

A literal written in an annotation or cast never decays.

## Narrowing

Condition tests refine a local variable's type on the branches they select.
Narrowing is most useful on union types, where members are dropped from the
union according to the test.

The checker recognizes truth tests, comparisons with literals, `(type value
Class)` tests, and comparisons of `(type value)` with a class object. A truthy
test removes `nil` and `false`, but its false branch does not assume the value
is one of those: other values can be falsy. Short-circuiting operators also
carry narrowing into right-hand operand.

A variable which is mutated by a closure that captures it can never be
narrowed. Copy it to a fresh local and narrow the copy instead.

### Truthiness and Equality

```
def describe value @ (Str | nil) -> Str
  if (value == nil)
    "missing"
  else
    value
```

A comparison with a literal or a truth test does not remove arbitrary
classes from a union, as classes can define their own `(eq)` and `(bool)`
methods with arbitrary behavior. `Nil`, `Bool` and `Sym` are final, so a
comparison *can* drop them from a union. `Str`, for example, is not final,
so a comparison can't exclude it in general:

```
def show x @ (Bool | Sym | Str)
  if (x == true)
    # x is true | Str
    echo $x
```

### Type Tests

A type test can narrow the type of a value to a more specific subtype.

```
class Animal
  pub def name _self -> Str
    "animal"

class Dog: Animal
  pub def bark _self -> Str
    "woof"

def speak a @ Animal -> Str
  if (type a Dog)
    # a is Dog
    a.bark()
  else
    a.name()
```

Note that narrowing to a generic type can't determine type parameters. In
non-strict units, they are replaced with `Unknown`. In strict units, covariant
parameters are substituted with `Value` and contravariant with `Never`, while
invariant parameters prevent narrowing entirely.

```
def inspect x @ Value
  if (type x Tuple)
    # Tuple is covariant: x is Tuple[*Value]
    echo $x.len
  if (type x Sink)
    # Sink is contravariant: x is Sink[Never]
    echo $x
  if (type x Array)
    # Array is invariant: x is still Value
    echo $x
```

A type test drops a union member that is a
[final](../language/classes.md#final-classes) class not inheriting from the
tested class, or a member [disjoint](./classes.md#disjoint-classes) from it.
`#[mixable]` classes are effectively treated like their nearest disjoint
ancestor when determining disjointness:

```
def report x @ (std.TypeError | Dict[Str, Str])
  if (type x Dict)
    # TypeError is mixable, but its nearest disjoint ancestor, RuntimeError, is
    # unrelated to Dict, which is also disjoint.  This allows `TypeError` to
    # be dropped, retaining the generc type arguments to `Dict`, which otherwise
    # could not be determined from a type test.
    # x is Dict[Str, Str]
    echo $x
```

### Correlated Narrowing

A pattern that unpacks a union value links the value to the items it binds.
A later test on any of them narrows the others to the union's members that
are not excluded by the test.

```
def size p @ ((:TEXT:, Str) | (:COUNT:, Int)) -> Int
  let kind v = p
  if (kind == :TEXT:)
    # v is Str, p is (:TEXT:, Str)
    v.len
  else
    v
```

A copy of a linked variable is narrowed along with it. Reassigning a variable,
unpacking it, or unpacking the original value again breaks any prior links.

## Type Compatibility

A subtype can be used where its supertype is expected. A union accepts values
of any of its alternatives. Function compatibility reverses the direction for
parameters: a replacement must accept everything the caller may pass, and
return a value the caller expects. Strand input and output types are also
checked as [implicit parameters](./functions.md#implicit-parameters).

Recursive aliases are compared by structure; see
[Recursive Aliases](./aliases.md#recursive-aliases).

### Unions

A value fits a union when it fits one of the members; a mismatch with every
member is an error. More granular set-theoretic inclusions are not modeled.
They are accepted silently in gradual units and result in a warning in strict
units.

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
both positions makes it invariant. `(init)` and ordinary private members do not
determine a class's variance. A private field of type [`Phantom`](std.Phantom)
forces a variance restriction according to its use of a binder.

## Kinds

Every binder and alias has a kind: it stands for either a type or a
[schema](./schemas.md). The checker infers kinds from declarations alone,
never from how a name is used:

- A variadic binder is a schema.
- A binder has the kind of its bound, so `S @ {...}` is a schema.
- An alias has the kind of its body.
- Anything else is a type.

A type argument of the wrong kind is generally an error:

```
@let Options = {name: Str, ?port: Int}

def record_id[S @ Options] row @ Dict[{id: Int, ...S}] -> Int
  row[:id:]

# `S` is a type because nothing makes it a schema
def loose_id[S] row @ Dict[{id: Int, ...S}] -> Int   # error: expected a schema
  row[:id:]
```
