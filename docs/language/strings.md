# Strings

Do has two string types: [`Str`](std.Str), an immutable UTF-8 string,
and [`Bin`](std.Bin), an immutable byte sequence that may hold
arbitrary non-UTF-8 data. Both have several literal forms, which vary in how
much processing they apply to their content:

- [Bare literals](#bare-literals) — statement-level text with no escapes or
  interpolation
- [`"..."`](#quoted-strings) — escapes and interpolation
- [`r"..."`](#raw-strings) — no escapes or interpolation, may span lines
- [`b"..."`](#binary-strings) — a `Bin` rather than a `Str`
- [`t"..."`](#formatted-sequences) — a [`Fmt`](std.Fmt) that keeps
  interpolations and literal text separate and inspectable
- [Here strings](#here-strings) — multi-line, introduced by an indented block
  rather than delimited, with `r` and `t` variants of their own

## Bare Literals

At statement level, most tokens are literal strings without any quoting:

```
echo "Quoted strings" or bare literals
```

See [Commands](commands.md#literal-strings) for what statement level treats as
literal text and [Implicit Concatenation](commands.md#implicit-concatenation)
for how adjacent tokens join.

## Quoted Strings

Double quotes delimit a string with escape processing and interpolation:

```
let name = "Alice"
echo "Hello, $name!"
echo "2 + 2 = $(2 + 2)"
echo "padded: ${count:05d}"
```

### Escaping

| Sequence   | Meaning              |
| ---------- | -------------------- |
| `\n`       | Newline              |
| `\t`       | Tab                  |
| `\\`       | Backslash            |
| `\"`       | Double quote         |
| `\$`       | Literal dollar sign  |
| `\u{NNNN}` | Unicode scalar value |

Unicode escapes contain one to six hexadecimal digits. A single underscore
may separate adjacent digits, as in `\u{1f_642}`. Leading, trailing, and
consecutive underscores are not accepted.

## Interpolation

`$` interpolates a value into supported string forms. Its behavior is more
conservative than in [compact expressions](expressions.md#compact-expressions):

- Simple variable substitution works: `"hello $name"`
- Anything beyond basic variable access must use `$()`: `"result: $(1 + 2)"`

```
let name = "Alice"
let age = 30

echo "Hello, $name!"
echo "$name is $age years old"
echo "In 10 years: $(age + 10)"
echo "Type: $(type name)"
```

In particular, a bare `$name` does not chain: `"$server.uri"` interpolates
`$server` and then appends the literal text `.uri`. Use `$(server.uri)` for
anything with a field access, index, or call in it.

### Formatted Interpolation

`${value:format-spec}` interpolates with formatting options. The value uses
compact-expression syntax; wrap it in parentheses to use a full expression.

```
let count = 42
echo "count: ${count:05d}"
echo "total: ${(subtotal + tax):8.2f}"
```

The specification may be omitted, along with its `:`, to interpolate with no
options: `"${count}"`.

The format specification is:

```
[[fill]align][sign][#][0][width][.precision][conversion]
```

`align` is `<`, `>`, or `^`. A preceding Unicode scalar sets the fill
character. `sign` is `+` or a space, `#` enables alternate formatting, and
`0` selects numeric-aware zero padding. Width and precision are decimal
counts.

Conversions select the representation:

| Conversion | Meaning            |
| ---------- | ------------------ |
| `s`        | string             |
| `?`        | debug              |
| `!`        | verbatim           |
| `x`        | integer (hex)      |
| `o`        | integer (octal)    |
| `b`        | integer (binary)   |
| `d`        | integer (decimal)  |
| `e`        | float (scientific) |
| `f`        | float (fixed)      |

Without an explicit conversion, string conversion is used by default.

Width and precision may use `$name` or `$(expression)` instead of a decimal
count:

```
let width = 8
let precision = 2
echo "${amount:$width.$(precision)f}"
```

## Raw Strings

Raw strings disable escape sequences and interpolation, making them useful for
anything where literal characters such as `$` or `\` must appear frequently:
regular expressions, Windows file paths, etc. Internal newlines are also
permitted.

```
# Simple raw string - no escapes, no interpolation
let pattern = r"^\d+$"
echo $pattern  # ^\d+$

# Raw strings can contain unescaped backslashes
let path = r"C:\Users\Alice\Documents"

# Raw strings don't interpolate
let value = 42
echo r"The value is $value"  # The value is $value
```

To include a double quote inside a raw string, use hashes around the delimiter:

```
let quoted = r#"She said, "Hello!""#
echo $quoted  # She said, "Hello!"
```

The number of `#` characters must match on both sides of the string.

## Here Strings

Here strings are multi-line string literals introduced by `|` (or `|-`). Like
quoted strings, they support `$` interpolation and `\$` escaping, but span
multiple indented lines instead of a pair of delimiters.

```
let doc = |
  Hello,
  world!
echo $doc  # Hello,\nworld!\n
```

The indentation of the first content line establishes the **baseline**. That
many leading spaces are stripped from every subsequent content line. The here
string ends when indentation drops below the baseline.

```
let msg = |
  line one
  line two
# msg == "line one\nline two\n"
```

`|` is **clip mode**: a final newline is appended after the last content line,
matching YAML `|` behavior.

`|-` is **strip mode**: no final newline is added, matching YAML `|-` behavior.

```
let clipped = |
  hello
# clipped == "hello\n"

let stripped = |-
  hello
# stripped == "hello"
```

Blank lines within the content are preserved (with any indentation stripped per
usual):

```
let with_gap = |-
  first

  third
# with_gap == "first\n\nthird"
```

Interpolation works the same way as in quoted strings:

```
let name = "Alice"
let greeting = |
  Hello, $name!
  You have $(3 + 1) messages.
  Total: ${total:8.2f}
```

Use `\$` to suppress interpolation:

```
let literal = |-
  Price: \$42
# literal == "Price: $42"
```

Use `\\` for a literal backslash.

### Raw Here Strings

Prefixing the introducer with `r` disables interpolation and escape processing
entirely, making `r|` and `r|-` the multi-line equivalents of raw strings.
Every character in the content — including `$` and `\` — is taken literally.

```
let script = r|
  #!/bin/bash
  echo $HOME
  echo $'\n'
# script == "#!/bin/bash\necho \$HOME\necho \$'\\n'\n"
```

Strip mode works the same way:

```
let pattern = r|-
  ^\d+\.\d+$
# pattern == "^\d+\.\d+$"
```

All the same indentation rules apply as for regular here strings.

## Binary Strings

Binary strings hold arbitrary bytes and are written with a `b"..."` prefix:

```
let data = b"\x01\x02\x03"
let text = b"hello"
```

### Escapes and Interpolation

Binary strings support the same escape sequences as regular strings,
plus hex byte escapes (`\xNN`):

```
let crlf   = b"\r\n"
let bullet = b"\xe2\x80\xa2"   # UTF-8 encoding of •
let check  = b"\u{2022}"       # The same UTF-8 bytes
```

`\xNN` is only valid inside binary strings; using it in a regular string is a
syntax error. A Unicode escape in a binary string contributes the scalar's
UTF-8 encoding.

Interpolation works the same way as in regular strings, using `$`:

```
let prefix = b"foo"
let result = b"$(prefix)bar"   # b"foobar"
```

Both `Str` and `Bin` values can be interpolated into a binary string. `Str`
values contribute their UTF-8 bytes; `Bin` values contribute their raw bytes.

### Comparison with `Str`

Binary strings and regular strings are distinct types and are never equal,
even when their byte content matches:

```
assert_ne b"hello" "hello"
```

## Formatted Sequences

Prefixing a quoted string or here string introducer with `t` — `t"..."`,
`t|`, `t|-` — produces a [`Fmt`](std.Fmt) instead of a `Str`. The
interpolation syntax is exactly the same; what differs is that the segments
are kept apart rather than concatenated, so a consumer sees each interpolated
value instead of only the text it produced.

```
let name = "Alice"
let seq = t"hello ${name:>8}!"

# Expanding gives what the equivalent `"..."` would have.
assert_eq $seq.format() "hello    Alice!"

# But the interpolated value is still there.
assert_eq $seq.len 3
assert_eq $seq[1].value $name
```

Every interpolation is a [`FmtValue`](std.FmtValue), whether or not
it states a specification, and each records the text it was written as. The
literal text between them is an ordinary `Str`.

A sequence never expands implicitly: `str` and `"$seq"` raise rather than
flattening it, and expansion has to be asked for with
[`format()`](std.Fmt.format). See
[Trust](std.Fmt.trust) — the distinction between literal text and
interpolated values is what a consumer such as a query builder acts on.

The multi-line forms `t|` and `t|-` interpolate as an ordinary here string
does:

```
let name = "Alice"
let doc = t|-
  hello $name

# doc[1].value == "Alice"
```

### Parameters

A `${#...}` introduces an *unbound* interpolation which can be filled later.
`${#0}` names a position, `${#name}` names a key, and both take a specification
like any other formatted interpolation:

```
let stmt = t"select * from t where a = ${#0} and b = ${#name:>8}"
assert_eq $stmt[1].name 0
assert_eq $stmt[3].name :name:
```

`$#0` and `$#name` are the shorthand for a hole with no specification, the way
`$name` is for `${name}`:

```
let stmt = t"select * from t where a = $#0 and c = $#name"
assert_eq $stmt[1].name 0
assert_eq $stmt[3].name :name:
```

A number is a name that happens to be an integer: it is never renumbered, so
`${#0}` means parameter `0` even in a sequence pasted inside another. Filling
holes is [`Fmt.(call)`](std.Fmt.(call)) and
[`Fmt.bind`](std.Fmt.bind); an unfilled one has no
designated rendering, so
[`format()`](std.Fmt.format) raises an error.

```
let stmt = t"select * from t where a = ${#0} and c = ${#name}"

# Call fills every hole at once; bind fills some and returns the rest.
assert_eq $(stmt 1 name: "n").format() "select * from t where a = 1 and c = n"
assert_eq $stmt.bind({name: "n"}).len 4

# `format` takes the same arguments as a call, for a template filled only to
# be expanded.
assert_eq $stmt.format(1, name: "n") "select * from t where a = 1 and c = n"
```

A filled hole becomes a [`FmtValue`](std.FmtValue).

Parameters are valid only in a `t` string.
