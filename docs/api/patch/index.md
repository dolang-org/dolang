# patch

The `patch` module parses, creates, encodes, and applies unified and git-style
patches.

## Types

| Type                             | Description                          |
| -------------------------------- | ------------------------------------ |
| [`Patch`](./patch.md)            | One file-level patch operation       |
| [`PatchIter`](./patchiter.md)    | Iterator over a patch stream         |
| [`ParseError`](./parseerror.md)  | Patch parsing error                  |
| [`ApplyError`](./applyerror.md)  | Error while applying a patch         |

## Functions

### `decode input`

Parses a patch stream.

**Parameters:**

| Name    | Type                                              | Description                |
| ------- | ------------------------------------------------- | -------------------------- |
| `input` | [`str`](../std/str.md)\|[`bin`](../std/bin.md)    | Unified or git-style patch |

**Returns:** [`PatchIter`](./patchiter.md)

**Errors:**

- Raises [`ParseError`](./parseerror.md) when iteration reaches malformed patch
  data

```
let patches = [...patch.decode diff_text]
```

### `diff before after :source? :target?`

Builds a text patch from two versions of the same content.

`before` and `after` must both be [`str`](../std/str.md) or both be
[`bin`](../std/bin.md).

**Parameters:**

| Name     | Type                                                                               | Description                            |
| -------- | ---------------------------------------------------------------------------------- | -------------------------------------- |
| `before` | [`str`](../std/str.md)\|[`bin`](../std/bin.md)                                     | Original content                       |
| `after`  | [`str`](../std/str.md)\|[`bin`](../std/bin.md)                                     | Modified content                       |
| `source` | [`Path`](../fs/path.md)\|[`str`](../std/str.md)?                                   | Source filename for the patch headers  |
| `target` | [`Path`](../fs/path.md)\|[`str`](../std/str.md)?                                   | Target filename for the patch headers  |

**Returns:** [`Patch`](./patch.md)

**Errors:**

- Raises `TypeError` if `before` and `after` are not both text or both binary
- Raises `TypeError` if `source` or `target` is not a [`Path`](../fs/path.md)
  or [`str`](../std/str.md)

```
let p = patch.diff "alpha\n" "beta\n" source: old.txt target: new.txt
echo (patch.encode p)
```

### `encode value`

Encodes a [`Patch`](./patch.md) or iterable of patches back to patch text.

When every encoded byte is valid UTF-8, this returns a
[`str`](../std/str.md). Otherwise it returns [`bin`](../std/bin.md).

**Parameters:**

| Name    | Type                              | Description                          |
| ------- | --------------------------------- | ------------------------------------ |
| `value` | [`Patch`](./patch.md)\|iterable   | One patch or an iterable of patches  |

**Returns:** [`str`](../std/str.md)\|[`bin`](../std/bin.md)

**Errors:**

- Raises `TypeError` if an iterable contains a non-`Patch` value

```
let patches = [...patch.decode diff_text]
write output.patch (patch.encode patches)
```
