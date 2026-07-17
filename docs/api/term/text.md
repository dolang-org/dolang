# Text

Validated terminal presentation produced by
[`term.style`](./index.md#style-options-args)
or a [`Style`](./style.md)
or [`term.preformat`](./index.md#preformat-text).

Nested `Text` values are flattened when composed. ANSI reset codes in a nested
value restore the enclosing style.

Terminal output functions render `Text` with ANSI styling when stderr is a
terminal and as plain text otherwise. Converting it with `str` returns its full
ANSI representation.

## Methods

### `indent spaces`

Adds spaces to the beginning of each line without changing ANSI formatting. A
terminal newline does not gain a trailing indentation prefix.

**Parameters:**

| Name     | Type                   | Description                   |
| -------- | ---------------------- | ----------------------------- |
| `spaces` | [`int`](../std/int.md) | Non-negative number of spaces |

**Returns:** `Text`

```
let diagnostic = result.diagnostics[0].render()
echo $diagnostic.indent(4)
```

## Example

```
let label = term.style ERROR fg: :RED: bold: true
echo $label " request failed"

# Preserve ANSI escapes for a file or another process.
let encoded = str $label
```
