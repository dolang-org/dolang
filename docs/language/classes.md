# Classes

Do supports user-defined classes with fields, methods, getters and setters,
multiple inheritance, and special methods to control behavior such as iteration
and arithmetic operators.

## Defining a Class

A class is defined with the `class` keyword, followed by a name and an
indented body containing field declarations and method definitions:

```
class Point
  pub field x = 0
  pub field y = 0

  def (init) self x y
    self.x = x
    self.y = y

  pub def distance self
    (self.x * self.x + self.y * self.y)
```

### Fields

Fields are declared with `field` inside the class body. A single declaration may
introduce one or more fields:

```
class Config
  field host = "localhost"
  field port verbose
  field retries timeout = 0
```

Defaults values are optional. A field declared without `= ...` is initialized
to `nil`.

Defaults are evaluated when an instance is created, once per field:

```
class Pair
  pub field left right = []

let p = Pair()
p.left.push 1
assert_eq $p.left [1]
assert_eq $p.right []
```

### Methods

Methods are defined with `def` inside the class body. The first parameter is
conventionally named `self` and receives the instance:

```
class Counter
  field count = 0

  pub def increment self
    self.#count = (self.#count + 1)

  pub def value self
    self.#count
```

## Computed Fields with `getter` and `setter`

Computed fields use decorators on methods:

```
class Config
  field port = 8080

  #[getter]
  pub def port obj
    obj.#port

  #[setter]
  pub def port obj value
    obj.#port = value
```

Reads and writes still use ordinary field syntax:

```
let cfg = Config()
assert_eq $cfg.port 8080
cfg.port = 9000
assert_eq $cfg.port 9000
```

See [Decorators](./decorators.md) for decorator syntax and evaluation order, and
[`getter`](std.Getter) / [`setter`](std.Setter) for the
descriptor helpers.

## Class and Static Members

Members marked `#[class]` or `#[static]` live on the type object rather than on
instances. Reach them through the class, not through an instance:

```
class Counter
  #[class]
  pub field count = 0

  #[class]
  pub def bump cls
    cls.count = (cls.count + 1)
    cls.count

assert_eq $Counter.count 0
assert_eq $Counter.bump() 1
```

A class method receives the class it was reached through as its first
parameter, the way an instance method receives `self`.

### Inheritance

`class` members are inherited; `static` members are not. Because both occupy
the type object's namespace, a static shadows an inherited class member of the
same name without propagating that shadowing any further.

Each class gets its own storage for a class field, seeded from the declared
initializer. A subclass therefore starts from the default rather than sharing
the parent's cell, and a non-constant initializer runs once per class:

```
class Derived: Counter

assert_eq $Counter.bump() 2
assert_eq $Derived.count 0
assert_eq $Derived.bump() 1
assert_eq $Counter.count 2
```

Class and static members are a separate namespace from instance members, so a
class field and an instance field may share a name. Two members of the same
name in the type namespace — a `class` and a `static`, say — collide, and the
class fails to build.

### `(call)` as a Class or Static Member

`(call)` is the only special method that may be a class or static member. As an
instance method it allows instances to be called; as a class or static member it
replaces what happens when the *class* is called.

Declaring it `static` gives a class a factory that subclasses do not inherit,
so the factory can construct a subtype without re-entering itself:

```
class Shape
  pub field kind = ""

  def (init) self kind
    self.kind = kind

  #[static]
  pub def (call) cls kind
    echo "building $kind"
    Type.(call) $cls $kind

class Circle: Shape

let s = Shape "round"     # prints "building round"
let c = Circle "arc"      # no factory: Circle did not inherit it
```

`Type.(call)` performs the default instantiation the override replaced. It is
the ordinary unbound-method idiom — a class is an instance of `Type`, so this
invokes `Type`'s implementation rather than the receiver's override, exactly as
`Animal.(init) $self $name` calls a parent constructor.

### Unbound Class Methods

`Class.method` binds the class, so `Counter.bump()` always passes `Counter`.
`type(Class)` returns the class's type object, whose namespace holds the
class-level methods unbound, letting one class's implementation run against
another:

```
assert_eq (type(Counter).bump(Derived)) 2
```

Two type objects are equal when they stand for the same class, and they hash to
match, so they work as dict keys. A type object is a subtype of
[`Type`](std.Type), so `type Counter Type` holds.

## Visibility

By default, all fields and methods of a class are **private** — they can only
be accessed from within the class's own methods. To make a field or method
accessible from outside the class, declare it with `pub`:

```
class Counter
  field count = 0        # private field

  def (init) self start
    self.#count = start

  pub def increment self   # public method
    self.#count = (self.#count + 1)

  pub def value self       # public method
    self.#count
```

A class itself may also be declared `pub` to make it part of a module's public
API:

```
pub class Point
  pub field x = 0
  pub field y = 0

  def (init) self x y
    self.#x = x
    self.#y = y
```

## Private Fields

Fields declared without `pub` are private. Within the class, private fields are
accessed using the `.#field` syntax:

```
class BankAccount
  field balance = 0

  def (init) self initial
    self.#balance = initial

  pub def deposit self amount
    self.#balance = (self.#balance + amount)

  pub def balance self
    self.#balance
```

The `#` explicitly signals a private access. Using `.field` (without `#`) on
`self` when the field is private produces a warning, and the compiler will
suggest using `.#field` instead.

### Private Methods

Methods declared without `pub` are also private. Call them with `.#method()`
syntax from within the class:

```
class Adder
  field base = 0

  def (init) self base
    self.#base = base

  def double_base self       # private helper
    (self.#base * 2)

  pub def add self x
    (self.#double_base() + x)

let a = Adder 5
assert_eq $a.add(3) 13
```

## Creating Instances

Call a class like a function to create an instance. Arguments are passed to
`(init)`:

```
class Rectangle
  pub field width = 0
  pub field height = 0

  def (init) self w h
    self.width = w
    self.height = h

  pub def area self
    (self.width * self.height)

let r = Rectangle 10 20
echo $r.area()   # 200
echo $r.width    # 10
```

## Inheritance

A class can inherit from one or more parents by listing them after a colon.
Let's start with a base class:

```
class Animal
  pub field name = nil
  pub field species = "unknown"

  def (init) self name species
    self.name = name
    self.species = species

  pub def describe self
    "$(self.name) is a $(self.species)"
```

A child class inherits all fields and methods from its parent. Methods can be
overridden by redefining them. To call a parent method, use
`Parent.method $self`:

```
class Dog: Animal
  pub field breed = nil

  def (init) self name breed
    Animal.(init) $self $name dog
    self.breed = breed

  pub def description self
    "$(Animal.describe self) ($(self.breed))"
```

This results in the following behavior:

```
let rex = Dog "Rex" "German Shepherd"
echo $rex.describe()           # Rex is a dog (German Shepherd)

# Call a parent method directly
echo $ Animal.describe $rex    # Rex is a dog
```

### Multiple Inheritance

List multiple parents after the colon, separated by spaces:

```
class LoudDog: Animal Pet
  pub def describe self
    "$(Animal.describe self)! pet=$(Pet.category self)"
```

Superclass references may also be dotted names:

```
class LocalTool: tools.build.Tool
  pub field root = "."
```

### Member Resolution Order

- Earlier superclasses in the list win when the same member is defined multiple
  times
- A class's own members override inherited ones
- This rule is recursive: each parent brings along its already-merged inherited
  members.

For example:

```
class A
  pub def who self
    A

class B
  pub def who self
    B

class C: A B

assert_eq $(C().who()) $A
```

Swapping the parents changes the result:

```
class D: B A

assert_eq $(D().who()) B
```

### Calling Parent Constructor

Call the parent's `(init)` explicitly to initialize inherited fields:

```
class Cat: Animal
  pub field indoor = false

  def (init) self name indoor
    Animal.(init) $self $name cat
    self.indoor = indoor
```

### Inheriting from a Built-in Type

A class may also inherit from a built-in type such as
[`Str`](std.Str), [`RuntimeError`](std.RuntimeError), or
[`term.Geometry`](term.Geometry). Such a supertype brings a
representation of its own, which `(init)` must initialize by chaining:

```
class Tagged: Str
  pub field tag = nil

  def (init) self value tag
    Str.(init) $self $value
    self.tag = tag

let t = Tagged "hello" :greeting:
assert_eq (str t) "hello"
assert_eq $t.len 5
```

The chained call gives the instance its inherited behavior: `Tagged` above gets
`str`, `len`, comparison, and the rest from the `Str` it was initialized with.
Skipping the call leaves that representation empty, and the constructor fails
with `native supertypes not initialized` rather than producing a half-built
object. A few types — [`AbortError`](std.AbortError) among them —
are sealed and have no constructor to chain to.

Overriding a special method takes precedence over the inherited one, so a
subclass can keep the representation and still present itself differently:

```
class Quiet: Str
  def (init) self value
    Str.(init) $self $value

  pub def (dbg) self
    "<Quiet $(Str.(dbg) self)>"
```

## Type Inspection

The `type` builtin works with classes:

```
let rex = Dog "Rex" "German Shepherd"

# Get the type of a value (returns the type object)
assert_eq (type rex) $Dog

# Test if a value is an instance of a class
assert (type rex Dog)       # true: rex is a Dog
assert (type rex Animal)    # true: Dog inherits from Animal
assert_not (type rex Cat)   # false: Dog is not a Cat
```

See [Basic Types](basic-types.md#type-inspection) for more on `type`.

## Operator Overloading

Arithmetic, shift, bitwise, and comparison operators are dispatched to special
methods. Define the method corresponding to the operator:

```
class Vec2
  pub field x = 0
  pub field y = 0

  def (init) self x y
    self.x = x
    self.y = y

  def (add) self other
    Vec2 (self.x + other.x) (self.y + other.y)

  def (sub) self other
    Vec2 (self.x - other.x) (self.y - other.y)

  def (mul) self scalar
    Vec2 (self.x * scalar) (self.y * scalar)

  def (shl) self count
    Vec2 (self.x << count) (self.y << count)

  def (shr) self count
    Vec2 (self.x >> count) (self.y >> count)

  def (neg) self
    Vec2 (0 - self.x) (0 - self.y)

  def (eq) self other
    (self.x == other.x && self.y == other.y)

let a = Vec2 1 2
let b = Vec2 3 4
assert_eq (a + b) (Vec2 4 6)
assert_eq (b - a) (Vec2 2 2)
assert_eq (a * 3) (Vec2 3 6)
assert_eq (a << 1) (Vec2 2 4)
assert_eq (a >> 1) (Vec2 0 1)
assert_eq (-a) (Vec2 -1 -2)
assert (a == Vec2 1 2)
```

For binary operators, if the left operand does not define the method (because
it is a different type), the runtime tries the **reverse** variant on the right
operand. For example, `5 * myobj` first tries `int.(mul)`, and if that fails
for this operand type, falls back to `myobj.(rmul)`:

| Forward  | Reverse   | Operator |
| -------- | --------- | -------- |
| `(sub)`  | `(rsub)`  | `-`      |
| `(div)`  | `(rdiv)`  | `/`      |
| `(ediv)` | `(rediv)` | `//`     |
| `(mod)`  | `(rmod)`  | `%`      |

Shift operators do not have reverse variants. Use `(shl)` for `<<` and `(shr)`
for `>>`.

**Ordering:** Defining `(lt)` and `(eq)` is sufficient for all four comparison
operators. `<=`, `>`, and `>=` are derived automatically:

```
class Num
  pub field val = 0

  def (init) self val
    self.val = val

  def (lt) self other
    (self.val < other.val)

  def (eq) self other
    (self.val == other.val)

let n1 = Num 1
let n2 = Num 2
assert (n1 < n2)
assert (n1 <= n2)
assert (n2 > n1)
assert (n2 >= n1)
```

## Special Method Reference

Special methods integrate class instances with language features. They are
defined with the method name in parentheses.

### `(init)`: Constructor

Called when a new instance is created. Receives the new instance as the first
argument:

```
class Point
  field x = 0
  field y = 0

  def (init) self x y
    self.x = x
    self.y = y
```

### `(call)`: Function Call

Invoked when an instance is called like a function. As a class or static member
it instead governs calling the class itself; see
[Class and Static Members](#class-and-static-members).

```
class Multiplier
  field factor = 1

  def (init) self factor
    self.factor = factor

  def (call) self x
    (x * self.factor)

let double = Multiplier 2
echo (double 5)   # 10
```

### `(unpack)`: Destructuring

Return a more primitive type (such as a `dict`) for the runtime to destructure
in lieu of `self`:

```
class Point
  field x = 0
  field y = 0

  def (init) self x y
    self.x = x
    self.y = y

  def (unpack) self
    {x: self.x, y: self.y}

let p = Point 3 4
let :x :y = p
echo "$x, $y"   # 3, 4
```

### `(iter)`: Obtain Iterator

Invoked implicitly by for loops, certain iterator combinators, etc. Should
return an object supporting the iteration protocol: either a built-in type, or
a class instance that implements `(next)`:

```
class NumberRange
  field start = 0
  field stop = 0

  def (init) self start stop
    self.start = start
    self.stop = stop

  def (iter) self
    (Range start: self.start end: self.stop).iter()

let r = NumberRange 0 5
assert_eq [...r] [0, 1, 2, 3, 4]
```

### `(next)`: Iterator Protocol

Invoked when getting the next item from an iterator. Returns the next value,
or throws `IterStop` when exhausted:

```
import std:
  - IterStop

class Counter
  field current = 0
  field stop = 0

  def (init) self start stop
    self.current = start
    self.stop = stop

  def (iter) self
    self

  def (next) self
    if (self.current >= self.stop)
      throw IterStop()
    let value = self.current
    self.current = (self.current + 1)
    value
```

An iterator should conventionally implement `(iter)` by returning `self`.

### `(sink)`: Obtain Sink

Invoked to obtain a sink object, such as by `strand.put` or
`strand.redirect output: $instance`

```
class ListCollector
  field items = nil

  def (init) self
    self.items = []

  def (sink) self
    self.items.sink()

let collector = ListCollector()
redirect output: $collector do
  put 1
  put 2
  put 3
assert_eq $collector.items [0, 1, 2]
```

### `(put)`: Sink Protocol

Invoked when an object is written to a sink.

```
class Summer
  field sum = 0

  def (put) self value
    self.sum = (self.sum + value)

  def (sink) self
    self
```

A sink should conventionally implement `(sink)` by returning
`self`.

### `(bool)`: Boolean Conversion

Called when a value is used in a boolean context: `if`, `while`, `!`, `&&`,
`||`. Return a bool. If not defined, instances are always truthy:

```
class Vec2
  pub field x = 0
  pub field y = 0

  def (init) self x y
    self.x = x
    self.y = y

  def (bool) self
    (self.x != 0 || self.y != 0)

let zero = Vec2 0 0
let nonzero = Vec2 1 0
assert_not (bool zero)
assert (bool nonzero)
```

### `(hash)`: Hash Code

Called by `std.hash` and when an instance is used as a dictionary key. Must
return an `Int`. If not defined, the hash is derived from the instance's
identity (memory address), consistent with the default identity-based equality.

`std.hash` accepts multiple values and hashes them all together in sequence,
which makes it easy to combine fields:

```
import std:
  - hash

def (hash) self
  hash self.x self.y self.z
```

**Important:** if you define `(eq)`, you should also define `(hash)` so that
equal objects produce the same hash:

```
import std:
  - hash

class Point
  pub field x = 0
  pub field y = 0

  def (init) self x y
    self.x = x
    self.y = y

  def (eq) self other
    (self.x == other.x && self.y == other.y)

  def (hash) self
    (self.x * 31 + self.y)

let p1 = Point 3 4
let p2 = Point 3 4
assert_eq (hash p1) (hash p2)   # equal objects, equal hashes

# Can be used as dict keys
let d = {}
d[p1] = "hello"
assert_eq $d[p2] "hello"
```

### `(str)`: String Conversion

Called when an instance is converted to a string via `str()` or used in string
interpolation. Must return a `Str`. Falls back to `(dbg)` if not defined:

```
class Point
  pub field x = 0
  pub field y = 0

  def (init) self x y
    self.x = x
    self.y = y

  def (str) self
    "($(self.x), $(self.y))"

let p = Point 3 4
echo "Point is $p"   # Point is (3, 4)
```

### `(dbg)`: Debug String

Called for debug/inspect output and as a fallback when `(str)` is not defined.
Must return a `Str`. If neither `(str)` nor `(dbg)` is defined, the instance
displays as `<object>`:

```
class Node
  pub field val = 0

  def (init) self val
    self.val = val

  def (dbg) self
    "Node($(self.val))"
```

### `(verbatim)`: Verbatim Representation

Called when an instance is converted to its verbatim representation, including
when it is interpolated into an external command (e.g. `echo $obj` in a shell
context). Must return a `Str`. Falls back to
`(str)` if not defined, which in turn falls back to `(dbg)`:

```
class Path
  pub field parts

  def (init) self ...parts
    self.parts = parts

  def (verbatim) self
    self.parts.join("/")

  def (str) self
    "Path($(self.parts.join("/")))"
```

### `(fmt)`: Formatted Conversion

Called when an instance is formatted with a specification, as written by a
[formatted interpolation](./strings.md#formatted-interpolation) or built
with [`FmtSpec`](std.FmtSpec). Receives a
[`FmtSpec`](std.FmtSpec) and must return a `Str`.

The `kind` field of the specification says which conversion the surrounding
operation asked for, and [`FmtSpec.pad`](std.FmtSpec.pad)
applies the fill, alignment, width, and precision to a string, so a class that
only wants the standard layout applied to its own text is one line:

```
class Field
  pub field name = ""

  def (init) self name
    self.name = name

  def (fmt) self spec
    spec.pad(self.name)

let field = Field "id"
assert_eq "${field:6}" "id    "
```

A class that defines no `(fmt)` still gets width and alignment: the default
applies `(verbatim)`, `(str)`, or `(dbg)` according to `kind` and then pads the
result. A class inheriting a native type delegates to that type instead, so a
subclass of `Int` or `Float` keeps the numeric options:

```
class Money: Float
  def (init) self value
    Float.(init) $self $value

let price = Money 1.5
assert_eq "${price:+.2f}" "+1.50"
```

Defining `(fmt)` replaces both, including the default's rejection of numeric
kinds and of options such as `sign` — an instance handed a specification it does
not understand should raise an error itself.

### `(index)` and `(assign)`: Subscript Access

`(index)` is called for `instance[key]` reads; `(assign)` is called for
`instance[key] = value` writes:

```
class Table
  pub field data = nil

  def (init) self
    self.data = {}

  def (index) self key
    self.data[key]

  def (assign) self key value
    self.data[key] = value

let t = Table()
t["x"] = 10
assert_eq $t["x"] 10
```

### `(get)` and `(set)`: Dynamic Field Fallback

Called when a field or method is accessed on an instance and no matching field,
method, or getter/setter exists. Receives `self` and the field name as a
symbol:

```
class Dynamic
  field data

  def (init) self
    self.#data = {}

  def (get) self key
    self.#data[key]

  def (set) self key value
    self.data[key] = value

let d = Dynamic()
d.foo = 42
d.bar = "hello"
assert_eq $d.foo 42
assert_eq $d.bar "hello"
assert_eq (d.(get) :foo:) 42

d.(set) :baz: 99
assert_eq $d.baz 99
```
