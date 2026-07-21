# Terminal Output

The `term` module separates ordinary text from trusted terminal presentation.
This prevents values containing escape sequences or control characters from
changing terminal state unexpectedly.

## Ordinary Output

The shell prelude provides [`echo`](../api/term/index.md#echo-args) and
[`print`](../api/term/index.md#print-options-args).

```
echo "result: $value"
print "working...\n"
```

These functions always output to stderr regardless of strand input/output state.
`echo` behaves similarly to the Unix program or shell builtin, separating its
arguments with spaces and ending with a newline. Its arguments are converted to
strings using the [`std.arg`](../api/std/index.md#arg-value) coercion, which
preserves the syntactic form of arguments as best as possible. `print`
concatenates all its arguments without spaces, does not append a newline, and
uses ordinary `str` coercion.

Newlines and tabs are preserved, but other C0/C1 controls and terminal escape
sequences are sanitized before being output.

## Styled Text

[`term.style`](../api/term/index.md#style-options-args) returns
[`term.Text`](../api/term/text.md), which can contain terminal styling:

```
import term

let warning = term.style WARNING fg: :YELLOW: bold: true
echo $warning "disk space is low"
```

`Text` values may be nested in further `style` calls, with outer style
attributes inherited.

```
let key = term.style important bold: true
let message = term.style "check $key now" fg: :YELLOW:
echo $message
```

Coercing `Text` with `str` returns its ANSI representation. Passing it
directly to `echo` or `print` displays it with its styling.

## Existing ANSI Output

Use [`term.preformat`](../api/term/index.md#preformat-text) for strings that
already contain ANSI SGR styling:

```
let rendered = term.preformat $compiler_output
echo $rendered
```

`preformat` validates and canonicalizes SGR styling. Other terminal controls,
including hyperlinks, are removed.

## Styling Control

Styling is enabled when stderr is a terminal at process startup, otherwise
`Text` renders without it. Environment variables override terminal detection:

1. If `FORCE_COLOR` is set, any value except `0` enables styling; `0` disables
   it.
2. Otherwise, a non-empty `NO_COLOR` disables styling.
3. Otherwise, styling follows stderr terminal detection.

`term.have_terminal` reports whether stderr was a terminal; it does not include
the environment-variable override.

## Raw Output

Output to the default stdout sink using `strand.put` is not sanitized, but
follows the current I/O mode. See
[`proc.io_mode`](../api/proc/index.md#io_mode-mode-func).
