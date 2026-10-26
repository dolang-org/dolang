# LSP Guide

`dolang-lsp` is a Language Server Protocol server for Do, providing IDE
features in any LSP-compatible editor.

## Supported Features

- **Semantic tokens**: Syntax highlighting based on the compiler's
  understanding of the code (not just regex patterns). Tokens are marked with
  the `declaration`/`definition` modifiers where a name is introduced, the
  `defaultLibrary` modifier for prelude bindings, and the `static` modifier for
  a field a `#[class]` or `#[static]` decorator scopes to its class
- **Diagnostics**: Compile-time errors and warnings reported in real time
- **Quick fixes**: Code actions that apply a diagnostic's suggested fix
- **Hover**: The signature and doc comment of the name under the cursor,
  including imported and prelude names
- **Go to definition**: Jump to the definition of variables and functions
- **Document symbols**: The outline of a file -- classes, functions, methods,
  fields, bindings and imports, nested under whatever declares them
- **Find references and highlight**: Every use of the name under the cursor,
  for the "find all references" command and for highlighting occurrences as
  the cursor moves

Both references and go-to-definition are limited to the current file, and
resolve names the compiler resolves: a field access such as `self.count` is
dynamic, so only the field's declaration is reported.

## Running the LSP

Build `dolang-lsp` alongside `dolang` and `dolang-vfs` for a complete source
installation:

```bash
cargo build --release --bin dolang --bin dolang-lsp --bin dolang-vfs
```

Configure your editor to use `dolang-lsp` as the language server for `.dol`
files.

The repository's VS Code extension starts `dolang-lsp` automatically.

## Configuration with `.dolang-lsp.toml`

The LSP searches upward from each file's directory for a configuration file
named `.dolang-lsp.toml`. This file contains static settings. A file uses the
nearest configuration only; enclosing ones don't contribute. The server reads a
configuration once, so restart it after editing one.

### Prelude Override

The primary use of the config file is to specify the prelude -- the set of
imports that the LSP assumes are available in scope. This tells the LSP what
names exist so it can provide accurate diagnostics and hover text.

The file may contain a `prelude` table describing what to import.

```
# .dolang-lsp.toml
[prelude]
my_module = true
```

Supported forms:

```toml
[prelude]
sys = true
regression = ["assert", "log"]

[prelude.shell]
echo = true
env = true

[prelude.proc]
mod = true
sub = true
run = true
```

Prefix an item name with `@` to bind it only in types. In a table, the
prefix belongs to the source name; an alias has no prefix:

```toml
[prelude]
geometry = ["@Point"]

[prelude.collections]
"@Sequence" = "Seq"
```

### Default Prelude

When no `.dolang-lsp.toml` is found, or it has no `prelude` table, the LSP
defaults to the `dolang` prelude, which also includes the core language
prelude.

### Module Path

A `modules` table puts directories on the module path, as
`dolang -m compile check --path` does:

```toml
[modules]
path = ["lib", "*/stub"]
bundled = true
```

`path` lists directories searched in order. `a/b.dol` or `a/b/mod.dol` under
one of them is module `a.b`. A `*` matches any run of characters within one
path component, except a leading `.`; matches are taken in name order.

A file under the module path is checked as its module, in place of the bundled
module of the same name, and has only the core language prelude. Other files
are scripts. An import is found on the module path first, then among the
bundled modules. Set `bundled = false` to leave the bundled modules out, as
`--no-bundled` does.

A configuration may come from any repository an editor opens, so the server
reads only inside the configuration's directory:

- Each entry is relative to that directory. Absolute paths and `..` components
  are rejected.
- Each directory, after following symbolic links, must lie inside the
  configuration's directory.
- Each module file, after following symbolic links, must be a regular file
  inside its directory on the module path. This is checked whenever the file
  is read.

The server logs a rejected entry or module file and goes on without it.

### When to Use This

The config file is primarily useful for **non-`dolang` projects** -- for
example, when Do is embedded in a Rust application with custom native
functions. It tells the LSP what names your embedding provides so diagnostics
don't report false "unbound variable" errors.

For `dolang` projects, the default prelude is correct. A project that keeps
modules in a source tree needs a configuration only for the module path.
