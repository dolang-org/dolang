# winreg

The `winreg` module provides functions and types for reading and writing the
Windows registry.

This API is VFS-aware: it operates through the VFS in scope for the current
strand, so it works transparently under remote/elevated contexts (e.g.
`sudo.with`) that provide a registry-capable backend. It is only supported on
Windows targets; on other platforms every operation raises
[`sys.UnsupportedError`](../sys/unsupported-error.md).

## Types

| Type                  | Description                                        |
| --------------------- | -------------------------------------------------- |
| [`Key`](./key.md)     | An open registry key                               |
| [`Value`](./value.md) | One named value read from a key (name, kind, data) |

## Functions

### `open root view? access? func?`

Opens a predefined registry root and returns a [`Key`](./key.md).

**Parameters:**

| Name     | Type  | Description                                                                             |
| -------- | ----- | --------------------------------------------------------------------------------------- |
| `root`   | sym   | `:CLASSES_ROOT:`, `:CURRENT_USER:`, `:LOCAL_MACHINE:`, `:USERS:`, or `:CURRENT_CONFIG:` |
| `view`   | sym?  | `:NATIVE:`, `:WOW32:`, or `:WOW64:` (default: `:NATIVE:`)                               |
| `access` | sym?  | `:READ:`, `:WRITE:`, or `:READ_WRITE:` (default: `:READ:`)                              |
| `func`   | func? | Callable to run with the key; auto-closes when done                                     |

**Returns:** [`Key`](./key.md) when no `func` is given, otherwise the result
of calling `func`

```
winreg.open :CURRENT_USER: do |root|
  echo (root.open("Environment").get "TEMP")

let root = winreg.open :LOCAL_MACHINE: access: :READ_WRITE:
root.close()
```
