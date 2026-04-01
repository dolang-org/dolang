# progress

The `progress` module provides terminal progress bars and spinners. It
coordinates output so that `echo`, `print`, and child process output do not
clobber progress indicators.

Progress state is implicit: `progress.with` activates a progress context for the
current scope, and `progress.show` creates widgets at the appropriate
nesting depth automatically.

## Functions

### `with func`

Activates a progress context for the duration of `func`. Terminal output (echo,
print, child process stderr/stdout) is routed through the progress display so it
does not interfere with active indicators.

| Name    | Type  | Description             |
| ------- | ----- | ----------------------- |
| `func`  | func  | Callback (no arguments) |
| `style` | dict? | Display style overrides |

**Style dict:**

The optional `style` parameter accepts a dict whose top-level keys name the
visual elements of the progress display. Each element accepts a sub-dict of
properties.

**Elements with width and color:**

| Element   | Default width | Default fg | Default attrs |
| --------- | ------------- | ---------- | ------------- |
| `bar`     | 20            | `cyan`     |               |
| `message` | 30            |            | `bold`        |
| `icon`    | 2             |            | `bold`        |

**Color-only elements:**

| Element    | Default fg | Default attrs |
| ---------- | ---------- | ------------- |
| `spinner`  | `cyan`     |               |
| `elapsed`  |            | `dim`         |
| `position` |            |               |
| `total`    |            |               |

**Properties** (all optional):

| Property | Type                   | Description                                       |
| -------- | ---------------------- | ------------------------------------------------- |
| `width`  | [`int`](../std/int.md) | Character width (bar, message, icon only)         |
| `fg`     | [`str`](../std/str.md) | Foreground color name                             |
| `bg`     | [`str`](../std/str.md) | Background color name                             |
| `attrs`  | array of `str`         | Text attributes                                   |
| `alt`    | dict                   | Alt style for unfilled bar portion (bar only)     |

The `alt` sub-dict accepts the same `fg`, `bg`, and `attrs` properties. The
default alt style for `bar` is `fg: "blue"`.

**Color names:** `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
`white`. Append `:bright` for bright variants (e.g. `cyan:bright`,
`green:bright`). Use `bright` alone to brighten the default color without
changing it.

**Attributes:** `bold`, `dim`, `italic`, `underlined`, `blink`, `reverse`,
`hidden`, `strikethrough`

All keys are optional; omitted values use defaults.

**Returns:** The return value of `func`

```
progress.with do
  progress.show total: 100 message: "downloading" do |w|
    for i = range 100
      w.delta()

# With custom style
progress.with
  style:
    bar:
      width: 30
      fg: green
      alt:
        fg: black
        attrs:
          - dim
    message:
      fg: white
      attrs:
        - bold
    position:
      fg: yellow
  do progress.show message: "working" do |w|
    # ...
```

### `show func`

Creates a progress indicator and runs `func` with it. The indicator is
automatically removed when `func` returns. When called inside another indicator
scope, the new indicator appears indented beneath the parent widget.

If `total` is provided, the indicator starts in bar mode (showing a progress
bar). Otherwise, it starts in spinner mode. The mode can be changed dynamically
by setting `total` on the indicator.

Outside a `progress.with` scope, the callback is invoked with a dummy indicator
whose methods are silent no-ops.

| Name      | Type                                            | Description                                         |
| --------- | ----------------------------------------------- | --------------------------------------------------- |
| `func`    | func                                            | Callback receiving an [`Indicator`](./indicator.md) |
| `total`   | [`int`](../std/int.md)?                         | Total value for bar mode                            |
| `message` | [`str`](../std/str.md)?                         | Initial message                                     |
| `icon`    | [`str`](../std/str.md)?                         | Prefix icon, e.g. "📦"                              |
| `units`   | [`sym`](../std/sym.md)\|[`str`](../std/str.md)? | Unit format                                         |
| `tick`    | [`float`](../std/float.md)?                     | Tick interval in seconds (default 0.08)             |

**Units:**

| Value     | Description                     |
| --------- | ------------------------------- |
| `:count:` | Display as `pos/len` or `pos`   |
| `:bytes:` | Display as human-readable bytes |

When `total` is provided and `units` is omitted, units default to `:count:`.
When neither `total` nor `units` is provided, spinner mode shows only elapsed
time.

**Returns:** The return value of `func`

```
progress.with do
  progress.show total: 3 message: "building" do |w|
    progress.show message: "step 1" do |_|
      do_step_1()
    w.delta()
```

!!! warning
    The [`Indicator`](./indicator.md) object is only valid inside its callback.
    Using it after the callback returns raises a runtime error.
