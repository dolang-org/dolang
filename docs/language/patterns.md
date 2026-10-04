# Pattern Matching

Patterns bind names, unpack sequences and keyed values, and test constants or
runtime types. Use them in `let`, `bind`, `for`, and lambda parameters. With
`if let`, `if bind`, or their `while` forms, a pattern match controls whether
the body runs.

## Binding and Unpacking

`let` matches arrays and similar sequences by listing multiple names:

```
let a b = [1, 2]
assert_eq $a 1
assert_eq $b 2
```

By default, the pattern must exhaustively match the entire structure or an
error will result. Use `...` to capture surplus items instead. The specified
variable receives a structural rest that can be unpacked or spread again.
Unpacking reads the source without advancing an iterator. In-memory iterators
such as array, tuple, pair, view, split, and dict key/value iterators support
this read-only operation; generic `map` and `filter` iterators do not.

```
let first ...rest = [1, 2, 3, 4]
assert_eq $first 1
assert_eq [...rest] [2, 3, 4]
```

Specify nothing after `...` to simply ignore surplus items:

```
let first ... = [1, 2, 3, 4]
```

Match dictionaries and similar keyed values with key patterns:

```
let :name age: years = {name: "Alice", age: 30}
assert_eq $name "Alice"
assert_eq $years 30
```

A pattern can mix positional and keyed items; how they match depends on the
source. For dictionaries, positional patterns bind
incrementing integer keys:

```
let first :foo = {foo: 42, "ultramarine"}
assert_eq $first "ultramarine"
assert_eq $foo 42
```

### Positional and Key Rests

`*` captures only surplus positional items, and `**` only surplus key items.
Either or both may end a pattern, `*` first, in place of `...`. Surplus items
of a kind that neither takes are an error, as without a rest:

```
let first *others **options = {1, 2, 3, color: "red"}
assert_eq $first 1
assert_eq [...others] [2, 3]
assert_eq {...options} {color: "red"}
```

`*` or `**` alone discards surplus items entirely.

Source-backed rests are neither iterators nor iterable. Materialize a positional
rest with `[...rest]` before iterating. Materialize a keyed or mixed rest with
`Dict(rest)` or `Record(rest)`, then iterate the resulting container. A rest can
be unpacked and spread repeatedly; these operations do not consume it.

### Non-symbol Keys

`:key` and `key: name` match **symbol** keys. External inputs such as decoded
JSON will typically have string keys. To match such keys, use a constant
expression instead of a bare key:

```playground
#> import test:
#>   - assert_eq
import json

let payload = json.decode r|
  {"name": "Alice", "age": 30}

let "name": name "age": age = payload
assert_eq $name "Alice"
assert_eq $age 30
```

## `bind`

`bind` takes the scrutinee (the value to match) first, followed by the pattern
in vertical layout. Use it to lay out longer patterns across several lines.

```playground
#> import test:
#>   - assert_eq
bind {1, foo: false, 2, bar: nil}
  a b
  :foo :bar
assert_eq $a 1
assert_eq $b 2
assert_eq $foo false
assert_eq $bar nil
```

### Default Values in `bind`

`bind` also permits specifying default values for missing items:

```playground
#> import test:
#>   - assert_eq
bind []
  a = 1
  b = 2
assert_eq $a 1
assert_eq $b 2

bind [false]
  a = 1
  b = 2
assert_eq $a false
assert_eq $b 2

bind {}
  :foo = 42
assert_eq $foo 42

bind {foo: nil}
  :foo = 42
# nil is a present value, not missing
assert_eq $foo nil
```

## Nested Patterns

An item in parentheses matches its value against a pattern of its own:

```playground
#> import test:
#>   - assert_eq
let a (b c) = [1, [2, 3]]
assert_eq [a, b, c] [1, 2, 3]

let :name address: (:city :zip) = $
  name: Alice
  address:
    city: Springfield
    zip: 12345
assert_eq $city Springfield
```

A space must separate the parentheses from a preceding name. Within the
parentheses, items read as in a parameter list, so they may have defaults, and
newlines are only whitespace:

```
let id (
  host
  port = 80
) = ["web", ["example.com"]]
```

A parenthesized pattern always unpacks, even with one item. `let (x) = [1]`
binds the item, where `let x = [1]` binds the array itself. An empty `()`
matches only an empty value, so `if let () = items` tests for one.

In vertical layout, an indented block after a key nests the same way:

```playground
#> import test:
#>   - assert_eq
bind {status: 200, headers: {"content-type": "text/plain"}}
  :status
  headers:
    "content-type": type
assert_eq $type "text/plain"
```

Annotations go on the names a sub-pattern binds, never on the sub-pattern, and a
sub-pattern has no default of its own. Lambda parameters accept sub-patterns
too:

```
let add = (do |(x1 y1) (x2 y2)| [x1 + x2, y1 + y2])
```

A `def`'s parameters must be names, which its signature can annotate. Unpack
them in the body instead.

## Constant Patterns

A constant in a pattern matches an equal value using `==` and binds no name.
A bare word still binds a name; quote a string to match it.

```
if let "GET" path = request
  echo $path

if bind response
  status: 200
  :body
do
  echo $body
```

A lone constant tests the whole value. Parentheses require a sequence with the
listed items. Constants also nest in sub-patterns and runtime type tests, such
as `Array("GET" path)`.

`false` and `nil` match equal values in conditional patterns; the truthiness
test for a lone name does not apply to constants.

A mismatch takes a conditional pattern's failure branch. In a plain `let`,
`bind`, or `for`, it raises `std.TypeError`. Constants cannot have annotations
or defaults, and cannot be `def` parameters. Interpolated strings and other
expressions that cannot be folded to constants are rejected.

## Type-Test Patterns

`C(pattern)` tests whether a value is an instance of `C`, then matches the
inner pattern against the same value. The test runs before any unpacking:

```
let Int(n) = 42
let Int(x) Str(label) = [1, "first"]
let Point(x: px y: py) = point
```

A single required positional binding captures the whole value: `Int(n)` binds
an integer without unpacking it. To unpack one element explicitly, use
`Array((item))`. `Array()` tests for an empty array.

The parentheses must touch the class name. `Int(n)` is a type test, while
`Int (n)` binds `Int` and unpacks another value into `n`. The class may be a
dotted runtime name such as `std.Int`; it takes no type arguments. Type-only
imports, aliases, and protocols cannot supply a runtime class. The class cannot
be bound by the same pattern or parameter list, as in `let C C(x) = ...`; a
later pattern may use it.

In vertical patterns, `C $` introduces the inner pattern as an indented block:

```
bind point
  Point $
    x: px
    y: py
```

This form works in `bind` and nested vertical patterns. Use `C(pattern)` in
horizontal bindings. Both forms accept annotations and defaults on inner
bindings, but neither accepts an annotation or default on the whole type-test
pattern. `def` parameters accept neither form.

A failed class test raises `TypeError` in an ordinary binding or lambda
parameter. In a conditional binding it takes the failure branch. A successful
class test can capture a falsy value: `if let Bool(value) = false` succeeds and
binds `false`. A subsequent inner pattern can still fail independently.

## Conditional Matching

`let` and `bind` after `if` or `while` make the pattern match the condition.
The bindings are in scope for the branch body when the pattern matches, and
the else branch runs when it does not.

```
if let a b = [1, 2]
  echo "matched $a $b"
else
  echo "no match"
```

`bind` takes the same vertical layout as its statement form, with `do`
introducing the branch body. Because the pattern is vertical, it also supports
default values:

```
if bind response
  :status
  :body = ""
do
  echo "$status $body"
else
  echo "unexpected shape"
```

Both forms work with `while`, in which case the loop ends the first time the
pattern fails to match:

```
let i = 0
while let a b = pairs.get(i)
  echo "$a $b"
  i = (i + 1)
```

Both forms also work where `if` appears in
[vertical layout](./vertical-layout.md), building arrays, dictionaries, or
argument lists:

```
let parts = $
  - always
  if let a b = pair
    - $a
    - $b
  else
    - "no pair"
```

### What Counts as a Match

A pattern takes the failure branch when an unpack has too few or too many
positional items, a missing or unexpected key, or a type test or constant
comparison fails. Other errors propagate as usual. In particular, trying to
unpack a value that does not support it is an error:

```
# Branches: [1, 2] unpacks fine, but not into three elements
if let a b c = [1, 2]
  echo unreachable
else
  echo "wrong arity"

# Raises: an int cannot be unpacked
if let a b = 42
  echo unreachable
else
  echo also-unreachable
```

A default supplies a missing element, so it turns what would otherwise be a
mismatch into a match.

### Binding a Single Name

A pattern that is a bare identifier binds the scrutinee itself and branches on
its truthiness without unpacking it:

```
if let value = lookup key
  echo "found $value"
else
  echo "not found"
```

To match a sequence of exactly one item instead, write the item in parentheses:
`if let (value) = items`.

A nested pattern matches only if every level does.

## Patterns in `for`

`for` matches each element against its pattern:

```playground
for k v = {name: "Alice", age: 30}
  echo "$k: $v"

for index value = [10, 20, 30].pairs()
  echo "$index: $value"

for :name :age = [{name: "Alice", age: 30}, {name: "Bob", age: 44}]
  echo "$name is $age years old"
```
