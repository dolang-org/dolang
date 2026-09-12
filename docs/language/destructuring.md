# Destructuring

Do supports destructuring data in `let`, `bind`, and `for`.

## `let` Destructuring

Destructure arrays and similar sequences by listing multiple names:

```
let a b = [1, 2]
assert_eq $a 1
assert_eq $b 2
```

By default, the pattern must exhaustively match the entire structure or an
error will result. Use `...` to capture surplus items instead. The specified
variable will be bound to an iterator over them.

```
let first ...rest = [1, 2, 3, 4]
assert_eq $first 1
```

Specify nothing after `...` to simply ignore surplus items:

```
let first ... = [1, 2, 3, 4]
```

Destructure dictionaries and similar key/value structures with key
patterns:

```
let :name age: years = {name: "Alice", age: 30}
assert_eq $name "Alice"
assert_eq $years 30
```

Mixed positional/key destructuring is also possible, with the semantics
depending on the structure. For dictionaries, positional patterns bind
incrementing integer keys:

```
let first :foo = {foo: 42, "ultramarine"}
assert_eq $first "ultramarine"
assert_eq $foo 42
```

### Non-symbol Keys

`:key` and `key: name` match **symbol** keys. External inputs such as decoded
JSON will typically have string keys. To destructure such data, use a constant
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

`bind` is similar to `let` but takes the scrutinee (the value to destructure)
first and provides the destructuring pattern in vertical layout. This is
useful when the pattern is more complex than what you're destructuring.

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

## Conditional Destructuring

`let` and `bind` after `if` or `while` make the destructuring itself the
condition: the bindings are in scope for the branch body when the pattern
matches, and the else branch runs when it does not.

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

Only a *shape* mismatch takes the else branch: too few or too many
positional elements, or a missing or unexpected key. Any other error raised
while destructuring propagates as usual. In particular, destructuring a value
that does not support it at all is an error rather than a silent non-match:

```
# Branches: [1, 2] unpacks fine, but not into three elements
if let a b c = [1, 2]
  echo unreachable
else
  echo "wrong arity"

# Raises: an int cannot be destructured at all
if let a b = 42
  echo unreachable
else
  echo also-unreachable
```

A default supplies a missing element, so it turns what would otherwise be a
mismatch into a match.

### Binding a Single Name

A pattern that is a bare identifier binds the scrutinee itself and branches on
its truthiness rather than destructuring it:

```
if let value = lookup key
  echo "found $value"
else
  echo "not found"
```

## Destructuring in `for`

Destructure elements during iteration:

```playground
for k v = {name: "Alice", age: 30}
  echo "$k: $v"

for index value = [10, 20, 30].pairs()
  echo "$index: $value"

for :name :age = [{name: "Alice", age: 30}, {name: "Bob", age: 44}]
  echo "$name is $age years old"
```
