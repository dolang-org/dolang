# Vertical Layout

Do supports YAML-like vertical layout for arguments and data structures. This
provides a natural way to format complex commands or function calls or even
define small declarative languages. Vertical layout is started by placing an
indented block in the following contexts:

- In the middle of a command (but not `if`/`while` conditions, or `for`/`bind`
  scrutinees; in these contexts, indentation is reserved for the body of the
  statement form)
- Immediately after `=` in a `let` or assignment statement, or immediately
  after `return` (constructs an array or dictionary)

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

## Bin-packing

A line starting with neither a dash item nor a key item may contain additional
positional arguments separated horizontally according to the usual rules. Key
arguments can't be bin-packed in vertical layout; place these on the first line
of a command or otherwise one per line.

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
  arg3 arg3

# Not allowed
#func
#  key1: val1 key2: val2
```

## Vertical Data

In non-argument contexts, vertical layout defines an array or dictionary:

```
# Vertical array
let items =
  - 1
  - 2
  - 3

# A vertical dictionary will result if at least one key is present
return
  host: localhost
  port: 8080

# Vertical dictionary with implicit integer keys
let config =
  name: Bob
  - false # integer key 0
  - 42 # integer key 1

# Nested
let data =
  name: Example Student
  scores:
    - 95
    - 87
    - 92
  address:
    city: Anytown
    zip: "00000"
```

A dictionary results if any keys are present; otherwise, an array is
constructed.

In a vertical argument context, further indentation will introduce an array or
dictionary as an argument:

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

## Line Items

Items that span the line (after `-` or `key:`) are treated like ordinary
command arguments, except that whitespace is preserved literally instead
being a separator. There is also a special form:

- `$` followed by a space: the remainder of the line is treated as a full
  command statement, with the value being its result

## `for` in Vertical Layout

Within vertical layout, `for` generates variadic items or arguments

```
let doubled =
  for i = [1, 2, 3]
    - (i * 2)
```

## `if` in Vertical Layout

`if` introduced conditional items or arguments in vertical layout:

```
let items =
  - always_here
  if include_extra
    - extra_item
  - also_always_here

let foo =
  if true
    - 1
    - 2
assert_eq $foo [1, 2]

let bar =
  if false
    - 1
    - 2
assert_eq $bar []
```

## Spreading

Use `...` to spread an iterable in vertical context. It must not be preceded by
`-`.

```
let extras = [4, 5, 6]
let all =
  - 1
  - 2
  - 3
  ...extras
assert_eq $all [1, 2, 3, 4, 5, 6]
```

The behavior of the spread depends on the context:

- *Arguments*:
    - An ordinary iterable is spread as positional arguments
    - An iterable yielding key/value pairs is spread as arguments, with integer
      keys starting from 0 and increasing contiguously treated as positional
      argument and symbol keys treated as key arguments; all others cause a
      runtime error
- *Array* (no static keys specified in vertical layout)
    - An iterable is expanded as individual items in place
- *Dictionary* (at least one static key specified in vertical layout)
    - An ordinary iterable is spread as incrementing integer keys
    - An iterable yielding key/value pairs is spread as such, preserving
      ordering and multiplicity
