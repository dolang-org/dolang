# Patch

One file-level patch operation.

`Patch` values are returned by
[`patch.diff`](./index.md#diff-before-after-source-target) and yielded by
[`patch.decode`](./index.md#decode-input).

## Fields

### `type`

Operation kind as a symbol.

Possible values are `:CREATE:`, `:DELETE:`, `:MODIFY:`, `:MOVE:`, and `:COPY:`.

```
let p = patch.diff "alpha\n" "beta\n"
assert_eq $p.type :MODIFY:
```

### `source`

Source path of the patch as a [`Path`](../fs/path.md).

This field is present for delete, modify, move, and copy operations.

```
let p = [...patch.decode diff_text][0]
echo $p.source
```

### `target`

Target path of the patch as a [`Path`](../fs/path.md).

This field is present for create, modify, move, and copy operations.

```
let p = [...patch.decode diff_text][0]
echo $p.target
```

## Methods

### `apply base`

Applies the patch to `base`.

`base` may be text or binary. If `base` is text, the patched result must still
be valid UTF-8.

#### Parameters

| Name   | Type                                              | Description                    |
| ------ | ------------------------------------------------- | ------------------------------ |
| `base` | [`str`](../std/str.md)\|[`bin`](../std/bin.md)    | Content to patch               |

#### Returns

[`str`](../std/str.md)\|[`bin`](../std/bin.md)

#### Errors

| Exception                       | Condition                                                      |
| ------------------------------- | -------------------------------------------------------------- |
| [`ApplyError`](./applyerror.md) | The patch does not apply cleanly                               |
| [`ApplyError`](./applyerror.md) | A text result is not valid UTF-8                               |
| `TypeError`                     | `base` is not [`str`](../std/str.md) or [`bin`](../std/bin.md) |

```
let p = patch.diff "alpha\n" "beta\n"
assert_eq (p.apply "alpha\n") "beta\n"
```
