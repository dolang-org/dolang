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

Extensions (`dolang-ext-*`): registered via the `extension!` macro, linked in
via `linkme`. Each crate name describes its domain (shell, http, json, sqlite,
regex, zip, compile, load, progress).

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

This compiles fine either way, so the bug only shows up at runtime (e.g. as
a wrong URL or a stray literal suffix) — there is no parse error to catch it.

This is a deliberate consequence of the shell-like design, not an
inconsistency to "fix": at statement level, whitespace separates tokens, so
`$foo.bar` unambiguously ends where the next space is. Inside a string,
whitespace is just another character — there's no delimiter to stop an
interpolation from *looking* like it should keep chaining. If `$name` chained
freely, `"$basename.txt"` would try to resolve a field/property named `txt`
on `$basename` instead of doing the obviously-intended thing (interpolate
`$basename`, then append the literal `.txt`). Restricting bare `$name` to
just the identifier keeps that common case unsurprising, at the cost of
requiring `$(...)` for anything more complex.

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

No compile error results — the literal string is often plausible-looking
(e.g. `"name"`), so this tends to surface as a confusing assertion failure
rather than an obvious syntax mistake.

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

### Type Annotations

Annotations document types and have no runtime effect. `@` and a type follow a
bound name and precede any default. Whitespace on either side of `@` is
optional. By convention, write a space on each side, except where several
bindings share a line: there write none, so whitespace separates only bindings.

```
let count @ Int = 0
def connect :host@Str = "localhost" :port@(Int | nil) = nil
  echo $host
class Point
  pub field x y@Int = 0
```

`->`, whitespace, and a type give a return type, after the parameters (or after
the `do` ending vertical parameters):

```
def add a@Int b@Int -> Int
  (a + b)
def build
  :tag @ Str
  ...args @ Str
do -> Array[Str]
  [tag, ...args]
let double = (do |x @ Int| -> Int x * 2)
```

`[]` directly after a `def` or `class` name declares binders (names standing for
types; also `:K`, and trailing `...R` or `*R`/`**R` as for rest parameters).
Binders may have type-expression bounds and defaults (`T @ Bound = Default`); a
schema bound such as `S @ {...}` marks a schema binder. Type arguments expand a
pack only with `...`, whatever its binder: `class Tuple[*Ts]` is used as
`Tuple[...Ts]`. A superclass may take type arguments:

```
def first[T] items @ Array[T] -> T
  items[0]
class Registry[V]: Table[Sym, V]
pub @let Pair[T] = Tuple[T, T]
```

`@def name` declares a bodiless overload of the function or method `name` in the
same block or class body; it is exported with its implementation, so it is
never `pub`. `@class Name` declares a protocol, whose members have no bodies.
`@` before a supertype makes it type-only, so the class does not inherit from it
at runtime:

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

`@` before an import item binds it for types only; the item is not imported:

```
import geometry:
  - @Point
  - @Vector: Offset
```

`@import` makes every module and item in the statement type-only, including
renamed imports. `import @geometry` similarly binds a whole module only for
dotted type names without loading it at runtime. `@let Name = Type` declares a
type-only alias visible throughout its block, so it may refer to itself or a
later alias.

An annotation or return type is a compact type, so the first whitespace after
it begins ends it, even inside `()`. Parenthesize unions and function types:
`@Str|Path` is not a union, but `@(Str | Path)` is. Other forms:
`@Dict[Str, Array[Int]]`, `@{name: Str, ?port: Int}`, `@((Int, ?Int) -> Int)`,
`@(:a: | :b:)`. Braces form schemas rather than types; use `Dict[{...}]` for a
dict with a schema. Within a schema, `...T` describes further items of any
kind, `*T` further positional items, `**T` further keyed items, and `...K: V`
arbitrary keyed entries; `{...}` is shorthand for `{...std.Value}`. Schemas are
closed unless they contain a rest item. Keyed and open rest items are not
allowed in function parameter lists: `((Int, *Str, **Bool) -> nil)`. A type
whose only parameter is a schema takes `Foo[T]` for `Foo[{*T}]` and
`Foo[K, V]` for `Foo[{...K: V}]`; prefer these to a schema with a single rest
item.

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
dodo test                  # cargo and shell tests
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

## Documentation Style (docs/)

When writing or editing documentation in `docs/` or in doc comments, follow
these guidelines to keep prose direct and technical. The recurring problems
these address were AI-generated verbosity — if a sentence reads like filler,
cut it.

### API Reference

The API reference under `docs/api/` is generated from doc comments in Do
sources. Never write or edit pages there; the directory is not checked in.
`dodo mkdocs` extracts each module with `dolang -m compile extract --doc`,
writes a page for the module and one for each documented public class, and
renders them with the mkdocstrings handler in `docs/mkdocstrings_handlers/do/`.
LSP hover text for imported and prelude names comes from the same extraction.

Documented modules live in:

- `dolang-shell-modules/lib/` — modules written in Do, documented where they are
  implemented.
- `dolang-runtime/stub/` and `dolang-ext-*/stub/` — stubs for native modules. A
  stub declares a module's public API, with `...` as each body, to carry
  its documentation and type annotations. Keep it in step with the native
  module.

A file `foo/bar.dol` documents module `foo.bar`. Only `pub` declarations are
extracted, and the pages leave out undocumented ones.

#### Doc Comments

A block of comment lines directly above a declaration documents it; a blank line
ends a block. A block on the first line of a file (after any `#!` line)
documents the module, so a blank line must separate it from the first
declaration's block.

The handler lays out the page: signature headings, parameter tables, and the
tables of a module's types and functions. A doc comment holds only prose, in
this order:

1. A brief first paragraph. A class's first paragraph is its summary in the
   module's table of types.
2. Further paragraphs, if needed.
3. Sections as `##` headings, omitting any that don't apply: `## Returns` (only
   when it says more than the return type), `## Errors`, `## Example`.

A short code fence may follow the opening paragraphs without a heading. Once a
section begins, a later code fence goes under its own `## Example` rather than
inheriting the preceding section. Don't use bold text as a substitute for a
section heading.

When more than one exception is worth documenting, use a table with
`Exception` and `Condition` columns so each exception's meaning is explicit.
Single exceptions may use concise prose. Do not document incidental exceptions
such as cancellation or interruption unless they are part of the API's
specific contract.

A parameter's doc comment goes on the parameter itself, so declare documented
parameters vertically. The first paragraph becomes the parameter's row in the
table; later paragraphs become a subsection of its own, which may use nested
headings. A declaration with a parameter table has its signature heading
abbreviated to its first few required positional parameters.

```
# Pulls an image.
pub def pull
  # Image name or ID.
  image @ Str
  # Registry to pull from; the configured default when omitted.
  :registry @ Str = nil
do -> Image
  ...
```

#### Types

Annotations supply the Type column, the return type after a signature heading,
and a field's type, with names linked to their documentation:

- Annotate what a caller may pass. A keyword to omit rather than pass `nil` is
  still `:limit @ Int = nil`; a parameter that accepts `nil` is
  `@ (Int | nil)`.
- A native property is a method marked `#[getter]`, with its type as the return
  type: `pub def len self -> Int`.
- Name a type from another module with a type-only import item (`- @Iter`) when
  the module is otherwise unused.
- Leave out an annotation on a parameter that accepts anything.

#### Declarations in Stubs

- Write a declaration without parameters as `def name()`; `def name` directly
  followed by its body is a syntax error.
- A constructor is the special method `(init)`. A method on the type object is
  marked `#[class]` and still declares `self`.
- Qualify a superclass from another module: `class Error: std.RuntimeError`. A
  class names one runtime supertype; name any other type it implements as a
  type-only supertype, as in `class Blake3: @State`.

#### Links

Link to another documented name by its identifier, as in
``[`Str`](std.Str)`` or ``[`Regex.match`](regex.Regex.match)``. A doc comment's
relative links resolve against the page that renders it, not the source file.
Link to a heading on the same page through the page itself
(`](./index.md#anchor)`), never a bare `](#anchor)`.

### Voice and Brevity

- **Lead with what it does, not what it is.** One-line descriptions should
  state the function/type's purpose directly, not narrate it.
    - Good: `Computes the BLAKE3 digest.`
    - Bad: `Computes the BLAKE3 digest of a string or binary value and returns
      the raw digest bytes.` (the signature already says what it takes and
      returns)
- **Don't restate what the annotations show.** Prose shouldn't repeat a
  parameter's or return value's type.
- **Don't list interface methods on every concrete type.** If `Blake3`
  implements `State`, say so once — don't re-list `update`, `digest`, etc.
  on the `Blake3` page. Link to `State` instead.
- **Use Do-native terminology.** Say "callable" or "block", not "thunk". Say
  "supertype", not "nominal base type".

### Code Examples

- **Use plain fences.** The MkDocs setup handles highlighting without a language
  tag. A fence tagged `playground` renders the same, plus a link that opens the
  example in the browser playground; use it for examples that run there.
- **Don't include `import` lines** in examples unless the example is
  specifically about importing. API doc examples should assume the module's
  exports are in scope. In a `playground` fence, write the imports the example
  needs on lines starting with `#>`, which the page hides but the playground
  runs.
- **Use Do idioms in examples.** Prefer `$x.method()` and vertical layout
  over wrapping everything in `(...)`. Break long lines with here strings
  or vertical argument lists, not by cramming into one line.
- **Module-qualify only when the reader might be confused.** Within a module's
  own doc page, use bare names: `Blake3()` not `digest.Blake3()`.

### Factual Accuracy

- **Don't invent behavior.** If you haven't verified how a function handles
  edge cases, don't document edge-case behavior. Missing docs can be added
  later; wrong docs cause bugs.
- **Don't document removed or renamed features.** If the code doesn't have
  a "current directory" module search, don't document it. Check the
  implementation when uncertain.
- **Avoid "notes" that merely re-explain the obvious.** E.g. if `[]` is a
  full expression context, you don't need a parenthetical reminding the
  reader that keys must be quoted strings.

## Writing Extensions

Extensions live in `dolang-ext-*` crates and are auto-registered at link time
via `linkme`. An extension implements the `Extension` trait and calls
`extension!` to register itself. A typical extension has these files:

- `extension.rs` — trait impl + `extension!` call
- `global.rs` — global state holding `Type` handles
- One or more implementation files — object types, module functions, etc.
- `stub/<module>.dol` — the module's public API and its documentation (see
  [API Reference](#api-reference))

### Extension Entry Point

```rust
use dolang::{compile::Compiler, extension, extension::{Extension, Version},
    runtime::vm::Builder};

pub struct MyExt;

impl Extension for MyExt {
    type Error = MyError; // or std::convert::Infallible if you can't fail
    const NAME: &str = "dolang-my-ext";
    const VERSION: Version = Version { major: 0, minor: 1, patch: 0 };
    const DESCRIPTION: &str = "My Extension";

    fn apply_compiler(&self, _compiler: &mut Compiler) -> Result<(), Self::Error> {
        Ok(()) // hook for registering syntax extensions; usually a no-op
    }

    fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
        let global = Global::new(builder);
        let global = builder.register_state(global);
        configure_vm(builder, global);
        Ok(())
    }
}

extension!(MyExt); // auto-registers via linkme distributed slice
```

### `Register` vs. `Builder`

`Builder` dereferences to `Register`, which carries the registration API:
`sym`, `register_state`, `register_type`, `build_type`, `module`, and
`module_object`. `TypeBuilder` dereferences to `Register` too. Setup helpers
should take `&mut Register<'v>`; only VM-wide configuration (strand-local keys,
importers, traps) needs `&mut Builder<'v>`. Functions exported for other crates
that only look up state should take `&Vm<'v>`, never a builder.

### Lazy Setup

`Builder::lazy` defers registration until something needs it. The setup runs at
most once: when Do code imports one of the modules it declares, or when Rust
code forces its tag through an `Alloc`. Declare it under the `Tag` of the state
it registers, so `force_state` can run it and return that state:

```rust
fn apply_vm<'v>(&self, builder: &mut Builder<'v>) -> Result<(), Self::Error> {
    builder.lazy::<global::Tag>(&["my_ext"], |reg| {
        let global = Global::new(reg);
        let global = reg.register_state(global);
        configure_vm(reg, global);
    });
    Ok(())
}
```

- The setup must register exactly the modules it declares; anything else panics.
- Strand-local keys, importers, and traps are `Builder`-only. Reserve them in
  `apply_vm` and move them into the setup.
- `Vm::state` never runs a setup, and panics on state whose setup hasn't run.
  Public functions for other crates that create objects call
  `strand.force_state::<Global>()` (`AllocExt` must be in scope). A function
  that only needs to force can take `&mut dyn Alloc<'v>`, so it works from a
  strand or during registration.
- Code that only recognizes existing objects can use `Vm::try_state`: if it
  returns `None`, the setup hasn't run, so no instance exists.

### Global State (`State<'v, T>`)

`Register::register_state` stores a value for the lifetime of the VM and returns
a `State<'v, T>` handle. `State` is `Copy` and dereferences to `&T`. Use it to
hold `Type` handles and other VM-lifetime data that methods need.

```rust
use dolang::runtime::{Type, vm::{Register, Stateful}};

pub(crate) struct Global<'v> {
    pub(crate) types: Types<'v>,
}

pub(crate) struct Types<'v> {
    pub(crate) widget: Type<'v, Widget>,
    pub(crate) widget_iter: Type<'v, WidgetIter>,
}

pub struct Tag;
impl<'v> Stateful<'v> for Global<'v> {
    type Tag = Tag; // unique tag prevents collisions
}

impl<'v> Global<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self { types: Types {
            widget: builder.register_type(),
            widget_iter: builder.register_type(),
        }}
    }
}
```

### Modules

`Register::module` creates a native module with exported values and functions.

```rust
pub fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    builder
        .module("my_ext")
        .value("Widget", global.types.widget) // export type object
        .function("helper", async move |strand, args, out| {
            let ([arg], []) = unpack!(strand, args, 1, 0)?;
            // ...
            Ok(())
        })
        .commit();
}
```

### The `Object<'v>` Trait

Native object types implement `Object<'v>`. Key associated items:

| Item              | Purpose                                                 |
| ----------------- | ------------------------------------------------------- |
| `NAME` / `MODULE` | Display name and module path (for error messages)       |
| `SLOTS`           | Number of GC-visible slots (default 0). See below.      |
| `type Annex`      | **Immutable** per-instance data; no borrow check needed |
| `type Type`       | **Mutable** data on the type singleton (usually `()`)   |
| `type TypeAnnex`  | **Immutable** data on the type singleton (usually `()`) |

`Annex` is stored alongside the GC object and accessible without a runtime
borrow check via `Instance::annex()`. Use it for data that never changes after
construction (e.g. a `State` handle, a compiled regex). Mutable per-instance
data goes in the struct itself, accessed through `Instance::borrow_mut()` which
performs a runtime borrow check.

Key methods (all have default no-op impls):

```rust
impl<'v> Object<'v> for Widget {
    const NAME: &'v str = "Widget";
    const MODULE: &'v str = "my_ext";
    type Annex = WidgetAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    // Called at registration time to add methods, getters, supertypes
    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("foo", async move |this, strand, args, out| { ... })
            .get("bar", |this, strand, out| { ... })
    }

    // Iterator protocol: return self or an iterator
    async fn input<'a, 's>(this: Instance<'v, 'a, Self>, strand, out) -> Result<'v, 's, ()>;
    // Yield next item; return Ok(true) if yielded, Ok(false) if exhausted
    async fn next<'a, 's>(this: Instance<'v, 'a, Self>, strand, out) -> Result<'v, 's, bool>;
    // Destructuring support
    async fn unpack<'a, 's>(this, strand, unpack: Unpack<'v, 'a>) -> Result<'v, 's, ()>;
    // Display/debug for string conversion
    fn display<'a, 's>(this, strand, w: &mut dyn fmt::Write) -> Result<'v, 's, ()>;
}
```

### `TypeBuilder` — Registering Methods and Properties

`TypeBuilder` is the API used inside `Object::build` to register methods,
getters, setters, and supertypes.

```rust
fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
    let some_sym = builder.sym("some_key"); // register a symbol (see below)
    builder
        // Instance methods
        .method("name", async move |this, strand, args, out| { ... })
        .method_with_slots("name", async move |this, strand, args, out, [s0, s1]| { ... })
        // Getters / setters
        .get("prop", |this, strand, out| { ... })
        .set("prop", |this, strand, value| { ... })
        // Type-level methods (on the type object, not instances)
        .type_method("class_method", async move |ty, strand, args, out| { ... })
        // Supertypes (e.g. Iter for iterator types)
        .supertype(TypeObject::Iter)
}
```

**Method signatures** (types are usually inferred):

- **Method**: `async |this: Instance<'v, 'b, T>, strand: &mut Strand<'v, 's>,
  args: Args<'v, 'b>, out: Slot<'v, 'b>| -> Result<'v, 's, ()>`
- **Method with scratch slots**: same but adds `[Slot<'v,'b>; N]` at the end.
  Scratch slots are GC-rooted temporaries for intermediate values.
- **Getter**: `|this: Instance<'v,'b,T>, strand: &mut Strand<'v,'s>,
  out: Slot<'v,'b>| -> Result<'v,'s,()>` (sync, not async)

### Symbol Registration

Keyword argument names and any other interned symbols must be registered with
`Register::sym` (or `TypeBuilder::sym`, which derefs to `Register`). Capture the
returned `Sym` in a closure — symbols are `Copy`.

```rust
fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
    let limit_sym = builder.sym("limit");
    builder
        .method("split", async move |this, strand, args, out| {
            let ([haystack], [limit]) =
                unpack!(strand, args, 1, 0, limit_sym = None)?;
            //     positional: 1 required, 0 optional ^^
            //     keyword: limit_sym with default None  ^^
            // limit is Option<&Value<'v>> — None if not passed
            ...
        })
}
```

Bareword Do-side dict/vertical-layout keys (`method: GET`, unquoted identifier
keys generally) compile to `Sym` values, not `Str` — even though they look
like plain text. Looking one up from Rust with a raw `&str` via
`Dict::get(strand, "method", ...)` silently returns not-found, since `Str`
and `Sym` are distinct key types. Always look up a known bareword key with a
pre-interned `Sym` (`builder.sym("method")`), never a raw `&str`. Quoted keys
(`"x-custom": v`) produce real `Str` keys, so a dict built from a mix of
bareword and quoted keys may have mixed `Str`/`Sym` key types — this only
matters for `Dict::get()` with a fixed expected key, not for generic
iteration (`Dict::pairs()` + `.to_string(strand)` stringifies either kind the
same way).

### Argument Unpacking (`unpack!`)

The `unpack!` macro destructures `Args` into positional and keyword arguments.

```rust
// 2 required positional, 1 optional positional, no keywords
let ([a, b], [c]) = unpack!(strand, args, 2, 1)?;
// c: Option<&Value>

// 1 required positional, 0 optional, 1 keyword with default
let ([haystack], [limit]) = unpack!(strand, args, 1, 0, limit_sym = None)?;
// limit: Option<&Value>
```

### Creating Instances

Use `Type::create` (when `Annex: Default`) or `Type::create_with_annex`:

```rust
// In a method or module function:
global.types.widget.create_with_annex(
    strand,
    Widget { /* mutable state */ },
    WidgetAnnex { global, /* immutable state */ },
    &mut out, // Slot to place the new object into
);
```

After creation, you can downcast a `Slot`/`Value` back to an `Instance`:

```rust
let instance = global.types.widget.downcast(&out).unwrap();
let borrow = instance.borrow(strand)?;      // Ref<Widget> — shared
let mut borrow = instance.borrow_mut(strand)?; // Mut<Widget> — exclusive
let annex = instance.annex();                // &WidgetAnnex — no borrow check
```

### GC Slots and Lifetime Transmuting

When wrapping types that borrow from GC-managed values (e.g. a regex iterator
that borrows both a `Regex` and a haystack `str`), you cannot store the
borrowed references directly because the GC may move or collect the referents.
The solution:

1. **Declare `SLOTS`** — each slot is a GC-scanned `Value` that keeps a
   referent alive.
2. **Transmute borrowed lifetimes to `'static`** — strip the borrow's lifetime
   so it can be stored in the struct.
3. **Store the original GC values in slots** — the slots keep the referents
   alive for as long as the object exists, making the transmuted references
   valid.

```rust
pub(crate) struct Find {
    // SAFETY: transmuted to 'static; actual borrows kept alive by slots
    iter: regex::CaptureMatches<'static, 'static>,
}

impl<'v> Object<'v> for Find {
    const SLOTS: usize = 2; // slot 0 = regex, slot 1 = haystack
    type Annex = FindAnnex<'v>;
    // ...
}
```

Population pattern (inside a method that creates the object):

```rust
// 1. Create the borrowed iterator
let iter = annex.regex.find_iter(hay);

// 2. Transmute to 'static (UNSAFE: must root referents in slots)
let iter = unsafe {
    mem::transmute::<regex::CaptureMatches<'_, '_>,
                     regex::CaptureMatches<'static, 'static>>(iter)
};

// 3. Create the object
global.types.find.create_with_annex(
    strand, Find { iter }, FindAnnex { global }, &mut out,
);

// 4. Root the referents in slots (keeps them alive for GC)
let mut borrow = global.types.find.downcast(&out).unwrap().borrow_mut_unwrap();
Output::set(strand, Mut::slot_mut::<0>(&mut borrow), this);     // regex
Output::set(strand, Mut::slot_mut::<1>(&mut borrow), haystack); // haystack string
```

Slots are accessed via `Ref::slot::<N>` (read) and `Mut::slot_mut::<N>` (write).
The const generic `N` is bounds-checked at compile time against `SLOTS`.

**Key invariant**: the transmuted references are only valid as long as the
slot values are alive. Slots are scanned by the GC, so the referents will not
be collected while the object exists. Never clear or overwrite a slot that
backs a transmuted reference.
