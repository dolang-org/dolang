# Vertical Layout

Do supports YAML-like vertical layout for arguments and data structures. This
provides a natural way to format complex commands or function calls, or even
define small declarative languages. An indented block starts vertical layout in
these contexts:

- In the middle of a command (but not `if`/`while` conditions, or `for`/`bind`
  scrutinees; in these contexts, indentation is reserved for the body of the
  statement form)
- After a `$` at statement level

## Dash Items

A line that starts with `-` is a positional argument spanning the rest of the
line:

```
compile_sources
  - foo.c
  - bar.c
  - file name with spaces.c
```

## Key Items

A line that starts with a key is a key argument spanning the rest of the line:

```
copy_file
  source: foo.txt
  dest: bar.txt
```

A key's colon must be followed by whitespace or an indented block.

A key later on the same line is text in the value, not another key argument:
`key: x other: y` passes `key: "x other: y"`. Put each key on its own line.

## Line Items

Dash and key items are line items. A line item is parsed like an ordinary
command argument, except that whitespace within it is preserved literally
instead of separating arguments. Whitespace before a trailing comment is not
part of the item.

`$` followed by a space makes the rest of the line a full command statement,
and the item's value is its result:

```
some_func
  key1: $ command arg1 arg2
  key2: $ command arg3 arg4
```

## Bin-packing

Lines in the vertical continuation of a command may hold several positional
arguments, laid out horizontally. Such a line must not start with `-` or a key.
Key arguments can't be bin-packed; place them on the first line of the command
or one per line.

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

`$` followed by an indented block produces an array, dictionary or record.
Positional items alone make an array, keys alone a dictionary, and both together
a [record](./data-structures.md#records-record):

```
# Array
let items = $
  - 1
  - 2
  - 3

# Dictionary
return $
  host: localhost
  port: 8080

# Record
let task = $
  - build
  help: compile the sources
assert (type task Record)

# As an argument (a single array argument, instead of 3 string arguments)
echo $
  a b c
```

As with command arguments, vertical data may contain bin-packed positional
items:

```
# Produces an array
let command = $
  gcc
  -Wall -Werror
  -o $output
  $input
```

## Nested Data

Within vertical layout, a further indented block introduces an array,
dictionary or record as an item:

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

Nested items can't be bin-packed unless the nested block is introduced with `$`.

## Continuation Lines

A dash item continues onto following lines that start at its content column,
the column after `- `. Each line break between them folds into a space, as in a
YAML plain scalar. Blank lines fold into one newline each instead, and comment
lines are skipped. The item ends at the first line indented less than its
content column; a line indented further is an error.

```
let banner = $
  - This is a long line of text that
    continues here

  - First paragraph.

    Second paragraph.
# banner == ["This is a long line of text that continues here",
#            "First paragraph.\nSecond paragraph."]
```

A key item's value continues onto following lines indented past its key,
folding the same way. Continuation lines may sit at any deeper column; the value
ends at the first line at or before the key's column. A key item after `-`
follows the key item rule.

```
let config = $
  motd: This is a long line of text that
    continues here
  user: admin
# config == {motd: "This is a long line of text that continues here",
#            user: "admin"}
```

Folding applies when the item's value starts with text, a constant, or a `$`
expression. Nested data, a `$` command, `$` data, a `do` block, or a here
string doesn't fold.

A value that starts with a quoted string or a parenthesized, bracketed or braced
expression is that expression alone. Anything after it, joined directly, after
whitespace, or on a continuation line, is an error. Insert `$` before it to make
the whole value text: `- $(count) files`.

## `for` in Vertical Layout

Within vertical layout, `for` generates items or arguments from each iteration:

```
let doubled = $
  for i = [1, 2, 3]
    (i * 2)
```

Literals take the same elements on one line, as in `[for i = xs => i * 2]`;
see [Comprehensions](data-structures.md#comprehensions).

Where bin-packing is allowed, `=>` after the iteratee takes a line of bin-packed
items, as it does [after an `if` condition](#if-in-vertical-layout):

```playground
#> import test:
#>   - assert_eq
let pairs = $
  for i = [1, 2] => $i (i * 10)
assert_eq $pairs [1, 10, 2, 20]
```

`=>` can also take a nested `for` or `if`, which holds a single element, so it
is allowed even where bin-packing isn't:

```playground
#> import test:
#>   - assert_eq
let config = $
  ports:
    for host = [1, 2] => for offset = [0, 1]
      - (8000 + host * 10 + offset)
assert_eq $config[:ports:] [8010, 8011, 8020, 8021]
```

## `if` in Vertical Layout

`if` introduces conditional items or arguments in vertical layout:

```
let items = $
  always_here
  if include_extra
    extra_item
  also_always_here

let foo = $
  if true
    1 2
assert_eq $foo [1, 2]

let bar = $
  if false
    1 2
assert_eq $bar []
```

`if let` and `if bind` work here too, and their bindings are in scope for the
items of the matching branch:

```playground
#> import test:
#>   - assert_eq
let pair = [1, 2]
let items = $
  if let a b = pair
    $a $b
  else
    "no pair"
assert_eq $items [1, 2]
```

See [Conditional Matching](./patterns.md#conditional-matching).

Where bin-packing is allowed, `=>` after a condition and `else` each take a
line of bin-packed items, as a [`match`](#match-in-vertical-layout) arm does:

```playground
#> import test:
#>   - assert_eq
let verbose = false
let args = $
  if verbose => --verbose --trace
  else --quiet
assert_eq $args ["--quiet"]
```

## `match` in Vertical Layout

A [`match`](./patterns.md#match) passes the items of the first arm that
matches, or of `else`. An arm's bindings are in scope for its items. Where
bin-packing is allowed, an arm can give a line of bin-packed items after `=>`,
and `else` can give one directly; inside [nested items](#nested-data), its items
go in an indented block.

```playground
#> import test:
#>   - assert_eq
def flags level
  $
    --color
    match level
      Int(n) if (n > 1) => --jobs $n
      :quiet: => --quiet --no-progress
    else --verbose

assert_eq $flags(4) ["--color", "--jobs", 4]
assert_eq $flags(:quiet:) ["--color", "--quiet", "--no-progress"]
assert_eq $flags(nil) ["--color", "--verbose"]
```

## Spreading

Use `...` to spread in vertical layout. It must not be preceded by `-`.

```playground
#> import test:
#>   - assert_eq
let extras = [4, 5, 6]
let all = $
  1 2 3
  ...extras
assert_eq $all [1, 2, 3, 4, 5, 6]
```

Spreads alone don't make a dictionary or record: at least one static key is
needed, or an array results. See [Spreading](./data-structures.md#spreading) for
what each kind of value contributes.
