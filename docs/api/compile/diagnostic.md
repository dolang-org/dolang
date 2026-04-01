# Diagnostic

A compiler diagnostic produced by [`compile`](./index.md).

## Fields

### `severity`

The diagnostic severity as a symbol:

- `:error:`
- `:warning:`

### `message`

The primary diagnostic message as [`str`](../std/str.md).

### `span`

The primary source [`Span`](./span.md) for the diagnostic.

### `annotations`

An array of [`Annotation`](./annotation.md) objects describing additional
highlighted source regions.

### `notes`

An array of [`Note`](./note.md) objects with extra context or help.

### `patches`

An array of [`Patch`](./patch.md) objects describing suggested edits.

## Example

```
let result = compile "bad.dol" "let =\n"
import diagnostic
for diag = result.diagnostics
  diagnostic.print_compile_diag $diag
```
