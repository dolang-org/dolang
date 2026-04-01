# glob

Matches strings with compiled glob patterns using `wax`.

This module is for side-effect-free glob matching. It does not walk the
filesystem and does not overlap with the traversal APIs in `fs`.

## Types

| Type                | Description            |
| ------------------- | ---------------------- |
| [`Glob`](./glob.md) | Compiled glob matcher. |

## Functions

### `matches pattern value`

Tests whether `value` matches a glob pattern.

`pattern` may be either a [`Glob`](./glob.md) or a
[`str`](../std/str.md). Passing a `str` compiles it for that call only.

**Parameters:**

| Name      | Type                                        | Description                 |
| --------- | ------------------------------------------- | --------------------------- |
| `pattern` | [`Glob`](./glob.md)\|[`str`](../std/str.md) | Glob object or pattern text |
| `value`   | [`str`](../std/str.md)                      | Candidate string to test    |

**Returns:** [`bool`](../std/index.md) indicating whether the value matches.

```
let png = glob.Glob "**/*.png"

assert (glob.matches png "assets/logo.png")
assert (glob.matches "**/*.png" "assets/logo.png")
assert (!(glob.matches "**/*.png" "assets/logo.jpg"))
```
