# Module Paths and Caching

## Module Resolution

When a script imports a module, the shell resolves it by searching configured
module paths, followed by bundled modules. The **current directory is not
searched by default**.

### Module Search Paths

Modules are resolved in this order:

1. Paths supplied with repeatable **`--module-path <path>`** options, in the
   order specified
2. **Site directory**:

    | Platform | Location                                            |
    | -------- | --------------------------------------------------- |
    | Linux    | `${XDG_DATA_HOME:-$HOME/.local/share}/dolang/site/` |
    | macOS    | `~/Library/Application Support/dolang/site/`        |
    | Windows  | `%APPDATA%\dolang\site\`                            |

For example, `dolang --module-path ./lib --module-path ../shared script.dol`
searches `./lib`, then `../shared`, then the site directory.

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

| Platform | Location                                           |
| -------- | -------------------------------------------------- |
| Linux    | `${XDG_CACHE_HOME:-$HOME/.cache}/dolang/bytecode/` |
| macOS    | `~/Library/Caches/dolang/bytecode/`                |
| Windows  | `%LOCALAPPDATA%\dolang\cache\bytecode\`            |

Cache files are named by a Blake3 hash of the source file path and compilation
mode, with a `.dolc` extension. The cache is automatically invalidated when the
source file is newer than the cached bytecode.

This happens automatically and requires no configuration.

For programmatic loading from Do code, use the [`exec`](exec)
module. It wraps [`compile`](compile) and
[`load`](load) and stores cached bytecode under an
application-scoped subdirectory of
[`fs.cache_dir()`](fs.cache_dir).
