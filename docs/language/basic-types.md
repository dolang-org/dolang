# Basic Types

Do is dynamically typed. Values carry their type at runtime, and variables can
hold any type.

## Integers (`int`)

128-bit signed integers.

```
let x = 42
let y = -17
echo (x + y)  # 25
```

Integers support arithmetic (`+`, `-`, `*`, `/`, `//`, `%`), bitwise
(`&`, `|`, `^`, `~`, `<<`, `>>`), and comparison operators.

`/` performs floating point division and results in a float. The `//` operator
performs Euclidean division and `%` computes the Euclidean remainder, such that
`x == (x // y) * y + (x % y)`. This means both always yield `int`s as
results.

## Floats (`float`)

64-bit floating point.

```
let pi = 3.14
let sci = 5.5e-2
echo $pi       # 3.14
echo $sci      # 0.055
```

`//` and `%` likewise perform Euclidean division and remainder operations for
floats, meaning that `//` always returns an `int` while `%` returns a `float`.

## Strings (`str`)

Immutable UTF-8 strings. Created with double quotes or as bare literals at
statement level:

```
let quoted = "hello, world"
let bare = hello
echo "They are equal: $(quoted == bare)"
```

Quoted strings support escape sequences (`\n`, `\t`, `\\`, `\"`, `\$`) and
interpolation with `$`:

```
let name = Alice
echo "Hello, $name!"
echo "2 + 2 = $(2 + 2)"
```

See [Expressions](expressions.md) for details on string interpolation behavior.
Strings support a wide variety of [methods](../api/std/str.md) as well.

### Raw Strings

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

### Here Strings

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
let name = Alice
let greeting = |
  Hello, $name!
  You have $(3 + 1) messages.
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

## Binary Strings (`bin`)

Immutable byte sequences that may contain arbitrary (non-UTF-8) data. Created
with a `b"..."` prefix:

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
```

Interpolation works the same way as in regular strings, using `$`:

```
let prefix = b"foo"
let result = b"${prefix}bar"   # b"foobar"
```

Both `str` and `bin` values can be interpolated into a binary string. `str`
values contribute their UTF-8 bytes; `bin` values contribute their raw bytes.

### Comparison with `str`

Binary strings and regular strings are distinct types and are never equal,
even when their byte content matches:

```
assert (!(b"hello" == "hello"))
```

Use `(type v bin)` to test whether a value is a binary string.

## Booleans (`bool`)

`true` and `false`.

```
let flag = true
if flag
  echo yes
```

## Nil (`nil`)

The absence of a value.

```
let x = nil
if (!x)
  echo "x is falsy"
```

## Symbols (`sym`)

Interned identifiers surrounded by colons. Used for unquoted dictionary keys,
key parameters, record fields, ad-hoc enumerated values, etc.

```
let status = :ok:
let mode = :verbose:
```

Symbols can also be created from strings with `sym`:

```
let s = sym "my_symbol"
```

## Data Structures

See [Data Structures](./data-structures.md) for arrays, dictionaries, records,
sets, and tuples.

## Type Checking

Every value has an associated **type object** that represents its type. The
built-in types (`int`, `float`, `str`, `bool`, `sym`, `array`, `dict`, etc.)
each have a corresponding type object available in the standard library.

`type` is the type of types. You can call it to query or test types:

```
# Get the type of a value (returns the type object)
assert_eq (type 42) $int
assert_eq (type "hello") $str
assert_eq (type [1, 2]) $array
assert_eq (type nil) $Nil

# Test if a value is an instance of a type
assert (type 42 int)
assert (type "hello" str)
assert (type nil Nil)
```

User-defined classes work the same way:

```
class Foo
  let x = 0

let f = Foo()
assert_eq (type f) $Foo
assert (type f Foo)
```

See [Classes](./classes.md) for defining your own types.

## Type Conversions

The built-in types can be called as functions to convert values:

```
assert_eq (str 42) "42"
assert_eq (int "42") 42
assert_eq (float 42) 42.0
assert_eq (bool 0) false
assert_eq (bool 1) true
```
