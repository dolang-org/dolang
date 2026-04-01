# Result

The result object returned by `compile`.

## Fields

### `bytecode`

The compiled bytecode as [`bin`](../std/bin.md), or `nil` if compilation
failed.

### `diagnostics`

An array of [`Diagnostic`](./diagnostic.md) objects emitted during compilation.
This includes warnings on successful compilation and errors on failed
compilation.

### `ok`

`true` if bytecode was produced, otherwise `false`.

## Example

```
import diagnostic
let result = compile "example.dol" "let =\n"
if !result.ok
  for diag = result.diagnostics
    diagnostic.print_compile_diag $diag
```
