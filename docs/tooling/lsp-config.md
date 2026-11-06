# LSP Configuration

A `.dolang-lsp.toml` file overrides how the [language server](./lsp.md) makes
sense of the files under it. A [project](./index.md) laid out by convention
needs none. The file is for code that isn't one, such as Do embedded in a Rust
application with its own native functions: it tells the server which names the
embedding provides, so diagnostics don't report them as unbound.

A configuration may set two tables, `prelude` and `modules`:

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

## Prelude

The `prelude` table lists the imports the server assumes are in scope. When no
configuration sets it, the prelude is `dolang`'s, which includes the core
language prelude.

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

## Module Path

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
