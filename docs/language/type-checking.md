# Type Checking

The type checker uses [type annotations](./types.md) to check calls,
assignments, returns, and class members. `dolang --check` runs it without
executing the script; see
[Checking Scripts](../shell/index.md#checking-scripts).

Checking is gradual: unknown information leaves operations unchecked rather than
making every unannotated program an error. An imported module with neither
source nor a bundled type library is unknown to the checker. Where a non-strict
file uses a form the checker doesn't handle, it leaves that code unchecked
without a diagnostic, so a successful result is not a guarantee that every
operation has been checked.

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

A `do` block's result is inferred from its body. Its unannotated parameters can
take their types from the signature of the function it is passed to, or from a
function type expected of it by an annotation or a [cast](./types.md#casts).

Unknown type information is distinct from [`Value`](std.Value). `Value` admits
every runtime value, but few operations are available on it. Unknown type
information allows uses that the checker cannot verify.

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
[final](./classes.md#final-classes) class not inheriting from the tested class,
and a member [disjoint](./classes.md#disjoint-classes) from it.

A pattern that unpacks a union value links the value to the items it binds.
A later test on any of them narrows the others to the union's members that
remain. Dict entries and keyed rests unpack this way, key with value.

```
def size p @ ((:text:, Str) | (:count:, Int)) -> Int
  let kind v = p
  if (kind == :text:)
    # v is Str, p is (:text:, Str)
    v.len
  else
    v
```

A copy of a linked variable shares its link. Reassigning a variable, or
unpacking it with another pattern, ends its link; so does unpacking the same
value again.

Reassignment replaces the local's current type, and branches join it again. A
variable assigned by a closure is never narrowed; copy such a variable to a
local before testing and using that value.

## Type Compatibility

A subtype can be used where its supertype is expected. A union accepts values
of any of its alternatives. Function compatibility reverses the direction for
parameters: a replacement must accept everything the caller may pass, and
return a value the caller expects. Strand input and output types are also
checked as implicit parameters.

### Unions

A value fits a union when it fits one of the members. A mismatch with every
member is an error, except that the check is left undecided when the value's
class inherits from more than one generic member's class, such as
`BaseArray[Int]` and `BaseArray[Str]`. A value may fit several applications of
a generic class together without fitting any one of them: every
`(Int | Str,)` is an `(Int,)` or a `(Str,)`.

### Recursive Types

Recursive types fit each other when their structures do. While the checker
compares `A` with `B`, it assumes that `A` fits `B` wherever the comparison
meets the same pair again. So `List[Int]` fits `List[Num]` for
`@let List[T] = (nil | (T, List[T]))` when `Int` fits `Num`, and two aliases
with the same structure fit each other. A comparison that would hold only
because a type has no values, as for `@let Endless = (Endless, Int)`, is not
proven. Neither is a comparison where which member of a union fits depends
on which member fits elsewhere.

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
do not determine a class's variance.

### Class Objects

A class's name is exactly its class object. The checker shows its type as
`<type Shape>`, which no annotation can write. It fits `Type[Shape]`, the type
of the class object of `Shape` or of any subclass:

```
class Shape
  #[class]
  pub def unit _cls -> Int
    1

class Circle: Shape

def measure cls @ Type[Shape] -> Int
  cls.unit()

measure $Shape
measure $Circle
```

Since a `Type[Shape]` may hold a subclass's object, only the class's name
constructs through `(init)`, including where a function is expected, and
reaches static members, which subclasses don't inherit. A `Type[Shape]` is
called only through a `#[class]` `(call)`. A class method's unannotated first
parameter is `Type` of its class.

## Strict Checking

A file opts into strict checking with a directive comment on one of its first 10
lines:

```
# dolang: strict
```

A strict file must annotate its named functions' parameters and its classes'
fields. A method's receiver is still typed by its class, and a `do` block still
takes its types from where it is passed. Omissions that remain have conservative
types rather than unknown ones:

- An omitted return type is `nil`
- An omitted `<` or `>` is `Value` in the function's body, so it neither reads
    its input nor writes its output. A function that reads its input, or calls
    one that does, annotates `<`, and one that writes annotates `>`. So does a
    `do` block that isn't passed straight to a call and has no expected type
    from an annotation or cast:

    ```
    let report = do |>Sink[Str]|
      put done
    ```

A pattern that raises when it doesn't match, in `let`, `bind`, `for`, or a
block's parameters, must match every value its scrutinee's type admits. A
strict file gets a warning when the value may lack an item the pattern requires,
or have one it doesn't take. Type tests and constants in the pattern are
exempt. To handle a mismatch, give the `let` or `bind` an
[`else`](patterns.md#let-bind-with-else):

```
let k v = s.split "="
else throw std.ValueError "expected KEY=VALUE: $s"
```

Code the checker can't verify, such as a form it doesn't handle, gets a warning
in a strict file, with a note giving the reason.

To leave one declaration unchecked in a strict file, annotate it with
[`std.Unknown`](std.Unknown), the unknown type, rather than omitting the
annotation:

```
def (get) self name @ Str -> std.Unknown
  self.#values[name]
```

`# dolang: nostrict` turns strict checking off. Strictness belongs to the file,
so strict and non-strict modules can import each other; `--strict` is unrelated
and only makes warnings fail the check.
