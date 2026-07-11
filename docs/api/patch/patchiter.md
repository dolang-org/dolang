# PatchIter

Iterator over a patch stream.

`PatchIter` is returned by [`patch.decode`](./index.md#decode-input) and
implements [`Iter`](../std/iter.md).

Each iteration yields one [`Patch`](./patch.md).

## Inherits

- [`Iter`](../std/iter.md)

## Example

```
for p = patch.decode diff_text
  echo $p.type
```
