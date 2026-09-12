# Miscellaneous

## `$` as Low-Precedence Call

The `$` operator serves as a low-precedence, right-associative function call
operator. Everything to the right of `$` is passed as an argument:

```
# These are equivalent:
echo (type (str (Range 10)))
echo $ type $ str $ Range 10
```

This works in both statement and expression contexts:

```
let ty = (type $ str $ Range 10)
```

### Forcing Vertical Layout as a Single Argument

Normally, trailing vertical layout after a call is interpreted as multiple
arguments. Use `$` to force it to be parsed as a single array or dict literal
argument:

```
# Without $: three separate arguments
func
  - 1
  - 2
  - 3

# With $: single array argument
func $
  - 1
  - 2
  - 3
```

## String Representations: `Str`, `dbg`, and `std.verbatim`

Do has three builtins for converting values to strings, each serving a
different purpose:

### `str` -- Human-Readable

`str` produces a plain, human-readable string. It is the general-purpose
conversion used by string interpolation (`"$value"` is equivalent to
`"$(str value)"`):

```
assert_eq (str 42) "42"
assert_eq (str "hello") "hello"
assert_eq (str true) "true"
assert_eq (str nil) "nil"
```

### `dbg` -- Debug Representation

`dbg` produces a representation that shows internal structure. Strings are
quoted, type tags are visible, and the output is intended for debugging rather
than display:

```
assert_eq (dbg "hello") "\"hello\""
assert_eq (dbg 42) "42"
assert_eq (dbg :foo:) "<sym foo>"
```

### `std.verbatim` -- Verbatim Representation

`std.verbatim` produces a value's verbatim form. This is suitable for passing
a value to an external program, and preserves the textual forms of constants
in Do source code where possible. Numeric literals in unevaluated contexts
remember their verbatim text:

```playground
def print_verbatim x
  echo (std.verbatim x)

# Statement and vertical-layout numeric literals remember their textual form.
print_verbatim 042   # prints "042"
print_verbatim (042) # prints "42"
```

This is also the form used for implicit concatenation in statement and
vertical-layout contexts. Interpolation inside ordinary quoted strings and
here strings uses `str` conversion instead.
