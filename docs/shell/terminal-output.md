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

A child process launched without `stdout:` writes to the strand's output. In the
main strand, when stdout is a terminal, that is [`term.default`](term.default),
which forwards to whatever [`term.output()`](term.output) currently is, so the
child's output is captured by [`term.capture`](term.capture) and coordinates
with `progress` indicators. Otherwise it is [`shell.stdout`](shell.Stdout), and
the child writes directly to the process's stdout.

A child process launched without `stderr:` always writes to `term.output()`.

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

`echo` and `print` style their output when the console they write to reports
[`can_style`](term.Console.can_style).

For the host console, this follows a process-wide policy:

1. If `DOLANG_CONSOLE` sets `style`, that value wins outright — `FORCE_COLOR`
   and `NO_COLOR` are not consulted at all. See
   [Overriding Console Behavior](#overriding-console-behavior).
2. Otherwise, if `FORCE_COLOR` is set, any value except `0` enables styling;
   `0` disables it.
3. Otherwise, a non-empty `NO_COLOR` disables styling.
4. Otherwise, styling follows stderr terminal detection.

Text captured with [`term.sub`](term.sub) is unstyled unless `can_style: true`
is passed, so it is the same whether or not the script runs on a terminal:

```
# Plain, on a terminal or not.
assert_eq (term.sub do echo $warning) "warning"

# Unless the styling is the point.
term.sub can_style: true do echo $warning
```

[`term.console`](term.console) always refers to the host console, so
`term.console.can_style` reports the process-wide policy even inside a capture.
Use `term.output().can_style` for the console that `echo` writes to.

## Terminal Detection and Dimensions

[`term.console.is_tty`](term.Console.is_tty) reports whether the host console
is usable as a terminal. It is `false` while an
extension such as `progress` has taken the terminal over, during which
`geometry()` still reports the real terminal's size.

[`term.console.geometry()`](term.Console.geometry) returns the
terminal's `rows` and `cols`, but is only advisory: each is independently `nil`
when that dimension cannot be determined. A real terminal that declines to
report its size has both `nil`, so their absence does not imply `is_tty` is
`false`.

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
