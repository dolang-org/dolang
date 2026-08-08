# Path

[`fs.Path`](../path.md) using Unix path syntax.

## Constructor

### `Path path`

**Parameters:**

| Name   | Type                                               | Description |
| ------ | -------------------------------------------------- | ----------- |
| `path` | [`Str`](../../std/str.md)\|[`fs.Path`](../path.md) | Path value  |

**Returns:** `Path`.

Converting a Windows path is allowed only when it is relative and has no root,
prefix, or alternate data stream.

See [`fs.Path`](../path.md) for fields, methods, and operators.
