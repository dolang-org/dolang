# ApplyError

[`RuntimeError`](../std/runtime-error.md) for patch application failures.

`ApplyError` is raised by [`Patch.apply`](./patch.md#apply-base) when a patch
cannot be applied to the provided base content.

## Inherits

- [`RuntimeError`](../std/runtime-error.md)

## Example

```
let p = patch.diff "alpha\n" "beta\n"

try
  p.apply "gamma\n"
catch patch.ApplyError: err
  echo "apply failed: $err"
```
