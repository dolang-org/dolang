# Annotations

## Annotation

`@` after a binding (parameter, let, etc.) and before any default value
annotates that binding. Whitespace is optional on either side of the `@`. By
convention, spaces are omitted where several annotations are introduced on the
same line, and included otherwise.

```
let count @ Int = 0
let :name@Str :age@Int = person

for key@Sym value@Int = scores
  echo "$key: $value"

def connect :host@Str = "localhost" :port@Int = 8080
  echo "Connecting to $host:$port"
```

A class's fields are annotated the same way; see
[Fields](./classes.md#fields).

### Rest Bindings

The annotation on a rest parameter gives the type of each item it collects. A
[schema](./schemas.md) instead describes all the items together:

```
def log level@Sym ...parts@Str
  echo "[$level]" ...parts

def tag name@Str *children@Str **attrs@Str
  [name, children, attrs]

def configure ...options@{name: Str, ?port: Int}
  apply ...options
```

A leading `...` after a rest binding's `@` maps a type over a
[schema binder](./generics.md#schema-binders), giving each collected argument
its own type. Here each block returns the corresponding item of `Rs`:

```
def fork[*Rs] *blocks @ ...(() -> Rs) -> Tuple[...Rs]
  ...
```

The mapping marker is accepted only on rest bindings.

A `*` rest collects only positional arguments and a `**` rest only key
arguments, so a schema annotating one, or each schema binder its mapping names,
must be bounded to that lane: `{*Value}` for `*`, `{*(Sym): Value}` for `**`.
Only a `...` rest takes both:

```
def options[S @ {*(Sym): Value}] **specs @ ...Option[S] -> Parsed[S]
  ...
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
treats a call to it as it does `throw`: nothing after the call runs, so it ends
the path for [initialization](./classes.md#object-initialization) and
narrowing. The checker warns about code after such a call, including a statement
that would use its result, as in `let x = fail "no"`.

## Casts

`(value @ Type)` checks `value` against `Type`, as an annotated binding would,
and has the type `Type`.

```
let names = ([] @ Array[Str])
let label = (do |item| item.name @ ((Item) -> Str))
let release = (pick current (:STABLE: @ Release))
```

`(value !@ Type)` is an unchecked cast: it has the type `Type` without
verifying that `value` fits it. Use it to assert properties the checker can't
prove, such as a field never being `nil` in context:

```
let dir = (path.parent !@ Path)
```

The checker warns about a `!@` cast whose value it can show fits the type, since
`@` would do. That includes a value whose type is unknown.

Casts, like all type checker annotations, have no effect at runtime.

A cast applies to the entire contents of its parentheses, so `(a + b @ Int)`
casts the sum. The type is compact, as in an annotation, so parenthesize a
union: `(value @ (Int | nil))`. A cast that is a call's argument needs
parentheses of its own: `f((value @ Int))`.
