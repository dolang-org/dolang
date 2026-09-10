# Vertical Layout

Do supports YAML-like vertical layout for arguments and data structures. This
provides a natural way to format complex commands or function calls, or even
define small declarative languages. Vertical layout is started by placing an
indented block in the following contexts:

- In the middle of a command (but not `if`/`while` conditions, or `for`/`bind`
  scrutinees; in these contexts, indentation is reserved for the body of the
  statement form)
- After a `$` at statement level

## Dash Items

A line in vertical layout may start with a leading `-`, in which case it's
considered a positional argument which spans the remainder of the line.

```
compile_sources
  - foo.c
  - bar.c
  - file name with spaces.c
```

## Key Items

A line may instead start with a leading key, in which case it's considered a
key argument which spans the remainder of the line.

```
copy_file
  source: foo.txt
  dest: bar.txt
```

A key's colon must be followed by whitespace or an indented block.

## Bin-packing

Positional arguments may continue to be laid out horizontally in the vertical
continuation of a command. The line must not start with a dash item or a key.
Key arguments can't be bin-packed in vertical layout; place these on the first
line of a command or otherwise one per line.

```
# Horizontal
func arg1 arg2 key1: val1 key2: val2

# Vertical
func
  arg1
  arg2
  key1: val1
  key2: val2

# Mixed
func arg1 key1: val1
  arg2
  key2: val2

# Bin packed
func
  arg1 arg2
  arg3 arg4

# Not allowed
#func
#  key1: val1 key2: val2
```

## Vertical Data

`$` followed by indentation introduces an array or dictionary:

```
# Vertical array
let items = $
  - 1
  - 2
  - 3

# A vertical dictionary will result if at least one key is present
return $
  host: localhost
  port: 8080

# As an argument (a single array argument, instead of 3 string arguments)
echo $
  - a
  - b
  - c
```

A dictionary results if any keys are present; otherwise, an array is
constructed.

As with command arguments, vertical data introduced by `$` may contain
bin-packed positional items.

```
# Produces an array
let command = $
  gcc
  -Wall -Werror
  -o $output
  $input
```

## Nested Data

In a vertical context, further indentation will introduce an array or
dictionary as an item:

```
my_func
  # Positional argument: 2-item dictionary
  - name: Bob
    age: 45
  # Key argument: 3-element array
  some_key:
    - 1
    - a string
    - false
  # Key argument: 2-item dictionary
  dict:
    x: 12
    y: 34
```

Note that bin-packing is not supported in nested items unless explicitly
introduced via `$`.

## Line Items

Items that span the line (after `-` or `key:`) are treated like ordinary
command arguments, except that whitespace is preserved literally instead being
a separator. There is also a special form: `$` followed by a space interprets
the remainder of the line as a full command statement, with the value being its
result.

```
some_func
  key1: $ command arg1 arg2
  key2: $ comannd arg3 arg4
```

## `for` in Vertical Layout

Within vertical layout, `for` generates variadic items or arguments

```
let doubled = $
  for i = [1, 2, 3]
    - (i * 2)
```

## `if` in Vertical Layout

`if` introduces conditional items or arguments in vertical layout:

```
let items = $
  - always_here
  if include_extra
    - extra_item
  - also_always_here

let foo = $
  if true
    - 1
    - 2
assert_eq $foo [1, 2]

let bar = $
  if false
    - 1
    - 2
assert_eq $bar []
```

`if let` and `if bind` work here too, and their bindings are in scope for the
items of the matching branch:

```
let pair = [1, 2]
let items = $
  if let a b = pair
    - $a
    - $b
  else
    - "no pair"
assert_eq $items [1, 2]
```

See [Conditional Destructuring](./destructuring.md#conditional-destructuring).

## Spreading

Use `...` to spread in vertical layout. It must not be preceded by `-`.

```
let extras = [4, 5, 6]
let all = $
  - 1
  - 2
  - 3
  ...extras
assert_eq $all [1, 2, 3, 4, 5, 6]
```

The behavior of the spread depends on the context:

| Context    | Spread input                                       | Behavior                                                                                       |
| ---------- | -------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| Arguments  | [`Iterable`](std.Iterable)                         | Positional arguments                                                                           |
| Arguments  | Dict-like object or [`Iter`](std.Iter) from `kv()` | Mixed arguments. Monotonic integer keys starting from `0` are positional, symbol keys are keys |
| Array      | [`Iterable`](std.Iterable)                         | Expanded as individual items in place                                                          |
| Dictionary | [`Iterable`](std.Iterable)                         | Items assigned incrementing integer keys                                                       |
| Dictionary | Dict-like object or [`Iter`](std.Iter) from `kv()` | Key/value pairs, preserving ordering and multiplicity                                          |

Note that a dictionary is only produced if at least one static key exists in
addition to any spreads; otherwise, an array results.
