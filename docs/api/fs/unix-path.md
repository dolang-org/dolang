# UnixPath

[`Path`](path.md) using Unix path syntax.

## Constructor

### `UnixPath path`

**Parameters:**

| Name   | Type                                      | Description |
| ------ | ----------------------------------------- | ----------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path value  |

**Returns:** `UnixPath`.

Converting a Windows path is allowed only when it is relative and has no root,
prefix, or alternate data stream.

See [`Path`](path.md) for fields, methods, and operators.
