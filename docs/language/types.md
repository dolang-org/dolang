# Type Annotations

Type annotations record what a binding is expected to hold. The
[type checker](./type-checking.md) uses them to check calls, assignments,
returns, and class members. Annotations have no effect at runtime.
[Type Syntax](./type-syntax.md) describes the types they can write, and
[Type Declarations](./type-declarations.md) covers generic binders, aliases,
and other declarations that exist only for the checker.

## Annotation

`@` after a binding (parameter, let, etc.) and before any default value
annotates that binding. Whitespace is optional on either side of the `@`. By
convention, space is used on both sides except where several bindings are
introduced on the same line, in which case no space is used.

```
let count @ Int = 0
let :name@Str :age@Int = record

for key@Sym value@Int = scores
  echo "$key: $value"

def connect :host@Str = "localhost" :port@Int = 8080
  echo "Connecting to $host:$port"
```

### Rest Bindings

The annotation on a rest parameter gives the type of each item it collects. A
schema instead describes all the items together:

```
def log level@Sym ...parts@Str
  echo "[$level]" ...parts

def tag name@Str *children@Str **attrs@Str
  [name, children, attrs]

def configure ...options@{name: Str, ?port: Int}
  apply ...options
```

A rest binding's annotation may also
[map a type over a schema](./type-declarations.md#mapped-rests).

### Fields

A field declaration that names several fields gives them all its annotation,
just as it gives them all its default value:

```
class Point
  pub field x y @ Int = 0
```

## Return Types

`->` followed by whitespace and a type gives a function's return type. It comes
after parameters for single-line `def`s, or after the `do` for vertical
parameters:

```
def add a@Int b@Int -> Int
  (a + b)

def greeting() -> Str
  "hello"

def build
  :tag @ Str
  *args @ Str
do -> Array[Str]
  [tag, ...args]
```

A `do` block's return type follows its parameters:

```
let double = do |x @ Int| -> Int (x * 2)
let halve = (do |x @ Int| -> Int x // 2)
```

A function whose return type is [`Never`](std.Never) doesn't return. The checker
treats a call to it as it does `throw`: nothing after the call runs, so a field
assigned on every other path is initialized:

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

The checker warns about code after such a call, including a statement that
would use its result, as in `let x = fail "no"`.

## Casts

`(value @ Type)` checks `value` against `Type` as an annotated binding would,
and has the type `Type`. It gives a type where there is no binding to annotate:
a collection literal or `do` block, or an argument to a generic function.

```
let names = ([] @ Array[Str])
let label = (do |item| item.name @ ((Item) -> Str))
let release = (pick current (:STABLE: @ Release))
```

`(value !@ Type)` has the type `Type` without checking `value`. Use it for what
the checker can't show, such as a property that may be `nil` but isn't in
context:

```
let dir = (path.parent !@ Path)
```

The checker warns about a `!@` cast whose value it can show fits the type, since
`@` would do. That includes a value whose type is unknown.

Neither cast changes the value at runtime.

A cast takes the whole contents of its parentheses, so `(a + b @ Int)` casts
the sum. The type is compact, as in an annotation, so parenthesize a union:
`(value @ (Int | nil))`. A cast that is a call's argument needs parentheses of
its own: `f((value @ Int))`.
