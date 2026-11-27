# Functions

## Function Types

`->` separates a function's parameters from its return type. Parameters are
written in `()`, with `?` before an item marking it optional. Parentheses can
be omitted for a single parameter. `->` groups to the right:

```
let add @ ((Int, ?Int) -> Int) = do |a b = 0| (a + b)
let address @ ((Str, ?port: Int) -> Str) = do |host :port = 80| "$host:$port"
let curried @ (Int -> Int -> Int) = do |a| do |b| (a + b)
let thunk @ (() -> Str) = do "hello"
```

A parameter list is a [schema](./schemas.md), so its items take the same
elements and quantifiers, and a repeating one may appear anywhere and more than
once. A parameter's key must be a name, so a key given as a type is not allowed;
write `**V` for a list that takes any key.

```
let log @ ((Sym, *Str, **Str) -> nil) = do |level *parts **opts|
  echo "[$level]" ...parts ...opts
let shout @ ((...) -> nil) = do |...args| nil
```

A `def`'s own signature is written with [annotations](./annotations.md) on its
parameters and a [return type](./annotations.md#return-types).

## Implicit Parameters

Every function also takes the strand's ambient input and output. `<T` gives the
required input type `>T` the required output type. Both are items of the
parameter list, so they may appear in any order among the others, but each at
most once:

```
def collect count@Int <Iter[Int] >Sink[Str] -> nil
  for value
    put $ str(value)
let runner @ ((Int, <Iter[Int], >Sink[Str]) -> Int) = (do |n| n)
```

A `def` or method that omits one is implicitly generic over it, so it may be
called with any input or output respectively. The body can't rely on an
unspecified input or output being anything in particular: in a
[strict](./strict.md) file it is effectively `Value`; in a gradually-checked
file its use is unchecked.

A function type in a signature that omits either inherits the `def`'s:

```
def with[R] block@(() -> R) -> R
  # block may be called here because it shares `with`'s input and output
  block()
```

A function type in a `do` block's parameters, return type or implicit
parameters takes the block's channels in the same way. The block isn't generic
over a channel it omits. That channel is the one each use gives it: the call
the block is passed to, or the function type expected of it.

```
def apply f @ (((Int) -> nil) -> nil) <Iter[Int] >Sink[Str]
  f (do |n| put $ str(n))

def run <Iter[Int] >Sink[Str]
  # cb has the block's channels, which apply gives as its own
  apply do |cb @ ((Int) -> nil)| cb 1
```

Anywhere else, an omitted implicit parameter defaults to `Value`, which
effectively disallows strand input/output in strict units. For callback
functions which will be run later in an arbitrary context, explicitly specify
`<Value, >Value`.

```
def on_click block @ ((<Value, >Value) -> nil)
  handlers.push $block
```

An input or output may be any type, but inputs are conventionally `Iter` and
outputs `Sink`. A `for` without an iteratee reads the input through its
`Iter.(next)`, and [`strand.put`](strand.put) requires a `Sink` output.

### Top-Level Code

A unit's top level has an input and output too, decided by how it is compiled.
A module's top level has neither, as `Value`: any strand may import it, so it
can't read or write without a downcast. Its `def`s declare their own as usual.

A script's top level, and a REPL line's, has `strand.ScriptInput` and
`strand.ScriptOutput`, which stand for whatever the embedding runs scripts
with. In the shell they are [`shell.Input`](shell.Input) and
[`shell.Output`](shell.Output): a script reads strings and writes any value,
and each side gives its bytes through `io()`. With no embedding to say, they
are unchecked.

[`load.run`](load.run) and [`exec.run`](exec.run) run a script with their
caller's input and output, so they require the script's. Inside
[`strand.with_io`](strand.with_io), [`std.null`](std.null) serves as both:

```
def run_quietly code @ Bin -> Value
  strand.with_io input: $std.null output: $std.null do load.run $code
```

[`strand.redirect`](strand.redirect) gives its block an `Iter` and a `Sink`,
which aren't a script's.

## Generic Function Types

`@[]` before a function type declares binders for it, written as a `def`'s
[binders](./generics.md) are. A value of the type is a generic function: each
call chooses its binders, as a call of a generic `def` does.

```
def id[T] x @ T -> T
  x
def both f @ (@[T] (T) -> T) -> (Int, Str)
  (f(1), f("a"))
both $id
```

An omitted input or output is an implicit generic binder, as it is of a `def`.

A generic function type may be the type of a parameter, a variable, a field or
an alias, a function's parameter or result, or a union member. It cannot be a
type argument, which includes the element of an array or the item of a tuple,
`Pack` or dictionary, whether written there or through an alias:

```
@let Id = (@[T] (T) -> T)
def apply f@Id x@Int -> Int
  f $x
let fs @ Array[Id] = []    # error
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

An overload doesn't take `pub`; it's exported if its implementation is. A
method, including a special method such as `(init)`, takes overloads in its
class body the same way, and so do protocol methods. An overload can't be
decorated.

A method's overload may annotate its receiver with a narrower type, so that it
applies only to those instances. The class's binders in the overload then stand
for the receiver's type arguments, so `shout` below returns `Str`.

```
class Box[T]
  pub field item @ T

  @def shout self @ Box[Str] -> T
  pub def shout self
    (self.item !@ Str).upper()
```

An implementation belongs to every instance, so its receiver annotation can't
narrow the class. It may be the class or a supertype of it, and the body sees
only that type. Called through the class, as in `Square.describe $shape`, the
method then takes any value of that type:

```
@class Shape
  pub def area self -> Float

class Square: @Shape
  pub field side @ Float = 1.0

  pub def area self -> Float
    (self.side * self.side)

  pub def describe self @ Shape -> Str
    "area $(self.area())"
```

The annotation may use the method's own binders, which each call solves from
its receiver. The check needs only some choice of them that every instance
fits. A method that returns a new array of the items can take their type this
way, so that its result doesn't make the class's binder invariant (see
[Generic Variance](./checking.md#generic-variance)):

```
@class Readable[T]
  pub def read self -> T

class Reader[T]: @Readable[T]
  pub def read _self -> T
    ...

  pub def batch[U] self @ Readable[U] -> Array[U]
    [self.read(), self.read()]
```

An overload's receiver must then be below the implementation's rather than
the class, as `self @ Readable[Str]` is. The class's binders in the overload
stand for what that receiver gives them.

Overloads have relaxed parameter shape requirements: rest parameters may appear
anywhere and more than once, and a required parameter may follow an optional
one.

```
@def pipeline[*Rs, R] *stages@...(() -> Rs) last@(() -> R) -> R
```

At a call, the checker uses an overload if exactly one is compatible with the
arguments; the implementation is not considered. Failure to select an overload
is an error.

A method that overrides another or meets a protocol's member is compared by the
signatures it's called with in the same way: each of the required method's
overloads, or its signature if it has none, must be met by one of the method's
own overloads, or its signature if it has none. A required signature that only
several overloads meet together, such as `(Int | Str) -> (Int | Str)` by
`(Int) -> Int` and `(Str) -> Str`, isn't met.

Overloads do not dispatch at runtime or have any runtime effect; every call
runs the same implementation. The checker checks the implementation's body
under its own signature, then again under each overload, with the parameters
bound as a call of that overload binds them:

- A parameter that every call of the overload passes has the overload's type,
  and its default isn't used.
- A parameter that no call of the overload passes has only its default's type.
- The body's result must fit the overload's return type, and its input and
  output are the overload's.

What only an overload's check finds is reported pointing at the overload.
An overload that admits a call the implementation's parameters can't take, by
omitting a required parameter or passing one it doesn't have, is an error.
Functions nested in the body are checked under each overload too.

Under an overload, a branch that narrowing shows no call of it reaches isn't
checked. Only a type test narrows away a class (see
[Truthiness and Equality](./checking.md#truthiness-and-equality)), so test for
an absent parameter's `nil` default with `type(x, std.Nil)`, not `x == nil`:

```
@def twice -> ((Int) -> Int)
@def twice x @ Int -> Int
pub def twice x @ Int = nil -> Value
  if type(x, std.Nil)
    return do |y @ Int| -> Int (y * 2)
  (x * 2)
```

Under the first overload, `x` is `nil`, and only the first branch is checked;
under the second, `x` is an `Int`, and only the second is. With `x == nil`, the
second overload's check would reach the first branch and report its result.

Where the checker can't follow the body under an overload, a cast (`!@`) asserts
what it can't verify. A stub's `...` body isn't checked, so a stub's overloads
remain trusted assertions about its native implementation.
