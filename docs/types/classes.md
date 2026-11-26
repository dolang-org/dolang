# Classes and Protocols

## Fields

A field is annotated as any other binding is. A field declaration that names
several fields gives them all its annotation, just as it gives them all its
default value:

```
class Point
  pub field x y @ Int = 0
```

A field's default must fit its annotation, except for the placeholders
described under [Field Defaults](#field-defaults).

## Supertype Arguments

A superclass can take type arguments:

```
class Registry[V]: Table[Sym, V]
```

## Protocols

`@class` declares a protocol, which describes the fields and methods exposed by
implementing types. Its methods have no bodies, and its fields have no defaults.

```
pub @class Shape
  pub field name @ Str
  pub def area self -> Int
```

Protocols have no corresponding class objects at runtime, so they can't be used
in runtime type tests. A class conforms to a protocol when it has each of the
protocol's public members with a compatible type, and inherits each class the
protocol names. It doesn't need to name the protocol:

```
class Circle
  pub field name @ Str = "circle"
  pub def area _self -> Int
    3

pub def describe shape @ Shape
  echo "$(shape.name): $(shape.area())"

describe (Circle())
```

A generic protocol's arguments are inferred from the members: a class whose
`get` returns `Int` conforms to `Source[Int]` for a protocol `Source[T]` with
`pub def get self -> T`.

A protocol's members may name the protocol itself, directly or through another
protocol. Conformance checking of such recursive protocols must eventually
"bottom" out in the manner of recursive aliases.

A protocol that names no runtime superclass, and whose public members are all
ones every value has, such as `(str)` and `(hash)`, would admit every possible
value. The compiler issues a warning in this case. One with no public members
at all is treated as an intentional marker protocol and does not generate a
warning; a class must explicitly claim it to conform to it.

The standard protocols are summarized under [Standard
Types](./standard.md#protocols).

## Protocol Claims

A class can declare that it implements a protocol by naming it as a supertype
with `@`:

```
class Square: @Shape
  pub field name = "square"
  pub def area _self
    1
```

The type checker holds the class to the claim at its declaration: it must
provide each of the protocol's members with a compatible type; a field may be
provided by a getter and a setter.

A protocol's own supertypes are always written without `@`.

```
pub @class Solid: Shape
  pub def volume self -> Int
```

A protocol may name a runtime class as a supertype, but claiming the protocol
doesn't inherit it; a class claiming the protocol must inherit that class
itself. Protocols are never [disjoint](#disjoint-classes).

A class or protocol may name a generic protocol more than once with different
arguments. Each instance adds its members' signatures as overloads, so a `Seq`
is indexed both by integers and by ranges:

```
pub @class Seq[T]: Index[T] Index[{*(Range[Int]): Seq[T]}]
```

Such a protocol isn't an ancestor of the declaration naming it: a value is below
it by its members. A generic class reached more than once is reached as its
first instance, which must be below the others.

## Receiver Binders

A method may take its receiver through a binder bounded by its class. An
override, or a method meeting a protocol's, may bound that binder by its own
class instead, provided the binder appears elsewhere in the signature only in
positive positions, such as the result:

```
class Base
  pub def copy[X @ Base] self @ X -> X
    self

class Sub: Base
  pub def copy[X @ Sub] self @ X -> X
    self
```

With a parameter `other @ X`, a call through a `Base` could pass any `Base` as
`other`, so the override can't narrow the bound. A class method's binder is
narrowed the same way inside `Type`, as in `cls @ Type[X]`.

## Disjoint Classes

Every class is disjoint unless it is decorated with the prelude's
[`mixable`](std.mixable). A class can't inherit from two disjoint classes
unless one of the two inherits from the other.

```
#[mixable]
class Named
  pub field name = nil

class Shape
class Image
class Square: Shape Named
class Icon: Image Named

class Tile: Square Image   # error: unrelated disjoint classes `Square` and `Image`
```

Diamond inheritance is allowed when every class in its middle is mixable.
Classes whose nearest disjoint superclasses are unrelated have no instances in
common, so a [type test](./checking.md#narrowing) for one can narrow the other
out of a union.

A [final](../language/classes.md#final-classes) class has no subclasses, so the
checker treats its values as outside every class it doesn't inherit from.

## Object Initialization

The checker verifies that a class's `(init)` initializes all its annotated
fields to valid values by the time the instance becomes observable outside
`(init)`. Until then, a field may hold a placeholder.

### Field Defaults

A field's default must fit its annotation, unless it is `nil` or a symbol. Such
a default is a sentinel: a placeholder that `(init)` must replace. A field
declared without a default starts as `nil`, so it also holds a sentinel.

```
class Connection
  field host @ Str = nil
  field port @ Int = :UNSET:
  pub field retries @ Int = 3

  def (init) self host@Str port@Int
    self.#host = host
    self.#port = port
```

A class is responsible for only the fields it declares directly, not those of
its superclasses or subclasses.

### Escaping `self`

Within `(init)`, `self` escapes when:

- It's used other than to access one of its class's private fields: as an
  argument, a method's receiver, or an operand, or to access a public field.
- A function capturing it is created, whether or not it runs.
- `(init)` returns normally, including through a `return` in a `do` block.

Each field must fit its annotation wherever `self` escapes. Before that, a read
of a private field has the type assigned on the paths reaching it; after, it has
the field's annotation.

```
class Server
  field host @ Str = nil
  field port @ Int = nil

  def (init) self host@Str port@Int
    self.#host = host
    self.#port = port
    self.listen()

  pub def listen self
    echo "listening on $(self.#host):$(self.#port)"
```

A path that does not return normally, such as by `throw`ing or calling a
function returning [`Never`](std.Never), is not considered a `self` escape.

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

A subclass may override a field with a getter and a setter. The getter may
return a narrower type, but the setter must accept the field's whole type. A
field may likewise override a property: its type must fit what the getter
returns and accept what the setter takes.

Writing a public field may run a setter from a subclass override, so `self` is
conservatively treated as escaping. A
[final](../language/classes.md#final-classes) class's public fields can't be
overridden, so they're accessed as private fields are.

### Superclass Initialization

`(init)` must call each direct superclass's `(init)`, where the superclass has
or inherits one, before `self` otherwise escapes or `(init)` returns. The call
must name the superclass directly, as in `Animal.(init) $self`. This is
considered a `self` escape for the purpose of field initialization, so fields
directly declared by the class must be initialized first:

```
class Animal
  pub field name @ Str = nil

  def (init) self name @ Str
    self.name = name

class Dog: Animal
  field breed @ Str = nil

  def (init) self name@Str breed@Str
    self.#breed = breed
    Animal.(init) $self $name
    echo "$(self.name) the $(self.#breed)"
```

A default `(init)`, implicitly provided if none is specified, calls only the
first superclass's `(init)`.

## Class Objects

A class's name is exactly its class object. It is a subtype of `Type[Name]`,
the type of the class object of `Name` or of any subclass:

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
