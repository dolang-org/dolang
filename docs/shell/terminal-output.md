# Terminal Output

The `term` module provides terminal/console interfaces, including terminal
styling.

## Ordinary Output

The shell prelude provides [`echo`](term.echo) and
[`print`](term.print).

```
echo "result: $value"
print "working...\n"
```

These functions always output to `term.output()`. `echo` behaves similarly to
the Unix program or shell builtin, separating its arguments with spaces and
ending with a newline. Its arguments are converted to strings using the
[`std.verbatim`](std.verbatim) coercion, which preserves
the syntactic form of arguments as best as possible. `print` concatenates all
its arguments without spaces, does not append a newline, and uses ordinary `str`
coercion.

Newlines and tabs are preserved, but other C0/C1 controls and terminal escape
sequences are sanitized before being output.

## Capturing the Console

[`term.capture`](term.capture)
and [`term.sub`](term.sub) override
[`term.output()`](term.output) for their duration.

```
let greeting = term.sub do echo "Hello, Alice!"
assert_eq $greeting "Hello, Alice!"
```

## Child Process Output

The main strand's implicit output is set once at startup: to
[`term.default`](term.default) if stdout is a terminal, or to
[`shell.stdout`](shell.Stdout) otherwise. A child process launched
with no `stdout:` override inherits whichever one is current, so it follows
console/terminal interception — `progress` indicators,
[`term.capture`](term.capture) — only
when stdout was a terminal to begin with. An omitted `stderr:` always defaults
to `term.default`.

## Styled Text

[`term.text`](term.text) returns
[`term.Text`](term.Text), which can contain terminal styling:

```
import term

let warning = term.text WARNING fg: :YELLOW: bold: true
echo $warning "disk space is low"
```

`Text` values may be nested in further `text` calls, with outer style
attributes inherited. Nesting means passing the value as an argument:
interpolating it into a string converts it with `str` first, which drops the
styling.

```
let key = term.text important bold: true
let message = term.text "check " $key " now" fg: :YELLOW:
echo $message
```

Coercing `Text` with `str` returns its content with the styling dropped;
[`encode`](term.Text.encode) returns the ANSI representation.
Passing it directly to `echo` or `print` displays it with its styling.

A reusable style with no text of its own is a
[`term.Style`](term.Style):

```
let warning = term.Style fg: :YELLOW: bold: true
echo $warning("disk space is low")
```

## Existing ANSI Output

Use [`term.preformat`](term.preformat) for strings that
already contain ANSI SGR styling:

```
let rendered = term.preformat $compiler_output
echo $rendered
```

`preformat` validates and canonicalizes SGR styling. Other terminal controls,
including hyperlinks, are removed.

## Styling Control

`echo` and `print` style their output when the console they are writing to says
it can. That answer is the console's
[`can_style`](term.Console.can_style) — a property of the destination,
not a global.

For the host console, it is the process-wide policy:

1. If `DOLANG_CONSOLE` sets `style`, that value wins outright — `FORCE_COLOR`
   and `NO_COLOR` are not consulted at all. See
   [Overriding Console Behavior](#overriding-console-behavior).
2. Otherwise, if `FORCE_COLOR` is set, any value except `0` enables styling;
   `0` disables it.
3. Otherwise, a non-empty `NO_COLOR` disables styling.
4. Otherwise, styling follows stderr terminal detection.

For a [`capture`](term.capture) it is
`false` unless asked for, which is what keeps a test asserting on `echo`ed text
behaving the same piped and on a developer's terminal:

```
# Plain, on a terminal or not.
assert_eq (term.sub do echo $warning) "warning"

# Unless the styling is the point.
term.sub can_style: true do echo $warning
```

Because it is the *installed* console that is consulted,
`term.console.can_style` still reports the process-wide policy from inside a
capture — naming it pins to the host, the same as for writes.

## Terminal Detection and Dimensions

[`term.console.is_tty`](term.Console.is_tty) is the determinative
test for whether stderr is a real terminal.

[`term.console.geometry()`](term.Console.geometry) returns the
terminal's `rows` and `cols`, but is only advisory. It never answers `nil`
itself for the host console; instead `rows` and `cols` are each
independently `nil` when that dimension cannot be determined — a real
terminal that declines to report its size still yields a `Geometry`, just one
with both fields `nil`, so their absence does not imply `is_tty` is `false`.

```
if term.console.is_tty
  let g = term.console.geometry()
  if g.cols
    echo $"─".repeat(g.cols)
```

## Overriding Console Behavior

The `DOLANG_CONSOLE` environment variable gives tests and CI deterministic,
explicit control over what the host console reports, independent of the real
stderr. It is a comma-separated list of `key=value` pairs, all optional:

| Key     | Value          | Overrides                                    |
| ------- | -------------- | -------------------------------------------- |
| `tty`   | `true`/`false` | [`is_tty`](term.Console.is_tty)              |
| `rows`  | integer        | [`geometry()`](term.Console.geometry)'s rows |
| `cols`  | integer        | [`geometry()`](term.Console.geometry)'s cols |
| `style` | `true`/`false` | [`can_style`](term.Console.can_style)        |

`tty` also governs whether an extension can take over the terminal (progress
bars and similar) — `tty=false` disables that even on a real terminal;
`tty=true` allows it even on a pipe, which is useful for capturing an
extension's rendered output deterministically. An unrecognized key or an
unparseable value is ignored rather than raising an error.

```text
DOLANG_CONSOLE=tty=false,cols=120,style=true dolang script.dol
```

`rows` and `cols` are independent: `DOLANG_CONSOLE=cols=120` alone pins
`geometry().cols` to `120` while `geometry().rows` falls back to the real
terminal (or `nil`, on a pipe).
