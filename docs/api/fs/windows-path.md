# WindowsPath

[`Path`](path.md) using Windows path syntax.

## Constructor

### `WindowsPath(path)`

Creates a Windows path without consulting the host platform.

**Parameters:**

| Name   | Type                                      | Description |
| ------ | ----------------------------------------- | ----------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path value  |

**Returns:** `WindowsPath`.

Converting a Unix path is allowed only when it is relative and unrooted.

See [`Path`](path.md) for fields, methods, and operators.
