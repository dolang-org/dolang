# Module Paths and Caching

## Module Resolution

The core Do language only supports built-in modules (like `base64`, `json`, and
`strand`). Filesystem-based module importing is provided by `dolang-shell`.

When a script imports a module, the shell resolves it by searching configured
module paths. The **current directory is not searched by default**.

### Module Search Paths

Modules are resolved in this order:

1. **Site directory**:
   - `~/.local/share/dolang-shell/site/` (Unix)
   - `%APPDATA%\dolang-shell\site\` (Windows)
2. **`DOLANG_SHELL_MODULE_PATH` environment variable**: additional paths,
   separated by `:` on Unix or `;` on Windows

### File Resolution

Dotted module names map to file paths. For a module name like `foo.bar.baz`,
the shell tries:

1. `<search_path>/foo/bar/baz.dol`
2. `<search_path>/foo/bar/baz/mod.dol`

```
import mylib
# Searches for mylib.dol or mylib/mod.dol in module paths
```

## Bytecode Cache

The shell caches compiled bytecode to speed up subsequent loads. The cache is
stored in a central location:

- Linux/macOS: `~/.cache/dolang-shell/bytecode/`
- Windows: `%LOCALAPPDATA%\dolang-shell\bytecode\`

Cache files are named by a Blake3 hash of the source file path and compilation
mode, with a `.dolc` extension. The cache is automatically invalidated when the
source file is newer than the cached bytecode.

This happens automatically and requires no configuration.
