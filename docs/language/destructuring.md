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

Destructure dictionaries and similar key/value structures with keyword
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

## `bind`

`bind` is similar to `let` but takes the scrutinee (the value to destructure)
first and provides the destructuring pattern in vertical layout. This is
useful when the pattern is more complex than what you're destructuring. It also
supports default values for missing elements:

```
bind {1, foo: false, 2, bar: nil}
  - a
  - b
  :foo
  :bar
assert_eq $a 1
assert_eq $b 2
assert_eq $foo false
assert_eq $bar nil
```

### Default Values in `bind`

Positional defaults:

```
bind []
  - a = 1
  - b = 2
assert_eq $a 1
assert_eq $b 2

bind [false]
  - a = 1
  - b = 2
assert_eq $a false
assert_eq $b 2
```

Keyword defaults:

```
bind {}
  :foo = 42
assert_eq $foo 42

bind {foo: nil}
  :foo = 42
assert_eq $foo nil  # nil is a present value, not missing
```

## Destructuring in `for`

Destructure elements during iteration:

```
for k v = {name: "Alice", age: 30}
  echo "$k: $v"

for index value = [10, 20, 30].pairs()
  echo "$index: $value"

for :name :age = [{name: "Alice", age: 30}, {name: "Bob", age: 44}]
  echo "$name is $age years old"
```
