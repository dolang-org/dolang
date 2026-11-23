# Checking

The checker uses [annotations](./annotations.md) where they are given and infers
types where they aren't. This page describes how it infers and narrows types,
and when one type fits another.

## Inference

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

A call of a [generic](./generics.md) function infers its binders from the
arguments. A [cast](./annotations.md#casts) gives an argument a type where its
own, such as an empty collection's, is too vague.

A `do` block's result is inferred from its body. Its unannotated parameters can
take their types from the signature of the function it is passed to, or from a
function type expected of it by an annotation or a cast.

Unknown type information is distinct from [`Value`](std.Value). `Value` admits
every runtime value, but few operations are available on it. Unknown type
information allows uses that the checker cannot verify.

### Literal Decay

A literal value has its own type: `1` is `1`, and `:a:` is `:a:`. Where a later
write could need a wider type, the literal decays to its class, such as `Int` or
`Sym`. A literal written in a type, as in an annotation, never decays.

A collection's literals decay at a position where its type is invariant, since
a later write through the collection could widen it there. An array's items
decay, and a tuple's, at a covariant position, don't:

```
let xs = [1, 2]       # Array[Int]
let pair = (1, "a")   # (1, "a")
```

A local assigned only once keeps its literals, except where its value could be
written through, as an array's items could be. A local assigned anywhere else
too, including in a `do` block that captures it, decays. So does an annotated
local, where its class fits the annotation, and so do fields, parameter
defaults and module exports.

```
let mode = :fast:     # :fast:
let level = 1
level = 2             # Int
let tag @ Sym = :a:   # Sym
```

A `for`'s or comprehension's iteratee keeps all of its literals, since nothing
else holds it:

```
for s = [:A:, :B:]
  echo $s             # s is :A: | :B:
```

## Narrowing

Conditions refine a local variable's type on the branches they select. The
checker recognizes comparisons with literals. It also recognizes
`(type value Class)` tests and comparisons of `(type value)` with a class
object.

A truthy test removes `nil` and `false`, but its false branch does not assume
the value is one of those: other values can be falsy. Short-circuit operators
also carry narrowing into the operand that runs after the test.

```
def describe value @ (Str | nil) -> Str
  if (value == nil)
    "missing"
  else
    value
```

A type test drops a union member that is a
[final](../language/classes.md#final-classes) class not inheriting from the
tested class, and a member [disjoint](./classes.md#disjoint-classes) from it.

A pattern that unpacks a union value links the value to the items it binds.
A later test on any of them narrows the others to the union's members that
remain. Dict entries and keyed rests unpack this way, key with value.

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
or unpacking it or the original value again, breaks its link.

Reassignment replaces the local's current type, and branches join it again. A
variable assigned by a closure is never narrowed; copy such a variable to a
local before testing and using that value.

## Type Compatibility

A subtype can be used where its supertype is expected. A union accepts values
of any of its alternatives. Function compatibility reverses the direction for
parameters: a replacement must accept everything the caller may pass, and
return a value the caller expects. Strand input and output types are also
checked as [implicit parameters](./functions.md#implicit-parameters).

Recursive aliases are compared by structure; see
[Recursive Aliases](./aliases.md#recursive-aliases).

### Unions

A value fits a union when it fits one of the members, and a mismatch with every
member is an error. The checker can't verify a value whose class inherits from
more than one member's generic class, such as `BaseArray[Int]` and
`BaseArray[Str]`: such a value may fit the members together without fitting any
one of them, as every `(Int | Str,)` is an `(Int,)` or a `(Str,)`.

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
do not determine a class's variance. A private field of type
[`Phantom`](std.Phantom) marks a variance that the class's other members don't
show.

## Kinds

Every binder and alias has a kind: it stands for either a type or a
[schema](./schemas.md). The checker infers kinds from declarations alone,
never from how a name is used:

- A variadic binder is a schema, and a key binder (`:name`) is a type.
- Any other binder has the kind of its bound, so `S @ {...}` is a schema, and
  so is `S @ Options` for an alias `Options` of a schema.
- An alias has the kind of its body.
- A binder or alias that nothing determines is a type.

A use that needs the other kind is an error. Only a schema can be included in
another, so `loose_id` below is rejected: its unbounded `S` is a type, even
though the inclusion would make sense only for a schema.

```
@let Options = {name: Str, ?port: Int}

def record_id[S @ Options] row @ Dict[{id: Int, ...S}] -> Int
  row[:id:]

def loose_id[S] row @ Dict[{id: Int, ...S}] -> Int   # error: expected a schema
  row[:id:]
```

A variadic binder's bound may be either kind: a type bounds each item, as in
`*Ts @ Int`, and a schema bounds the whole pack.
