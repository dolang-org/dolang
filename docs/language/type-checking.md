# Type Checking

The type checker uses [type annotations](./types.md) to check calls,
assignments, returns, and class members. `dolang --check` runs it without
executing the script; see
[Checking Scripts](../shell/index.md#checking-scripts).

Checking is gradual: unknown information leaves operations unchecked rather than
making every unannotated program an error. Importing a module that has neither
source nor a bundled type library is an error, and uses of that module are left
unchecked. Where a non-strict file uses a form the checker doesn't handle, it
leaves that code unchecked without a diagnostic, so a successful result is not a
guarantee that every operation has been checked.

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

A copy of a linked variable is narrowed along with it. Reassigning a variable,
or unpacking it or the original value again, breaks its link.

Reassignment replaces the local's current type, and branches join it again. A
variable assigned by a closure is never narrowed; copy such a variable to a
local before testing and using that value.

## Object Initialization

The checker follows a class's `(init)` to verify that its annotated fields hold
values that fit by the time the instance becomes observable outside `(init)`.
Until then, a field may hold a placeholder.

### Field Defaults

A field's default must fit its annotation, unless it is `nil` or a symbol. Such
a default is a sentinel: a placeholder that `(init)` must replace. A field
declared without a default starts as `nil`, so it also holds a sentinel.

```
class Connection
  field host @ Str = nil
  field port @ Int = :unset:
  pub field retries @ Int = 3

  def (init) self host @ Str port @ Int
    self.#host = host
    self.#port = port
```

A class is responsible for the fields it declares; inherited fields are its
superclasses' responsibility. A class without an `(init)` of its own runs a
default one, which only applies the defaults, so each of its sentinels is an
error, even if a superclass's `(init)` runs after it.

### Escaping `self`

Within `(init)`, `self` escapes when:

- It's used other than to access one of its class's private fields: as an
    argument, a method's receiver, or an operand, or to access a public field.
- A function capturing it is created, whether or not it runs.
- `(init)` returns, including through a `return` in a `do` block. A `catch`
    that lets `(init)` continue may skip assignments in its `try` body.

Each field must fit its annotation wherever `self` escapes. Before that, a read
of a private field has the type assigned on the paths reaching it; after, it has
the field's annotation.

```
class Server
  field host @ Str = nil
  field port @ Int = nil

  def (init) self host @ Str port @ Int
    self.#host = host
    self.#port = port
    self.listen()

  pub def listen self
    echo "listening on $(self.#host):$(self.#port)"
```

A path that ends with `throw`, or with a call to a function returning
[`Never`](std.Never), doesn't return, so a field it leaves unassigned isn't an
error:

```
def fail msg @ Str -> std.Never
  throw msg

class Port
  pub field number @ Int

  def (init) self number @ Int
    if (number > 0)
      self.number = number
    else
      fail "port must be positive"
```

Writing a public field may run a setter that a subclass overrides, so `self`
escapes for every field except the one written. Only the first public field
written may hold a sentinel. A [final](./classes.md#final-classes) class's
public fields can't be overridden, so they're accessed as private fields are.

### Superclass Initialization

`(init)` must call each direct superclass's `(init)`, where the superclass has
or inherits one, before `self` otherwise escapes or `(init)` returns. The call
must name the superclass directly, as in `Animal.(init) $self`. The call accepts
a partly initialized `self` for the superclass's fields, but it is an escape for
the class's own, so assign those first:

```
class Animal
  pub field name @ Str = nil

  def (init) self name @ Str
    self.name = name

class Dog: Animal
  field breed @ Str = nil

  def (init) self name @ Str breed @ Str
    self.#breed = breed
    Animal.(init) $self $name
    echo "$(self.name) the $(self.#breed)"
```

Since writing a public field also escapes, a class whose superclass has an
`(init)` gives its own public fields defaults that fit, unless it is final.

A default `(init)` calls only the first superclass's `(init)`. A class with
several superclasses that have one defines its own `(init)` to call each.

## Type Compatibility

A subtype can be used where its supertype is expected. A union accepts values
of any of its alternatives. Function compatibility reverses the direction for
parameters: a replacement must accept everything the caller may pass, and
return a value the caller expects. Strand input and output types are also
checked as implicit parameters.

### Unions

A value fits a union when it fits one of the members, and a mismatch with every
member is an error. The checker can't verify a value whose class inherits from
more than one member's generic class, such as `BaseArray[Int]` and
`BaseArray[Str]`: such a value may fit the members together without fitting any
one of them, as every `(Int | Str,)` is an `(Int,)` or a `(Str,)`.

### Recursive Types

Recursive types fit each other when their structures do. For
`@let List[T] = (nil | (T, List[T]))`, `List[Int]` fits `List[Int | Str]`
because `Int` fits `Int | Str`, and two aliases with the same structure fit each
other. A comparison that would hold only because a type has no values, as for
`@let Endless = (Endless, Int)`, is not proven.

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

A `Type[Shape]` may hold a subclass's object, so it neither constructs through
`(init)` nor reaches static members, which subclasses don't inherit. Only the
name `Shape` does both, including when it's passed where a function is
expected. A `Type[Shape]` can be called only through a `#[class]` `(call)`
method. A class method's unannotated first parameter is `Type` of its class.

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
