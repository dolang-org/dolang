# Syntax Design

Build and DevOps automation mixes three kinds of content:

- **Declarative content**: configuration, manifests, dependency lists,
  argument records.
- **Program invocations**, whose arguments are mostly literal strings with
  nearly arbitrary punctuation: paths, URLs, flags, globs.
- **General computation**, such as filtering a list or computing a dependency
  fixed point.

Many systems split these between two languages, often with one embedded in the
other: a Makefile and sh, a Dockerfile and bash, a GitHub Actions workflow and
PowerShell, `Cargo.toml` and `build.rs`. The declarative language is pleasant
until it needs logic, and the logic lives in a language that makes declarative
content a chore.

Another approach builds everything in a general-purpose language. Ad-hoc logic
is then easy, but declarative content requires more boilerplate. Sometimes you
just want a big list of source files or a build matrix; formats like TOML or
YAML make this simple and readable. General purpose langauges also tend to
require more boilerplate to invoke external programs than shell script, and
most build/automation pipelines spend substantial time doing precisely that.

Do aims to handle all three in one syntax, borrowing most heavily from YAML,
shell script, and Python, with plenty of inspiration from Rust and Ruby. This
entails certain quirks at the resulting intersection, but hopefully ones that
are as logical and unobtrusive as possible given the design constraints.

Substantial effort was put into adding good compiler errors, warnings, and
lints where syntax quirks might bite. Quick fixes are often available in an
IDE, and type checking adds another layer of defense.

## Literal by Default

At [statement level](./index.md#statement-level), a token is a literal string
unless something marks it otherwise, as in sh:

```
curl -fsSL https://example.com/install.sh?version=1.2
```

Making names variables by default, as most languages do, would force quoting
on nearly every argument of a typical invocation. Instead, `$` marks the places
that aren't literal, and `$name` reads the same here as it does in shell script.

## Leaving Literal Text

Scripts still need arithmetic, indexing, field access, etc. Two expression
levels provide them:

- **Full expressions** in `()`, `[]` and `{}` are C-like: operators work and
  whitespace is insignificant. Brackets already delimit them, so no sigil is
  needed.
- **Compact expressions** after `$` cover what most commonly appears inside an
  argument: variables, fields, indexing and even C-style calls, but ending at
  whitespace.

## Implicit Compact Expressions

Some positions are [compact expressions](./expressions.md#implicit) without a
`$`. Each is a position where a literal string would be useless:

| Position                | String literal there would be... |
| ----------------------- | -------------------------------- |
| A command's receiver    | Not be callable                  |
| `if`/`while` condition  | Always true                      |
| `for` iteratee          | Not be iterable                  |
| `bind` scrutinee        | Not be destructurable            |
| `let` right-hand side   | Rarely be intended               |
| `return`/`throw` value  | Rarely be intended               |

Requiring `$` everywhere would be more uniform, but `if foo` would always test
a string literal, and every command would have to start with `$func`, since a
string can't be called. The compiler raises an error or warning with a quick
fix when a superfluous `$` is used in these positions rather than ascribing it
some other meaning.

These positions also accept command arguments, so a command can be
changed into a test or assigned to a variable without rewriting it:

```
if fs.exists config.toml
  echo found

let found = fs.exists config.toml
```

## `$` as Application

Apart from introducing a compact expression, `$` is an operator: a
low-precedence, right-associative application of what is on its left to what
is on its right, or to the indented block below.

| Form                    | Applies                                    |
| ----------------------- | ------------------------------------------ |
| `echo $ str $ Range 10` | A function to the rest of the line         |
| `Dict $ pairs`          | A type constructor to the rest of the line |
| `let x = $` and a block | Nothing: the block is the value            |
| `key: $ cmd arg`        | Nothing: the command's result is the value |

The pattern and type syntax forms mirror the corresponding expression forms.

## Where Shell Meets YAML

[Vertical layout](./vertical-layout.md) is where both models can apply at once,
and the rule that decides between them is:

- **In vertical contexts, YAML conventions take precedence.** Dash and key
    items are line items: each spans its line, as a YAML scalar does, and may
    also be continued on subsequent lines.
- **Where an argument list continues, bin-packing is available.**  
  Bin-packed positional arguments are helpful for long command lines invoking
  external programs, but don't "compose" well with other YAML constructs, so
  they are restricted to vertical command arguments or an explicit `$` block.

This means the first line of a command behave differently from subsequent
continuation lines with regard to key arguments:

```
foo x: 1 y: 2      # foo(x: 1, y: 2)

foo
  x: 1 y: 2        # foo(x: "1 y: 2")
```

YAML constructs nest as they do in YAML:

```
foo
  - x: 1
    y: 2           # foo({x: 1, y: 2})

foo
  - - 1
    - 2            # foo([1, 2])
```
