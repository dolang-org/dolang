# Style

Stores reusable terminal style settings produced by
[`term.style`](./index.md#style-options-args) without positional arguments.

Calling a `Style` applies its saved settings. Supplied options override saved
colors or enable additional attributes. Omitted options retain saved settings;
`:INHERIT:` clears them.

## Operators

### `style :...options ...args`

Applies or derives the saved style.

**Parameters:**

| Name      | Type | Description                         |
| --------- | ---- | ----------------------------------- |
| `...args` | *    | Values converted to display strings |

Also accepts the [`term` style options](./index.md#style-options).

**Returns:** [`Text`](./text.md) when positional arguments are provided;
otherwise a derived `Style`

```
let warning = term.style fg: :YELLOW: bold: true
let urgent = warning fg: :RED: underline: true
let uncolored = urgent fg: :INHERIT:

echo $warning("Warning")
echo $urgent("Failure")
echo $uncolored("Notice")
```
