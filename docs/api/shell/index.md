# shell

The `shell` module provides shell-context values and functions.

## Types

| Name                            | Description                                              |
| ------------------------------- | -------------------------------------------------------- |
| [`Vfs`](./vfs.md)               | Execution context handle                                 |

## Functions

### `exit code?`

Exits the current shell with the given status code.

#### Parameters

| Name   | Type                   | Description              |
| ------ | ---------------------- | ------------------------ |
| `code` | [`int`](../std/int.md) | exit status (default: 0) |

#### Returns

never returns; raises an interrupt error

### `cd path? func?`

With no arguments, returns the current working directory. With a path, changes
the current working directory. If a callable is also provided, the directory is
changed only for the duration of that call, then restored.

#### Parameters

| Name   | Type                                            | Description                          |
| ------ | ----------------------------------------------- | ------------------------------------ |
| `path` | [`str`](../std/str.md)\|[`Path`](../fs/path.md) | directory path                       |
| `func` |                                                 | callable to run in the new directory |

#### Returns

Current working directory (no arguments), or result of `func`.

### `host func ...args`

Executes a callable in the interpreter's original host context, regardless of
the current or nested VFS contexts.

#### Parameters

| Name   | Type | Description                            |
| ------ | ---- | -------------------------------------- |
| `func` | func | Block to execute in fresh host context |
| `args` |      | Additional arguments to pass to `func` |

#### Returns

Return value of the executed callable

### `vfs_exe()`

Returns the current executable reported by the active VFS context, or `nil`
when running on the host filesystem.

#### Returns

[`fs.Path`](../fs/path.md) or `nil`.

### `env overrides func`

Runs `func` with scoped environment overrides. Keys may be strings or symbols.
`nil` unsets a variable and `:INHERIT:` captures its current strand value.

**Parameters:**

| Name        | Type                     | Description           |
| ----------- | ------------------------ | --------------------- |
| `overrides` | [`dict`](../std/dict.md) | Environment overrides |
| `func`      | callable                 | Block to run          |

## Values

### `env`

An object for accessing environment variables.

### `args`

An [`array`](../std/array.md) of command-line arguments passed to the current
shell invocation.

### `program`

Identifies what `dolang` is executing.

- For `dolang script.dol`, this is an [`fs.Path`](../fs/path.md) for
  `script.dol`.
- For `dolang -m foo.bar`, this is the string `"foo.bar"`.
- In the REPL, this is `nil`.

### `exe`

An [`fs.Path`](../fs/path.md) containing the path returned by the host for the
current `dolang` executable. The path is not automatically canonicalized.
