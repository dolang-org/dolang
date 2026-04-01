# Miscellaneous

## `$` as Low-Precedence Call

The `$` operator serves as a low-precedence, right-associative function call
operator. Everything to the right of `$` is passed as an argument:

```
# These are equivalent:
echo (type (str (range 10)))
echo $ type $ str $ range 10
```

This works in both statement and expression contexts:

```
let ty = (type $ str $ range 10)
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

## String Representations: `str`, `dbg`, and `std.arg`

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

### `std.arg` -- Program Argument Representation

`std.arg` produces a suitable form for passing a value to an external program
as an argument. In particular, the textual forms of constants in Do source
code are reproduced as faithfully as possible. To this end, numeric literals
at statement level remember their verbatim text:

```
def print_arg x
  echo (std.arg x)

# At statement level, numeric literals remember their textual form
print_arg 042   # prints "042"
# Other contexts do not retain this information
print_arg (042) # prints "42"
```

This is also the form used for concatenation or string interpolation in
statement or vertical layout contexts.
