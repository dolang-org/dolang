# LSP Guide

The `dolang-lsp` crate provides a Language Server Protocol server for Do,
enabling IDE features in any LSP-compatible editor.

## Supported Features

- **Semantic tokens**: Syntax highlighting based on the compiler's
  understanding of the code (not just regex patterns)
- **Diagnostics**: Compile-time errors and warnings reported in real time
- **Go to definition**: Jump to the definition of variables and functions

## Running the LSP

The LSP server communicates over stdin/stdout:

```bash
dolang-lsp
```

Configure your editor to use `dolang-lsp` as the language server for `.dol`
files.

## Configuration with `.dolang-lsp.dol`

The LSP searches upward from each file's directory for a configuration file
named `.dolang-lsp.dol`. This file is a Do script that is **executed** and must
return a settings dictionary. Only core modules will be available, so
interacting with the external system is not possible, but the returned settings
can be programmatically derived.

### Prelude Override

The primary use of the config file is to specify the prelude -- the set of
imports that the LSP assumes are available in scope. This tells the LSP what
names exist so it can provide accurate diagnostics and completions.

The returned dictionary should have a `prelude` key describing what to import.

```
# .dolang-lsp.dol
return
  prelude:
    - my_module
```

### Default Prelude

When no `.dolang-lsp.dol` is found, the LSP defaults to the `dolang-shell`
prelude, which also includes the core language prelude.

### When to Use This

The config file is primarily useful for **non-`dolang-shell` projects** -- for
example, when Do is embedded in a Rust application with custom native
functions. It tells the LSP what names your embedding provides so diagnostics
don't report false "unbound variable" errors.

For `dolang-shell` projects, the default prelude is correct and no
configuration file is needed.

### Execution Environment

The config script runs in a sandboxed VM with strict resource limits (4 MB
memory, 250 ms time limit). Compiled config files are cached in
`~/.cache/dolang-lsp/bytecode-cache/`.
