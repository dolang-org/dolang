# AGENTS.md - Development Guidelines for the Do Language Project

This file contains essential information for AI coding agents working on the Do
programming language project.

## Project Overview

Do is a Rust-implemented scripting language for DevOps automation. Source files
use the `.dol` extension. The implementation is organized as a Cargo workspace
of multiple crates.

### Workspace Structure

Core crates:

- **dolang**: Public API facade for embedding Do in Rust applications
- **dolang-bytecode**: Bytecode format, instruction set, and verification
- **dolang-compile**: Lexer, parser, name resolution, and bytecode emitter
- **dolang-runtime**: VM, garbage collector, strand concurrency, standard
  library
- **dolang-private-util**: Shared utilities (string interning, arena allocator,
  etc.)

Tooling: **dolang-shell** (CLI/REPL), **dolang-lsp** (LSP server)

Internal support crates: **dolang-private-build**, **dolang-private-test**

Extensions: `dolang-ext-*` crates, registered via the `extension!` macro.

Tests: `dolang-private-regression/tests/` (core language), `dolang-ext-*/tests/`
(per-extension tests), `tests/` (full integration tests).

Crates may have `ARCHITECTURE.md` files with detailed design notes.

## Do Language Syntax Summary

Full reference: [docs](./docs). This section is a quick-reference to help avoid
common syntax errors.

### Syntactic Levels

Do has three parsing contexts. Understanding which one you are in determines
whether `$` is needed or forbidden, whether operators are active, and whether
indented blocks are allowed.

#### 1. Statement Level (Shell-Like) — the default

At the top level and within indented blocks, tokens are **literal strings** by
default. Whitespace separates arguments. Operators like `+`, `/`, `=` are
literal characters, not operations.

```
echo hello world          # two literal string args: "hello", "world"
echo 1+1                  # one literal string arg: "1+1" (NOT 2)
echo https://example.com  # literal string, punctuation is literal
```

Use `$` to introduce variable references and compact expressions:

```
let name = Alice
echo hello $name          # "hello" "Alice"
echo $person.name         # field access
echo $arr[0]              # indexing
echo $func(x, y)          # C-style call
echo $!flag               # boolean not
```

Certain argument forms are automatically expressions without `$`:

- Parenthesized: `echo (1 + 1)` → `2`
- Data literals: `func [1, 2]`, `func {a: 1}`, `func (1, 2)`, `func (a: 1)`
- Quoted strings: `func "hello $name"`
- Constants: `func 42`, `func true`, `func nil`, `func :symbol:`
- `do` blocks: `func do |x| echo $x`

**Indented blocks are only valid at statement level.** The body of `if`,
`while`, `for`, `def`, `class`, `try`, etc. is always an indented block under
the keyword, never an expression inside `()`.

#### 2. Full Expression Level (C-Like) — inside `()`, `[]`, `{}`

Within parentheses, brackets, and braces, parsing switches to C-like: whitespace
is insignificant, operators work normally, `$` is **not used** (and using it is
a syntax error or means something different).

```
let x = (1 + 2 * 3)           # 7
let arr = [1, 2, 3]
let d = {name: "Alice", age: 30}
let t = (1, "two", ...rest)   # tuple; (x,) is a singleton, () is empty
let r = (name: "Alice", 30)   # record: has a static key (`k: v` or `:k`)
let v = (
  some_long_expr(x, y) +
  another * factor
)
```

**No indented blocks inside `()`.** You cannot write `if`/`for`/`while` with
bodies inside parentheses. Use `do` for inline lambdas, and `&&`/`||` for
conditional expressions:

```
# WRONG — indented block inside ()
# let x = (if condition
#   value)

# RIGHT — use && / || for ternary-style
let label = (condition && "yes" || "no")

# RIGHT — if/else as RHS at statement level (block form)
let x = if condition
  value_a
else
  value_b

# RIGHT — do lambda in expression context
let double = (do |x| x * 2)
assert_eq (double 5) 10
```

Function calls in expression context use either juxtaposition or C-style:

```
let r = (add 1 2)          # juxtaposition
let r = (add(1, 2))        # C-style
```

Whitespace before `(` only matters at statement level: `let t = f (1, 2)` passes
one tuple and `let t = f(1, 2)` passes two arguments, but inside `()` both
`(f (1, 2))` and `(f(1, 2))` are C-style calls. Pass a lone tuple there as
`(f((1, 2)))`.

#### 3. Compact Expression Level — after `$` or in implicit positions

A `$` at statement level starts a compact expression: variable access, field
access, indexing, C-style calls, and chaining. It does **not** support binary
operators or indented blocks.

Several positions are **implicitly** compact expressions (no `$` needed, and
using `$` is an error):

- **Receiver of a call** at statement level: `echo foo`
- **RHS of `let`/assignment**: `let x = foo.bar`
- **Condition of `if`/`while`**: `if flag`, `while running`
- **Iteratee of `for`**: `for x = items`
- **Scrutinee of `bind`**: `bind args`
- **Value of `return`/`throw`**: `return result`

These positions accept a compact expression followed optionally by command
arguments, so a command call works: `if func "arg"`, `let x = func 1 2`.

### Common Pitfalls

#### Unnecessary `$` in expression/implicit contexts

```
# WRONG -- $ not needed in "argument 0"
$echo foo

# Right
echo foo

# WRONG — $ not needed in let RHS (implicit compact expression)
let x = $foo

# RIGHT
let x = foo

# WRONG — $ not needed in if condition
if $flag
  echo yes

# RIGHT
if flag
  echo yes

# WRONG — $ not needed inside ()
let y = ($x + $z)

# RIGHT
let y = (x + z)
```

#### Missing `$` at statement level

```
# WRONG — name is literal string "name", not the variable
echo name

# RIGHT — $ introduces the variable
echo $name

# WRONG — this prints the literal "items.len"
echo items.len

# RIGHT
echo $items.len
```

#### `$name` Inside a String Literal Does Not Chain

Inside a `"..."` string, `$name` interpolates **only the bare identifier**.
It does not extend to field access, indexing, or calls the way `$` does in
argument/implicit-expression position. Anything beyond a bare name needs the
parenthesized `$(...)` form:

```
# WRONG — interpolates $server, then appends the literal text ".uri/hello"
echo "$server.uri/hello"

# RIGHT — $(...) evaluates the full expression
echo "$(server.uri)/hello"
```

This compiles fine either way; the bug only shows up at runtime. It is
deliberate, not an inconsistency to "fix": `"$basename.txt"` must interpolate
`$basename` and append `.txt`.

#### Vertical/Keyword-Argument *Values* Are Still Statement Level

A bareword on the **value** side of a vertical key item, dash item, or
keyword argument is a literal string, not a variable reference — the same
"statement level is shell-like by default" rule from the top of this section
applies there too, and it's easy to forget once you're several nested calls
deep:

```
let name = "Alice"

# WRONG — pushes the literal string "name", not the value of $name
parts.push
  - name

# WRONG — dict value is the literal string "name", not $name's value
let d = $
  key: name

# RIGHT — $ introduces the variable in a value position same as anywhere else
parts.push
  - $name

let d = $
  key: $name
```

No compile error results; it surfaces as a confusing assertion failure.

#### Expression-Level `if` (Does Not Exist)

There is no expression-level `if`/`else`. `if` is always a statement with an
indented block body. It can be on the RHS of `let`/assignment, but the branches
are still indented blocks:

```
# WRONG — no expression-level if
# let x = (if a > b then a else b)

# RIGHT — if/else as statement, result captured by let
let x = if (a > b)
  a
else
  b

# RIGHT — && / || as ad-hoc ternary in expression context
let x = (a > b && a || b)
```

#### Indented Blocks Inside `()`

Indented blocks (the bodies of `if`, `for`, `while`, `def`, multi-line `do`)
are a statement-level construct. They cannot appear inside `()`, `[]`, or `{}`.

```
# WRONG — block inside parentheses
# let result = (for x = items
#   x * 2)

# RIGHT — for at statement level with result
let result = $
  for x = items
    - (x * 2)

# RIGHT — do lambda in expression context (single expression body)
let doubled = (iter(items).map(do |x| x * 2))
```

`do` in expression context creates a lambda with an **expression** body (like
Python's `lambda`), not a block. For multi-statement blocks, use `do` at
statement level:

```
# Expression context: single-expression lambda
let f = (do |x| x * 2)

# Statement context: multi-statement block
let f = do |x|
  let y = (x * 2)
  echo "doubled: $y"
  y
```

### Variables and Assignment

```
let x = 42                # declare and bind
x = (x + 1)              # reassign (x must already exist)
let a b = [1, 2]         # destructuring
let :name :age = record   # keyword destructuring
```

### Functions

```
def greet name
  echo "Hello, $name!"

def add a b
  (a + b)                 # implicit return (last expression)

pub def exported x        # public (module export)
  (x + 1)

def connect :host = "localhost" :port = 8080
  echo "Connecting to $host:$port"

def log level ...rest     # variadic: rest is a Record
  echo "[$level]" ...rest

def run cmd *args **opts  # positional (Tuple) and key (Record) rests
  echo $cmd ...args ...opts
```

Vertical parameter layout with `do` to introduce the body:

```
pub def build
  :from
  :pull = true
  :tag
  ...args
do
  echo "Building $tag from $from"
```

### `do` Blocks (Anonymous Functions)

```
# Statement context: one-liner
let f = do echo hello

# Statement context: with params
let f = do |x| echo (x * 2)

# Statement context: multi-line block
let f = do |x|
  let y = (x * 2)
  y

# Expression context: lambda (expression body, no indented block)
assert_eq ((do |x| x * 2) 5) 10
let evens = (iter(items).filter(do |x| x % 2 == 0))
```

### Control Flow

`if`, `while`, `for` always use indented block bodies:

```
if (score >= 70)
  echo pass
else if (score >= 80)
  echo good
else
  echo fail

while (count < 5)
  echo $count
  count = (count + 1)

for item = [1, 2, 3]
  echo $item

for k v = {name: "Alice", age: 30}
  echo "$k: $v"
```

`if` as RHS of `let`/assignment:

```
let max = if (a > b)
  a
else
  b
```

`match` runs the first arm whose pattern (and optional `if` guard) matches. A
bare name matches anything, `else` lines up with `match`, and no match without
`else` gives `nil`. Unpacking a value that isn't a sequence or keyed value
raises rather than trying the next arm, so type-test first:

```
let label = match value
  Int(n) if (n < 0)
    "negative"
  0 | 1 do "small"
  Array(first _) do
    echo $first
    "pair"
  other do "other: $other"
```

### Commands and Calls

```
echo hello world              # command: func + literal args
echo $name                    # command with variable substitution
echo (1 + 1)                  # command with expression arg
func [1, 2] {a: 3}            # data literal args
func ...args                  # spread iterable into call

foo                           # bare name → evaluates variable (NOT a call)
foo()                         # zero-arg call
foo 1 2                       # call with args

range start: 1 end: 10        # keyword arguments
let start = 1
range :start end: 10          # :key shorthand (passes start: $start)
```

### Implicit Concatenation

Adjacent tokens without whitespace at statement level are concatenated:

```
let name = "world"
echo hello-$name              # "hello-world"
echo $name=$name              # "world=world"
echo prefix$name              # "prefixworld"
```

### Strings

```
"Hello, $name!"              # interpolation with $
"Result: $(1 + 2)"           # expression interpolation with $()
r"no\escapes\or$interp"      # raw string
r#"can contain "quotes""#    # raw string with # delimiters
b"\x01\x02\x03"              # binary string
```

Here strings (multi-line):

```
let doc = |
  Hello,
  world!
# doc == "Hello,\nworld!\n"

let stripped = |-
  hello
# stripped == "hello" (no trailing newline)

let raw = r|
  echo $HOME
# raw == "echo $HOME\n" (no interpolation)
```

### Vertical Layout

Indented blocks under commands become arguments; after `$` they
construct data:

```
# Vertical arguments
compile_sources
  - foo.c
  - bar.c

# Vertical data (array)
let items = $
  - 1
  - 2
  - 3

# Vertical data (dict — at least one key present)
let config = $
  host: localhost
  port: 8080

# Nested
let data = $
  name: Alice
  scores:
    - 95
    - 87

# for/if in vertical layout
let doubled = $
  for i = [1, 2, 3]
    - (i * 2)
```

Bare keys are symbols (`sym`) and must be valid identifiers. If you want a
string key, it must be quoted. An invalid bare key may parse as a string
literal instead:

```
# WRONG — `x-custom` isn't an identifier, so this doesn't parse as a key item
let headers = $
  x-custom: value

# RIGHT
let headers = $
  "x-custom": value
```

### `$` as Low-Precedence Call

`$` as an operator is a right-associative, low-precedence function call:

```
echo $ type $ str $ range 10
# equivalent to: echo (type (str (range 10)))
```

### Classes

```
class Point
  pub field x y = 0

  def (init) self x y
    self.x = x
    self.y = y

  pub def magnitude self
    ((self.x * self.x + self.y * self.y) / 1.0)

class Dog: Animal              # inheritance
  pub field breed = nil

  def (init) self name breed
    Animal.(init) $self $name dog
    self.breed = breed

let p = Point 3 4
echo $p.x                     # field access
echo $p.magnitude()           # method call
```

An empty `:` introduces vertical supertypes. Mix bin-packed lines and single
supertype dash lines; a dedented `do` ends the list. Protocols use the same
layout, with implicitly type-only supertypes:

```
class Derived[T]:
  Base[T] @Readable[T]
  - @Writable[T]
do
  pub field value @ T = nil
```

Private fields/methods use `.#`:

```
class Counter
  field count = 0             # private field

  pub def increment self
    self.#count = (self.#count + 1)

  pub def value self
    self.#count
```

### Modules

```
import math                   # whole module
import math: m                # alias
import math:                  # specific items names
  - add
  - subtract
```

### Error Handling

```
try
  risky_operation()
catch error.Type: err
  echo "Type error: $err"
catch err
  echo "Other: $err"
finally
  cleanup()

let result = try
  parse input
catch _
  default_value
```

### Destructuring

```
let a b = [1, 2]
let first ...rest = [1, 2, 3, 4]
let first *pos **keyed = {1, 2, color: "red"}  # pos == (2,), keyed has color
let :name :age = {name: "Alice", age: 30}

bind args
  - x
  - y = 0
  :verbose = false
```

Standalone `let` and `bind` accept a diverging `else`. Matching follows
`if let` / `if bind`; a single name tests truthiness. Bindings remain in scope
afterward, but are not visible in `else`:

```
let k v = s.split "="
else throw std.ValueError "expected KEY=VALUE: $s"
echo "$k -> $v"

bind response
  :status
  :body = ""
else
  return nil
```

`else` starts a new line at the statement's indentation and takes an inline
statement or an indented block. Falling through raises `std.RuntimeError`;
the checker warns when it finds a path to that end. `else if` is not allowed.
For a `let` RHS beginning with `if` or `match`, `else` belongs to the RHS;
a subsequent `else` cannot attach to the `let`.

### Type Annotations

Annotations document types and have no runtime effect. Full syntax (schemas,
quantifiers, implicit `<T`/`>T` parameters, vertical schemas):
[`docs/language/type-syntax.md`](./docs/language/type-syntax.md).

`@` and a type follow a bound name and precede any default. Write a space on
each side of `@`, except where several bindings share a line: there write none,
so whitespace separates only bindings. `->` gives a return type after the
parameters (or after the `do` ending vertical parameters):

```
let count @ Int = 0
def connect :host@Str = "localhost" :port@(Int | nil) = nil
  echo $host
def add a@Int b@Int -> Int
  (a + b)
def build
  :tag @ Str
  ...args @ Str
do -> Array[Str]
  [tag, ...args]
let double = (do |x @ Int| -> Int x * 2)
```

An annotation or return type is a **compact type**: the first whitespace ends
it, even inside `()`. Parenthesize unions and function types: `@Str|Path` is not
a union, but `@(Str | Path)` is. Other forms: `@Dict[Str, Array[Int]]`,
`@{name: Str, ?port: Int}`, `@((Int, ?Int) -> Int)`, `@(:a: | :b:)`. `(Int,
Str)` is `Tuple[Int, Str]`, `[T]` is `Array[T]`, and a form with a keyed item
is a record, `(name: Str, Int)`.

`[]` directly after a `def` or `class` name declares type binders, with optional
bounds and defaults (`T @ Bound = Default`):

```
def first[T] items @ Array[T] -> T
  items[0]
class Registry[V]: Table[Sym, V]
pub @let Pair[T] = Tuple[T, T]
```

Type-only declarations: `@def name` declares a bodiless overload (never `pub`;
it is exported with the implementation). `@class Name` declares a protocol.
`@` before a supertype makes it type-only. `@let Name = Type` is a type-only
alias. `@` before an import item, or `@import`, binds it for types only.

```
@def double x@Int -> Int
@def double x@Str -> Str
pub def double x
  (x + x)
pub @class Shape
  pub def area self -> Int
class Square: @Shape
  pub def area _self
    1
```

A cast takes the whole contents of its parentheses: `(e @ T)` checks `e`
against `T`; `(e !@ T)` asserts `T` unchecked. Neither has a runtime effect. As
a call argument, a cast needs its own parentheses: `f((x @ T))`.

```
let names = ([] @ Array[Str])
let dir = (path.parent !@ Path)
```

### Concurrency

```
import strand:
  - spawn
  - fork

let s = spawn do
  expensive_work()
let result = s.join()

let results = fork
  do task_a()
  do task_b()
```

## Build and Development

Prefer `dodo` for routine build and test tasks. If the `dodo` alias or
symlink is not available, run `dolang -m dodo` instead. The task definitions
and routing logic live in [`dodo.dol`](./dodo.dol) at the workspace root.
**DO NOT** use `cargo` directly unless there is no way to avoid it; running
`cargo` directly will often not set important environment variables.

```bash
dodo build                 # debug build
dodo fmt                   # format Rust
dodo lint                  # check for clippy and formatting warnings
dodo cargo-test            # (cargo test, use `--` to pass arbitrary additional arguments to cargo)
dodo shell-test            # (shell integration tests, use `--` to specify alternate arguments to `dolang -m test`)
dodo lsp-test              # dolang-lsp tests against the bundled typelibs (not in cargo-test)
dodo test                  # cargo, LSP and shell tests
dodo mkdocs                # build language docs (MkDocs site in site/)
dodo fmt-docs              # Format Markdown with rumdl
dodo lint-docs             # check for Markdown errors with rumdl
```

After changes: `dodo fmt` → `dodo lint` (address warnings, consider if they
indicate logic bugs).

- **Rust edition**: 2024, **MSRV**: 1.92.0+
- **Lifetimes**: prefer anonymous lifetimes (`&Foo`, `Bar<'_>`) unless a
  lifetime *must* be repeated or referencecd (e.g. it appears in both argument
  and return position, it comes from an ambient trait/impl bound/binder, or two
  parameters must agree because data transfers between them). Overconstrained
  lifetimes cause cascading borrow-checker problems. In particular, invariant
  brand lifetimes like `'v` and `'s` in this codebase (see
  `dolang-runtime/ARCHITECTURE.md`) will cause hard errors if unnecessarily
  repeated — e.g. `fn foo(s: &'s Strand<'v, 's>)` is wrong because `'s` is
  invariant in `Strand`; use `fn foo(s: &Strand<'v, 's>)`.

## Documentation and Extensions

Before writing or editing anything under `docs/`, or doc comments in Do sources
and `stub/` files, read [`agents/DOC_STYLE.md`](./agents/DOC_STYLE.md).

Before writing or changing a `dolang-ext-*` crate, or other Rust code that
registers native types or modules, read
[`agents/EXTENSIONS.md`](./agents/EXTENSIONS.md).
