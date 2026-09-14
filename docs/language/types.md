# Type Annotations

Type annotations record what a name is expected to hold. The compiler parses
them and reports malformed ones, but they have no effect at runtime: nothing is
checked or converted.

## Annotating Names

`@` followed by a type annotates the name before it. Whitespace separates the
name from the `@`, and the annotation comes before any default value:

```
let count @Int = 0
let :name @Str :age @Int = record

for key @Sym value @Int = scores
  echo "$key: $value"

def connect :host @Str = "localhost" :port @Int = 8080
  echo "Connecting to $host:$port"
```

Annotations work wherever a pattern or parameter list binds a name, including
vertical layout and `do` parameters:

```
bind args
  - path @Str
  :verbose @Bool = false

let double = do |x @Int| (x * 2)
```

The annotation on a rest parameter gives the type of each item it collects:

```
def log level @Sym ...parts @Str
  echo "[$level]" ...parts
```

A field declaration that names several fields gives them one annotation:

```
class Point
  pub field x y @Int = 0
```

## Return Types

`->` followed by whitespace and a type gives a function's return type. It comes
after the parameters, or after the `do` that ends vertical parameters:

```
def add a @Int b @Int -> Int
  (a + b)

def greeting() -> Str
  "hello"

def build
  :tag @Str
  ...args @Str
do -> Array[Str]
  [tag, ...args]
```

A `do` block's return type follows its parameters:

```
let double = do |x @Int| -> Int (x * 2)
let halve = (do |x @Int| -> Int x // 2)
```

## Type Syntax

An annotation or return type is a compact type, which whitespace ends. A type
that needs whitespace or operators must be parenthesized. This holds within
full expressions too: in `(do |x| -> Array[Int] [x])`, the space after
`Array[Int]` ends the type. Within a type's own `()`, `[]`, and `{}`,
whitespace is insignificant.

| Syntax                                | Meaning                   |
| ------------------------------------- | ------------------------- |
| `Str`, `time.Duration`                | Named type                |
| `:sym:`, `"str"`, `42`, `true`, `nil` | Constant                  |
| `Array[Int]`                          | Type arguments            |
| `(Str \| Path)`                       | Union                     |
| `{name: Str, ?port: Int}`             | Dict schema               |
| `(Int, ?Int) -> Int`                  | Function                  |

### Names

A name is an identifier, or a module name followed by `.`-separated names, such
as `time.Duration`.

### Constants

A constant type is a symbol, string, integer, boolean, or `nil`. A string
cannot contain interpolations.

```
let mode @(:TARGET: | :LINK:) = :TARGET:
```

### Type Arguments

`[]` directly after a type supplies its arguments. At statement level, a space
before the `[` ends the type instead.

The items of `[]`, `()`, and `{}` share one syntax: a type, `key: type`, or
`...type` for any number of further items.

```
let names @Array[Str] = []
let index @Dict[Str, Array[Int]] = {}
let row @Tuple[...Str] = []
```

### Unions

`|` separates the members of a union. A union may begin with `|`, which lets
a long one break across lines:

```
let target @(
  | Str
  | fs.Path
  | nil
) = nil
```

There is no shorthand for a type that also accepts `nil`; write `(T | nil)`.

### Dict Schemas

A schema lists the keys a dict has and the type of each value. Bare keys are
symbols and quoted keys are strings, as in dict literals. `?` before a key marks
it optional, and `...type` gives the type of any remaining entries:

```
let options @{name: Str, ?port: Int, ...Dict[Str, Value]} = {name: "db"}
```

### Functions

`->` separates a function's parameters from its return type. Parameters are
written as in `()`, with `?` before an item marking it optional. A single
parameter needs no parentheses, and `->` groups to the right:

```
let add @((Int, ?Int) -> Int) = nil
let connect @((Str, ?port: Int) -> Conn) = nil
let curried @(Int -> Int -> Int) = nil
let thunk @(() -> Str) = nil
```

A required positional parameter cannot follow an optional one, and a rest
parameter cannot be optional. Parentheses that `->` does not follow must hold
exactly one type.
