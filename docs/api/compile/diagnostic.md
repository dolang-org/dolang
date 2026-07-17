# Diagnostic

A compiler diagnostic produced by [`compile`](./index.md).

## Fields

### `severity`

The diagnostic severity as a symbol:

- `:ERROR:`
- `:WARNING:`

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

## Methods

### `render`

Renders the diagnostic for terminal presentation. The rendered value does not
include a final newline.

**Returns:** [`term.Text`](../term/text.md)

## Example

```
let result = compile "bad.dol" "let =\n"
for diag = result.diagnostics
  echo $diag.render()
```
