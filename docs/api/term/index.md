# term

The `term` module writes sanitized terminal output and constructs ANSI-styled
text.

## Types

| Type                  | Description                     |
| --------------------- | ------------------------------- |
| [`Style`](./style.md) | Reusable terminal style         |
| [`Text`](./text.md)   | Validated terminal presentation |

## Style options

The terminal styling functions accept these keyword options:

| Name            | Type                                                                                                    | Description                        |
| --------------- | ------------------------------------------------------------------------------------------------------- | ---------------------------------- |
| `fg`            | [`sym`](../std/sym.md)\|[`int`](../std/int.md)\|[`array`](../std/array.md)\|[`tuple`](../std/tuple.md)? | Foreground color                   |
| `bg`            | [`sym`](../std/sym.md)\|[`int`](../std/int.md)\|[`array`](../std/array.md)\|[`tuple`](../std/tuple.md)? | Background color                   |
| `bold`          | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Enables bold                       |
| `dim`           | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Enables dim intensity              |
| `italic`        | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Enables italics                    |
| `underline`     | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Enables underlining                |
| `blink`         | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Enables blinking                   |
| `reverse`       | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Reverses foreground and background |
| `hidden`        | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Hides text                         |
| `strikethrough` | [`bool`](../std/bool.md)\|[`sym`](../std/sym.md)?                                                       | Enables strikethrough              |

Attribute options accept `true` or `:INHERIT:`. `false` is not accepted.

Named colors are `:BLACK:`, `:RED:`, `:GREEN:`, `:YELLOW:`, `:BLUE:`,
`:MAGENTA:`, `:CYAN:`, and `:WHITE:`. Prefix the name with `BRIGHT_` for a
bright color, such as `:BRIGHT_RED:`. An integer selects a 256-color palette
index. A three-integer array or tuple specifies an RGB color. Numeric values
must be between 0 and 255. Color options also accept `:INHERIT:`.

**Errors:**

- An attribute option is present with a value other than `true` or `:INHERIT:`.
- A color name is unknown, a numeric value is out of range, or an RGB value
  does not contain three integers.

## Functions

### `echo ...args`

Prints arguments separated by spaces, followed by a newline. Ordinary values
are sanitized; direct [`Text`](./text.md) arguments retain their styling.

**Parameters:**

| Name      | Type | Description                                    |
| --------- | ---- | ---------------------------------------------- |
| `...args` | *    | Values converted with `arg` and written safely |

**Returns:** `nil`

```
echo status: ready count: 3
```

### `print :...options ...args`

Prints concatenated values without separators or a trailing newline. Styling
is omitted when stderr is not a terminal.

**Parameters:**

| Name      | Type | Description                         |
| --------- | ---- | ----------------------------------- |
| `...args` | *    | Values converted to display strings |

Also accepts the module's [style options](#style-options). `:INHERIT:` is a
no-op for `print`.

**Returns:** `nil`

```
print "status: " ready fg: :GREEN: bold: true
```

### `style :...options ...args`

Constructs styled terminal text from concatenated values. With no positional
arguments, it returns a reusable [`Style`](./style.md) instead.

**Parameters:**

| Name      | Type | Description                         |
| --------- | ---- | ----------------------------------- |
| `...args` | *    | Values converted to display strings |

Also accepts the module's [style options](#style-options). `:INHERIT:` leaves
a setting to the surrounding style. This is normally the default, but clears
a saved setting when deriving a [`Style`](./style.md).

**Returns:** [`Text`](./text.md) when positional arguments are provided;
otherwise [`Style`](./style.md)

```
let warning = style Warning fg: :YELLOW: bold: true
echo $warning
```

### `preformat text`

Validates existing ANSI-styled text. SGR styling is canonicalized; other
terminal controls, including hyperlinks, are removed.

**Parameters:**

| Name   | Type                   | Description              |
| ------ | ---------------------- | ------------------------ |
| `text` | [`str`](../std/str.md) | ANSI-formatted input     |

**Returns:** [`Text`](./text.md)

```
let formatted = preformat input
echo $formatted
```

Ordinary values preserve newlines and tabs but remove other C0/C1 controls and
escape sequences. Raw stdout and stderr sinks are unchanged and are not
sanitized by this module.
