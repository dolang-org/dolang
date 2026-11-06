# Language Server

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
- **Highlight**: Every use in the file of the name under the cursor,
  highlighted as the cursor moves

Highlighting covers names the compiler resolves: a field access such as
`self.count` is dynamic, so only the field's declaration is highlighted. Go to
definition reaches members, overloads and other
modules once the file has been type checked.

## Running the LSP

Build `dolang-lsp` alongside `dolang` and `dolang-vfs` for a complete source
installation:

```bash
cargo build --release --bin dolang --bin dolang-lsp --bin dolang-vfs
```

Configure your editor to use `dolang-lsp` as the language server for `.dol`
files.

The [VS Code extension](./vscode.md) starts `dolang-lsp` automatically.

## Projects

The server finds each file's [project](./index.md#finding-the-root) by
convention. With no [configuration](./lsp-config.md), a project has:

- `lib/` under the root as its module path, followed by the bundled modules;
- the default prelude.

A file under `lib/` is checked as its [module](./index.md#layout); other files
are scripts.

The VS Code extension runs scripts and starts the interactive shell with the
same module path, passing each directory to `dolang` as `--module-path`. Other
editors can ask for it with the `dolang/modulePath` request. Its parameter is
`{uri}`, a file or a directory. Its result has the project's `root` and its
`modulePath`, the module path's directories that exist, in search order.
