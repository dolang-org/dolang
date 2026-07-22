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

Internal support crates: **dolang-private-build**, **dolang-private-test**,
**dolang-private-doc**, **dolang-private-highlight**

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
- Data literals: `func [1, 2]`, `func {a: 1}`
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
let result =
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

def connect :host = localhost :port = 8080
  echo "Connecting to $host:$port"

def log level ...rest     # variadic
  echo "[$level]" ...rest
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

Indented blocks under commands become arguments; under `let`/`return` they
construct data:

```
# Vertical arguments
compile_sources
  - foo.c
  - bar.c

# Vertical data (array)
let items =
  - 1
  - 2
  - 3

# Vertical data (dict — at least one key present)
let config =
  host: localhost
  port: 8080

# Nested
let data =
  name: Alice
  scores:
    - 95
    - 87

# for/if in vertical layout
let doubled =
  for i = [1, 2, 3]
    - (i * 2)
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
  pub let x = 0
  pub let y = 0

  def (init) self x y
    self.x = x
    self.y = y

  pub def magnitude self
    ((self.x * self.x + self.y * self.y) / 1.0)

class Dog: Animal              # inheritance
  pub let breed = nil

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
  let count = 0               # private field

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
let :name :age = {name: "Alice", age: 30}

bind args
  - x
  - y = 0
  :verbose = false
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
dodo fmt                   # format Rust and Markdown
dodo lint                  # check for clippy and formatting warnings
dodo cargo-test            # (cargo test, use `--` to pass arbitrary additional arguments to cargo)
dodo shell-test            # (shell integration tests, use `--` to specify alternate arguments to `dolang -m test`)
dodo test                  # cargo and shell tests
dodo mkdocs                # build language docs (MkDocs site in site/)
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

When writing or editing documentation in `docs/`, follow these guidelines to
keep prose direct and technical. The recurring problems these address were
AI-generated verbosity — if a sentence reads like filler, cut it.

### Page Structure

**Module pages** (`index.md` in a module directory, or `module.md` for
leaf modules like `base64.md`):

1. `# module_name` — heading is the bare module name
2. One-line or short paragraph describing the module's purpose
3. `## Types` — only if the module exports type objects. This includes
   native types and Do-defined classes alike (a class is just a type
   defined in Do code). Use a link table:

    ```
    | Type                    | Description              |
    | ----------------------- | ------------------------ |
    | [`State`](./state.md)   | Supertype for ...        |
    | [`Blake3`](./blake3.md) | BLAKE3 state handle      |
    ```

    Don't list internal/return-only types here — only types the module
    exports as values. Types that are only returned by functions (e.g.
    `Result` from `compile`) are mentioned in the relevant function's
    **Returns:** line and linked to their own page from there.
4. `## Functions` — each function as `### \`name args\`` (see below)

**Type pages** (one `.md` file per type):

1. `# TypeName` — the type name (backtick-wrapped if it could be confused
   with prose, e.g. `` # `State` ``)
2. One-line description, ideally stating what supertype it extends if any
   (e.g. `` [`State`](./state.md) for BLAKE3. ``)
3. Sections in order, omitting any that don't apply:
    - `## Constructor` — `### \`TypeName(args)\``; parameters, returns, example
    - `## Fields` — each as `### \`field_name\`` with description and example
    - `## Class Methods` — methods on the type object itself
    - `## Methods` — instance methods, each as `### \`method_name args\``
    - `## Operators` — if the type overloads `+`, `[]`, iteration, etc.
    - `## Example` — a longer worked example if the type warrants one

Each type gets its own page. Don't document a type's full API inline on
the module page — the module page links to the type page.

**Function/method entries** follow this order (omit sections that don't
apply):

1. `### \`name param1 param2 :kw1? :kw2?\`` — signature as heading
2. One-line or short paragraph description
3. `**Parameters:**` — table with Name, Type, Description columns
4. `**Returns:**` — type and brief note
5. `**Errors:**` — bullet list of error conditions (only if non-obvious)
6. Code example (fenced, no label)

### Voice and Brevity

- **Lead with what it does, not what it is.** One-line descriptions should
  state the function/type's purpose directly, not narrate it.
    - Good: `Computes the BLAKE3 digest.`
    - Bad: `Computes the BLAKE3 digest of a string or binary value and returns
      the raw digest bytes.` (the signature already says what it takes and
      returns)
- **Don't restate what the type signature shows.** If parameters and return
  types are in a table, the prose shouldn't repeat them.
- **Don't list interface methods on every concrete type.** If `Blake3`
  implements `State`, say so once — don't re-list `update`, `digest`, etc.
  on the `Blake3` page. Link to `State` instead.
- **Use Do-native terminology.** Say "callable" or "block", not "thunk". Say
  "supertype", not "nominal base type".

### Parameter Tables

- **Optional parameters**: use `?` suffix in the Type column (e.g. `int?`,
  `str?`, `?`). Don't write `**(optional)**`.
- **Variadic/rest parameters**: use `*` in the Type column.
- **Omit type when it's unconstrained**: leave the Type cell empty.
- **Union types**: use `\|` (e.g. ``
  [`str`](./std/str.md)\|[`bin`](./std/bin.md) ``).

### Code Examples

- **Use plain fences. The MkDocs setup handles highlighting without a language
  tag.
- **Don't include `import` lines** in examples unless the example is
  specifically about importing. API doc examples should assume the module's
  exports are in scope.
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
`extension!` to register itself. A typical extension has three files:

- `extension.rs` — trait impl + `extension!` call
- `global.rs` — global state holding `Type` handles
- One or more implementation files — object types, module functions, etc.

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

### Global State (`State<'v, T>`)

`Builder::register_state` stores a value for the lifetime of the VM and returns
a `State<'v, T>` handle. `State` is `Copy` and dereferences to `&T`. Use it to
hold `Type` handles and other VM-lifetime data that methods need.

```rust
use dolang::runtime::{Type, vm::{Builder, Stateful}};

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
    pub(crate) fn new(builder: &mut Builder<'v>) -> Self {
        Self { types: Types {
            widget: builder.register_type(),
            widget_iter: builder.register_type(),
        }}
    }
}
```

### Modules

`Builder::module` creates a native module with exported values and functions.

```rust
pub fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
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
`Builder::sym` (or `TypeBuilder::sym`, which derefs to `Builder`). Capture the
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
