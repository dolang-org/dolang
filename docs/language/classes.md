# Classes

Do supports user-defined classes with fields, methods, inheritance, and special
methods to overload behavior such as iteration and calling.

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

Fields are declared with `field` inside the class body. Each field has a default
value that is used when an instance is created:

```
class Config
  field host = "localhost"
  field port = 8080
  field verbose = false
```

!!! warning "Beware Mutable Default Values" Currently, all instances share the
same default field value. This is subject to change. Fields that are data
structures such as arrays or dictionaries should ideally be initialized to fresh
instances in the `(init)` method, described below.

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

Computed fields are declared with `#[getter]` and `#[setter]` decorators on
methods:

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

A class can extend another class by specifying a parent after a colon.
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

### Calling Parent Constructor

Call the parent's `(init)` explicitly to initialize inherited fields:

```
class Cat: Animal
  pub field indoor = false

  def (init) self name indoor
    Animal.(init) $self $name cat
    self.indoor = indoor
```

## Type Checking

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

See [Basic Types](basic-types.md#type-checking) for more on `type`.

## Special Methods

Special methods integrate class instances with language features. They are
defined with the method name in parentheses.

### Quick Reference

| Method     | Trigger                                    | Description                                |
| ---------- | ------------------------------------------ | ------------------------------------------ |
| `(init)`   | `MyClass args...`                          | Constructor                                |
| `(call)`   | `instance args...`                         | Call instance as function                  |
| `(bool)`   | `if instance`, `!instance`                 | Boolean conversion                         |
| `(str)`    | `"$instance"`, `str(instance)`             | String conversion; fallback for `(arg)`    |
| `(dbg)`    | `dbg` function                             | Debug string; fallback for `(str)`         |
| `(arg)`    | `std.arg` function, external program spawn | Argument string                            |
| `(unpack)` | `let :x :y = instance`                     | Destructuring                              |
| `(index)`  | `instance[key]`                            | Index                                      |
| `(assign)` | `instance[key] = val`                      | Index assign                               |
| `(get)`    | `instance.missing_field`                   | Dynamic missing-field fallback             |
| `(set)`    | `instance.missing_field = val`             | Dynamic missing-field assignment fallback  |
| `(hash)`   | `std.hash(instance)`, dict key             | Hash code (must be consistent with `(eq)`) |
| `(iter)`   | `for x = instance`, `[...instance]`        | Input iteration                            |
| `(next)`   | iteration protocol                         | Advance input iterator                     |
| `(sink)`   | `redirect output: $instance`               | Output iteration                           |
| `(put)`    | output protocol                            | Advance output iterator                    |

`(get)` and `(set)` only run when ordinary field lookup misses. They are
separate from descriptor-backed fields such as
[`getter`](../api/std/getter.md) and [`setter`](../api/std/setter.md).

**Operators:**

| Method    | Operator                             | Notes                                                |
| --------- | ------------------------------------ | ---------------------------------------------------- |
| `(neg)`   | `-x`                                 | Unary negation                                       |
| `(bnot)`  | `~x`                                 | Bitwise NOT                                          |
| `(add)`   | `x + y`                              |                                                      |
| `(sub)`   | `x - y`                              | `self` is left operand                               |
| `(rsub)`  | `y - x`                              | `self` is right operand (left doesn't handle it)     |
| `(mul)`   | `x * y`                              |                                                      |
| `(div)`   | `x / y`                              | `self` is left operand                               |
| `(rdiv)`  | `y / x`                              | `self` is right operand                              |
| `(ediv)`  | `x // y`                             | Euclidean division; `self` is left                   |
| `(rediv)` | `y // x`                             | Euclidean division; `self` is right                  |
| `(mod)`   | `x % y`                              | `self` is left operand                               |
| `(rmod)`  | `y % x`                              | `self` is right operand                              |
| `(band)`  | `x & y`                              | Bitwise AND                                          |
| `(bor)`   | `x \| y`                             | Bitwise OR                                           |
| `(bxor)`  | `x ^ y`                              | Bitwise XOR                                          |
| `(eq)`    | `x == y`, `x != y`                   | `!=` is the logical inverse                          |
| `(lt)`    | `x < y`, `x <= y`, `x > y`, `x >= y` | All four comparisons derived from `(lt)` and `(eq)`  |
| `(hash)`  | `std.hash(x)`, dict key              | Must return an `int`; must be consistent with `(eq)` |

### `(init)` --- Constructor

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

### `(call)` --- Function Call

Makes an instance callable like a function:

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

### `(unpack)` --- Destructuring

Return a more primitive type (such as a `dict`) for Do to destructure in lieu
of `self`:

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

### `(iter)` --- Iteration

Makes an instance usable as an iterator source for `for` loops, spread syntax,
and so forth. Should return an object supporting the iteration protocol: either
a built-in type, or a class instance that implements `(next)`:

```
class NumberRange
  field start = 0
  field stop = 0

  def (init) self start stop
    self.start = start
    self.stop = stop

  def (iter) self
    (range start: self.start end: self.stop).iter()

let r = NumberRange 0 5
assert_eq [...r] [0, 1, 2, 3, 4]
```

### `(next)` --- Iterator Protocol

Defines a class as a stateful iterator. Return the next value, or throw
`IterStop` when exhausted:

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

### `(sink)` --- Sink Protocol

Makes an instance usable as a sink target with `strand.put` or
`strand.redirect`:

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

### `(put)` --- Sink Write Protocol

Receives values from `put` when the instance is used as a sink:

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

### `(bool)` --- Boolean Conversion

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

### `(hash)` --- Hash Code

Called by `std.hash` and when an instance is used as a dictionary key. Must
return an `int`. If not defined, the hash is derived from the instance's
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

### `(str)` --- String Conversion

Called when an instance is converted to a string via `str()` or used in string
interpolation. Must return a `str`. Falls back to `(dbg)` if not defined:

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

### `(dbg)` --- Debug String

Called for debug/inspect output and as a fallback when `(str)` is not defined.
Must return a `str`. If neither `(str)` nor `(dbg)` is defined, the instance
displays as `<object>`:

```
class Node
  pub field val = 0

  def (init) self val
    self.val = val

  def (dbg) self
    "Node($(self.val))"
```

### `(arg)` --- External Command Argument

Called when an instance is interpolated into an external command as an argument
(e.g. `echo $obj` in a shell context). Must return a `str`. Falls back to
`(str)` if not defined, which in turn falls back to `(dbg)`:

```
class Path
  pub field parts

  def (init) self ...parts
    self.parts = parts

  def (arg) self
    self.parts.join("/")

  def (str) self
    "Path($(self.parts.join("/")))"
```

### `(index)` and `(assign)` --- Subscript Access

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

### Operator Overloading

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

### `(get)` and `(set)` --- Dynamic Field Fallback

Called when a field or method is accessed on an instance and no matching `pub`
field or method exists in the class prototype. Receives `self` and the field
name as a symbol:

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
```

`pub` fields still take priority and are never routed through `(get)` or
`(set)`:

```
class WithPub
  pub field x = 0

  def (init) self v
    self.x = v

  def (get) _self _key
    "fallback"

let w = WithPub 10
# static pub field, not routed through (get)
assert_eq $w.x 10
assert_eq $w.missing "fallback"
```

`(get)` also serves as a fallback for method dispatch. When `obj.method(args)`
is called and `method` is not a statically defined method, the runtime calls
`(get)` to retrieve a callable and then invokes it directly with the provided
arguments. Any `self`-binding must be handled by `(get)` itself:

```
let dp = Dynamic()
dp.greet = do |name| "hello $name"
assert_eq $dp.greet("world") "hello world"
```
