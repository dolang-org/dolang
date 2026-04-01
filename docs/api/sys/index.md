# sys

The `sys` module provides functions and objects for OS interaction and
system-level operations.

## Types

| Name                                                    | Description                                   |
| ------------------------------------------------------- | --------------------------------------------- |
| [`Error`](./error.md)                                   | Error raised for system and I/O failures      |
| [`NotFoundError`](./not-found-error.md)                 | Subtype for missing files, paths, or programs |
| [`PermissionDeniedError`](./permission-denied-error.md) | Subtype for permission failures               |
| [`AlreadyExistsError`](./already-exists-error.md)       | Subtype for existing-path conflicts           |
| [`TimedOutError`](./timed-out-error.md)                 | Subtype for timed-out system operations       |

## Functions

### `echo ...args`

Prints arguments to the terminal output, separated by spaces, followed by a
newline. This always prints to the terminal regardless of I/O redirection.

**Parameters:**

| Name      | Type | Description                            |
| --------- | ---- | -------------------------------------- |
| `...args` |      | values to print (converted with `ext`) |

```
echo hello world
# prints: hello world
```

### `exit code?`

Exits the script with the given status code.

**Parameters:**

| Name   | Type                   | Description              |
| ------ | ---------------------- | ------------------------ |
| `code` | [`int`](../std/int.md) | exit status (default: 0) |

```
exit()     # exits with 0
exit 1     # exits with 1
```

### `cd path? func?`

With no arguments, returns the current working directory. With a path, changes
the current working directory. If a callable is also provided, the directory is
changed only for the duration of that call, then restored.

**Parameters:**

| Name   | Type                   | Description                          |
| ------ | ---------------------- | ------------------------------------ |
| `path` | [`str`](../std/str.md) | directory path                       |
| `func` |                        | callable to run in the new directory |

**Returns:** Current working directory (no arguments), or result of `func`.

```
# Print current directory
echo $cd()

# Permanent change
cd /tmp

# Scoped change
cd /tmp do
  echo "In /tmp"
# Back to original directory
```

## Objects

### `env`

An object for accessing environment variables.

#### Indexing

```
# Get a variable (errors if not set)
let path = env["PATH"]

# Set a variable
env["MY_VAR"] = "hello"

# Unset a variable
env["MY_VAR"] = nil
```

#### `env func key: value ...`

Runs a callable with temporary environment variable overrides.

```
env HOME: /tmp do
  echo $env["HOME"]  # /tmp
# HOME is restored
```

#### Methods

##### `env.get key :default? :else?`

Gets an environment variable with a fallback.

```
let val = env.get HOME default: /tmp
```

### `args`

An [`array`](../std/array.md) of command-line arguments passed to the
script.

### `program`

Identifies what `dolang-shell` is executing.

- For `dolang-shell script.dol`, this is an [`fs.Path`](../fs/path.md) for
  `script.dol`.
- For `dolang-shell -m foo.bar`, this is the string `"foo.bar"`.
- In the REPL, this is `nil`.

### `os`

A string indicating the current operating system (e.g., "linux", "macos",
"windows").

```
if (os == "linux")
  echo "Running on Linux"
```
