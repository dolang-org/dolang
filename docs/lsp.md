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
- **Go to definition**: Jump to the definition of variables and functions,
  of an item of a module on the module path, by its import or by a path such
  as `a.f`, and of the member a use such as `x.size` reaches. A call of an
  overloaded function or method jumps to the `@def` it chose, and a member of a
  union lists the declaration in each class.
- **Document symbols**: The outline of a file -- classes, functions, methods,
  fields, bindings and imports, nested under whatever declares them
- **Find references and highlight**: Every use of the name under the cursor,
  for the "find all references" command and for highlighting occurrences as
  the cursor moves

References are limited to the current file, and resolve names the compiler
resolves: a field access such as `self.count` is dynamic, so only the field's
declaration is reported. Go to definition reaches members, overloads and other
modules once the file has been type checked.

## Running the LSP

Build `dolang-lsp` alongside `dolang` and `dolang-vfs` for a complete source
installation:

```bash
cargo build --release --bin dolang --bin dolang-lsp --bin dolang-vfs
```

Configure your editor to use `dolang-lsp` as the language server for `.dol`
files.

The repository's VS Code extension starts `dolang-lsp` automatically.

## Projects

The LSP finds a file's project by convention. Its root is the nearest
directory above the file that contains `.git`, or else the file's own
directory. A submodule's or worktree's `.git` file makes that directory a root
of its own.

With no configuration, a project has:

- `lib/` under the root as its module path, followed by the bundled modules;
- the default prelude.

A file under `lib/` is checked as its module: `lib/a/b.dol` or
`lib/a/b/mod.dol` is module `a.b`, in place of the bundled module of the same
name. Other files are scripts. An import is found on the module path first,
then among the bundled modules.

## Configuration with `.dolang-lsp.toml`

A project with an unusual layout describes it in a `.dolang-lsp.toml` file. A
configuration may set two tables, `prelude` and `modules`:

- At the root, a configuration replaces the convention's settings for the
  tables it sets.
- Below the root, a configuration overrides the prelude for its directory and
  everything under it. Only the root's configuration may set `modules`, so a
  module name means the same thing throughout a project. A `modules` table
  anywhere else is an error.

A table a configuration leaves out comes from the nearest enclosing
configuration, and then from the convention. The server logs a configuration it
can't read, and the files it covers have an empty prelude. The server watches
configuration files, when the editor allows it, and applies a change to the
documents it affects.

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
cd = true
sub = true

[prelude.proc]
mod = true
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

When no configuration sets a prelude, the LSP defaults to the `dolang`
prelude, which also includes the core language prelude.

### Module Path

A `modules` table at the root replaces `lib/` with the directories it lists,
as `dolang -m compile check --path` does:

```toml
[modules]
path = ["lib", "*/stub"]
bundled = true
```

`path` lists directories searched in order. `a/b.dol` or `a/b/mod.dol` under
one of them is module `a.b`. A `*` matches any run of characters within one
path component, except a leading `.`; matches are taken in name order.

Modules and scripts both have the prelude of their directory, as the shell
compiles both under its own. Set `bundled = false` to leave the bundled modules
out, as `--no-bundled` does.

A configuration may come from any repository an editor opens, so the server
reads only inside the root:

- Each entry is relative to the root. Absolute paths and `..` components are
  rejected.
- Each directory, after following symbolic links, must lie inside the root.
- Each module file, after following symbolic links, must be a regular file
  inside its directory on the module path. This is checked whenever the file
  is read.

The server logs a rejected entry or module file and goes on without it.

### When to Use This

The config file is primarily useful for **non-`dolang` projects** -- for
example, when Do is embedded in a Rust application with custom native
functions. It tells the LSP what names your embedding provides so diagnostics
don't report false "unbound variable" errors.

For `dolang` projects, the default prelude is correct, and a project that keeps
its modules in `lib/` needs no configuration.
