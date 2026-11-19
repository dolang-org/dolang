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

Folding applies when the item's value starts with text, a constant, or a `$`
expression. A sub-list, a `$` command, `$` data, a `do` block, or a here string
doesn't fold. After `- `, a key item's value folds as a key item's does.

A value that starts with a quoted string or a parenthesized, bracketed or braced
expression is that expression alone. Anything after it, joined directly, after
whitespace, or on a continuation line, is an error. Insert `$` before it to make
the whole value text: `- $(count) files`.

## Key Items

A line may instead start with a leading key, in which case it's considered a
key argument which spans the remainder of the line.

```
copy_file
  source: foo.txt
  dest: bar.txt
```

A key's colon must be followed by whitespace or an indented block.

A key item's value continues onto following lines indented past its key, folding
the way a dash item's does. Continuation lines may sit at any deeper column; the
value ends at the first line at or before the key's column.

```
let config = $
  motd: This is a long line of text that
    continues here
  user: admin
# config == {motd: "This is a long line of text that continues here",
#            user: "admin"}
```

## Line Items

Items that span the line (after `-` or `key:`) are treated like ordinary
command arguments, except that whitespace is preserved literally instead being
a separator. Whitespace before a trailing comment is not part of the item. There
is also a special form: `$` followed by a space interprets the remainder of the
line as a full command statement, with the value being its result.

```
some_func
  key1: $ command arg1 arg2
  key2: $ comannd arg3 arg4
```

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

`$` followed by indentation introduces an array, dictionary or record:

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

Keys alone make a dictionary, and positional items alone an array. Both
together make a [record](./data-structures.md#records-record):

```
let task = $
  - build
  help: compile the sources
assert (type task Record)
```

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

In a vertical context, further indentation will introduce an array,
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

Note that bin-packing is not supported in nested items unless explicitly
introduced via `$`.

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

```playground
#> import test:
#>   - assert_eq
let pair = [1, 2]
let items = $
  if let a b = pair
    - $a
    - $b
  else
    - "no pair"
assert_eq $items [1, 2]
```

See [Conditional Matching](./patterns.md#conditional-matching).

## `match` in Vertical Layout

A [`match`](./patterns.md#match) passes the items of the first arm that
matches, or of `else`. An arm's bindings are in scope for its items. Where
bin-packing is allowed, an arm can give a line of bin-packed items after `=>`;
inside [nested items](#nested-data), its items go in an indented block.

```playground
#> import test:
#>   - assert_eq
def flags level
  $
    - --color
    match level
      Int(n) if (n > 1) => --jobs $n
      :quiet:
        - --quiet
        - --no-progress
    else
      - --verbose

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
  - 1
  - 2
  - 3
  ...extras
assert_eq $all [1, 2, 3, 4, 5, 6]
```

Note that a dictionary or record is only produced if at least one static key
exists in addition to any spreads; otherwise, an array results. See
[Spreading](./data-structures.md#spreading) for what each kind of value
contributes.

## Class Supertypes

An empty colon after a class header introduces a vertical supertype list.
Supertypes may be bin-packed or placed one per dash line, and the styles may
be mixed. A dedented `do` ends the list; the class body follows it. There are
no keyed supertype items. Protocols use the same layout. See
[Vertical Supertypes](classes.md#vertical-supertypes).
