# shell

The `shell` module provides shell-context values and functions.

## Types

| Name                            | Description                                              |
| ------------------------------- | -------------------------------------------------------- |
| [`Vfs`](./vfs.md)               | Shell VFS context handle                                 |

## Functions

### `echo ...args`

Prints arguments to the terminal output, separated by spaces, followed by a
newline.

#### Parameters

| Name      | Type | Description                            |
| --------- | ---- | -------------------------------------- |
| `...args` |      | values to print (converted with `arg`) |

#### Returns

`nil`

### `print arg`

Prints one value to the terminal output without a trailing newline.

#### Parameters

| Name  | Type | Description                 |
| ----- | ---- | --------------------------- |
| `arg` |      | value to print              |

#### Returns

`nil`

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

Executes a callable in a fresh host context, regardless of the current
context.

#### Parameters

| Name   | Type | Description                            |
| ------ | ---- | -------------------------------------- |
| `func` | func | Block to execute in fresh host context |
| `args` |      | Additional arguments to pass to `func` |

#### Returns

Return value of the executed callable

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

The path to the current `dolang` executable.
