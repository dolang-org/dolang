# proc

The `proc` module provides functions and objects for process invocation and
output capture.

## Types

| Name                  | Description                                      |
| --------------------- | ------------------------------------------------ |
| [`Error`](./error.md) | Error raised when a process exits unsuccessfully |

## Functions

### `mute func ...`

Executes a callable with its output discarded.

**Parameters:**

| Name      | Type | Description                              |
| --------- | ---- | ---------------------------------------- |
| `func`    |      | callable to execute with muted output    |
| `...`     |      | additional arguments passed to `func`    |

**Returns:** The return value of `func`.

The `mute` function redirects the output of the given callable to
[`nulliter`](../std/index.md#nulliter), effectively discarding
`stdout` of any executed external programs.

```
# Execute a command without printing its output
mute do run printf "this will not be printed"
```

### `sub func :trim?`

Captures the output of a callable as a string.

**Parameters:**

| Name   | Type                     | Description                                                        |
| ------ | ------------------------ | ------------------------------------------------------------------ |
| `func` |                          | callable whose output to capture                                   |
| `trim` | [`bool`](../std/bool.md) | whether to trim trailing carriage return/newline (default: `true`) |

**Returns:** [`str`](../std/str.md)

```
let output = sub do echo hello
assert_eq $output "hello"
```

## Module Object

### `run`

The `run` module object provides access to external programs. See
[run](../proc-run.md) for detailed documentation.

```
run.ls -la
run.git status
```
