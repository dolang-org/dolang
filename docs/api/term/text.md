# Text

Validated terminal presentation produced by
[`term.text`](./index.md#text-options-args)
or a [`Style`](./style.md)
or [`term.preformat`](./index.md#preformat-text).

Nested `Text` values are flattened when composed. ANSI reset codes in a nested
value restore the enclosing style.

Terminal output functions render `Text` with ANSI styling when stderr is a
terminal and as plain text otherwise.

Converting it with `str` returns the content with the styling dropped: the
escape sequences are terminal instructions, and a `Str` holding them counts
them in its length, matches them in a search, and writes them into whatever
file or pipe it reaches. Ask for them with [`encode`](#encode). `verbatim`
reproduces a value as it would have been written in source, and styled text
has no source form, so it gives the same content `str` does.

## Formatting

A [format specification](../std/fmt-spec.md) applied to `Text` measures
terminal cells rather than extended grapheme clusters, the same measure
[`width()`](#width) reports: `width` pads to a cell count, `precision` clips to
one at a cluster boundary, and SGR sequences consume neither.

Formatting produces a [`Str`](../std/str.md), so it drops the styling along
with every other `str` conversion: `"${label:6}"` is laid out in cells and
plain.

The console is where the layout and the styling arrive together. Given a
[`FmtValue`](../std/fmt-value.md) bound to a `Text`,
[`echo`](./index.md#echo-args), [`print`](./index.md#print-options-args),
[`text`](./index.md#text-options-args) and a [`Style`](./style.md) apply the
layout to the encoded form themselves, in the same terminal cells, and keep
the styling:

```
let label = text("界a", fg: :RED:)
echo (std.FmtValue label width: 6)     # padded and still red
echo "${label:6}"                      # padded and plain
```

A specification asking for a debug or numeric rendering is no longer a request
for terminal presentation, and is sanitized like any other value.

## Sequences

A `"..."` concatenates its interpolations as it is built, so a `Text` among
them is already flattened to its content by the time the console sees the
result. A [`t"..."`](../std/fmt.md) keeps them apart, and the console renders
each one for itself — so styling survives interpolation, at any depth:

```
let label = text("界a", fg: :RED:)
echo t"tag: ${label:6}|"     # padded in cells and still red
echo "tag: ${label:6}|"      # padded and plain

let framed = t"[${label:^8}]"
echo t"<${framed:>12}>"      # each level lays out what the one inside rendered
```

Expanding a sequence this way is the console's own decision, not a conversion:
no conversion expands one, which is what stops a sequence from arriving at a
consumer already flattened. See [Trust](../std/fmt.md#trust).

Each segment is taken exactly as an argument in its own right would be, so the
rules above apply to it unchanged: a `Text` keeps its styling, a specification
asking for a debug or numeric rendering is sanitized, and everything else is
converted — [`verbatim`](../std/index.md) in argument position, `str` inside a
`Text` — and sanitized.

The exception is an unfilled [`FmtParam`](../std/fmt-param.md). A parameter
printed on its own is just a name and shows itself as such, but one still
standing in a sequence means the template was never finished, so the console
raises a [`MissingPosError`](../std/missing-pos-error.md) or
[`MissingKeyError`](../std/missing-key-error.md) as
[`format()`](../std/fmt.md#format-bindings) does rather than printing a hole
where a value was meant to go.

## Methods

### `clip width :suffix?`

Clips the text to a terminal-cell width at an extended grapheme boundary.

SGR sequences do not contribute to the width. If the text already fits, it is
returned unchanged and `suffix` is ignored. Otherwise, the suffix is clipped
to the total budget, its width is reserved, and the longest fitting source
prefix is prepended. Styling remains valid across the cut.

#### Parameters

| Name     | Type                                         | Description                          |
| -------- | -------------------------------------------- | ------------------------------------ |
| `width`  | [`Int`](../std/index.md)                     | Non-negative terminal-cell budget    |
| `suffix` | [`Str`](../std/str.md)\|[`Text`](./text.md)? | Suffix appended only when truncating |

#### Returns

`Text`

#### Example

```
let warning = text("abcdef", fg: :YELLOW:)
echo $warning.clip(4, suffix: "…")
```

### `encode()`

Returns the ANSI representation: exactly the bytes a terminal write emits.

This is the inverse of [`term.preformat`](./index.md#preformat-text), so
`preformat` takes the result back (canonicalizing the SGR it re-emits).

#### Returns

[`Str`](../std/str.md)

#### Example

```
let label = text ERROR fg: :RED:
assert_eq (str label) ERROR
fs.write $path $label.encode()
```

### `indent spaces`

Adds spaces to the beginning of each line without changing ANSI formatting. A
terminal newline does not gain a trailing indentation prefix.

#### Parameters

| Name     | Type                   | Description                   |
| -------- | ---------------------- | ----------------------------- |
| `spaces` | [`Int`](../std/int.md) | Non-negative number of spaces |

#### Returns

`Text`

#### Example

```
let diagnostic = [...unit.diagnostics()][0].render()
echo $diagnostic.indent(4)
```

### `join iter?`

Concatenates values with this text between them, taking each the way
[`text`](./index.md#text-options-args) takes an argument of its own: a `Text`
keeps its styling, and anything else is converted and sanitized.

#### Parameters

| Name   | Type | Description                                      |
| ------ | ---- | ------------------------------------------------ |
| `iter` |      | Iterable to join (uses default input if omitted) |

#### Returns

`Text`

#### Example

```
let sep = text " | " fg: :YELLOW:
echo $sep.join([text("ok", fg: :GREEN:), "then", 3])
```

### `width()`

Returns the visible terminal-cell width of the text.

SGR sequences do not contribute to the width. Widths are summed within each
extended grapheme cluster and capped at two cells per cluster. C0 control
characters contribute zero.

#### Returns

[`Int`](../std/index.md)

#### Example

```
assert_eq ((text "é").width()) 1
assert_eq ((text "👩‍💻").width()) 2
assert_eq ((text "界").width()) 2
```

## Example

```
let label = term.text ERROR fg: :RED: bold: true
echo $label " request failed"

# Preserve ANSI escapes for a file or another process.
let encoded = label.encode()
```
