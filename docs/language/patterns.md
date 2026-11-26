# Pattern Matching

Patterns bind names, unpack sequences and keyed values, test constants or
runtime types, and try alternatives. They appear in `let`, `bind`, `for`, and
lambda parameters. With `if let`, `if bind`, or their `while` forms, whether
the pattern matches decides whether the body runs. `match` runs the body of the
first of several patterns that matches.

## Binding and Unpacking

Listing several names in a `let` unpacks an array or other sequence:

```
let a b = [1, 2]
assert_eq $a 1
assert_eq $b 2
```

The pattern must account for every item, or unpacking raises an error. A
`...rest` item collects the surplus, and `...` alone discards it:

```
let first ...rest = [1, 2, 3, 4]
assert_eq $first 1
assert_eq [...rest] [2, 3, 4]

let head ... = [1, 2, 3, 4]
```

Unpacking doesn't consume an iterator, so only an iterator over a fixed
collection can be unpacked. To unpack any other iterator, such as one from
`map`, first call [`buffer`](std.Iter.buffer) on it:

```
let a ...rest = [1, 2, 3].map(do |x| x * 10).buffer()
assert_eq $a 10
assert_eq [...rest] [20, 30]
```

Key items match dictionaries and other keyed values. `:name` binds the value
of key `name` to `name`, and `age: years` binds the value of key `age` to
`years`:

```
let :name age: years = {name: "Alice", age: 30}
assert_eq $name "Alice"
assert_eq $years 30
```

A pattern can mix positional and key items. Against a record, the positional
items match the record's positional items:

```
let first :foo = {foo: 42, "ultramarine"}
assert_eq $first "ultramarine"
assert_eq $foo 42
```

A pattern binds each name at most once, so `let a a = [1, 2]` is an error.
`_` is exempt and may appear any number of times.

### Positional and Key Rests

`...` collects every kind of surplus item. `*` collects only surplus positional
items, and `**` only surplus key items:

```
let first *others **options = {1, 2, 3, color: "red"}
assert_eq $first 1
assert_eq [...others] [2, 3]
assert_eq {...options} {color: "red"}
```

`*` and `**` may each end a pattern, and when both are used, `*` comes first.
Neither can be combined with `...`. A surplus item that no rest collects is an
error, as it is without any rest. A bare `*` or `**` discards its kind of
surplus.

Iterating a rest continues from where unpacking stopped. A `*` rest, or the
rest of an array or other sequence, yields values. A `**` rest, or a `...` rest
of a dictionary, record or `Pack`, yields `(key, value)` tuples, and a record's
or `Pack`'s positional items come keyed by index:

```
let _ *values **options = {1, 2, color: "red"}
for value = values
  assert_eq $value 2
for key value = options
  assert_eq (key, value) (:color:, "red")
```

A rest of a container can be spread, unpacked, or iterated any number of
times. The rest of an iterator is usually an iterator itself, so iterating or
spreading it consumes it.

A rest may be a view into the original container, in which case it sees later
changes to the container, and can sometimes raise `ConcurrencyError`.

## `bind`

`bind` takes the scrutinee (the value to match) on its first line, and the
pattern in vertical layout below it. Use it for a pattern too long for one
line:

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

An item in a `bind` pattern may have a default, which it takes when the value
has no such item:

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

### Patterns on the Header Line

A short pattern can follow `=>` on the `bind` line. There it reads like a
parameter list, so its items take defaults directly, where a `let` pattern
needs [parentheses](#nested-patterns) around them:

```playground
#> import test:
#>   - assert_eq
bind {host: "example.com"} => :host :port = 80
assert_eq $host "example.com"
assert_eq $port 80
```

The line can end with a [`C $` type test](#type-test-patterns), whose pattern
follows as an indented block:

```
bind point => Point $
  x: px
  y: py
```

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

## Nested Patterns

An item in parentheses unpacks its value with a pattern of its own:

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

A space must separate the parentheses from a preceding name. Inside them,
items read as in a parameter list: they may have defaults, and newlines are
only whitespace:

```
let id (
  host
  port = 80
) = ["web", ["example.com"]]
```

Parentheses always unpack, even around one item. `let (x) = [1]` binds the
item, where `let x = [1]` binds the array itself. An empty `()` matches only
an empty value, so `if let () = items` tests whether `items` is empty.

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

So does a dash item with a block of its own:

```
bind [[1, 2]]
  - - first
    - second
```

Each dash is one positional item, so a block of a single dash still unpacks:
`- x` alone matches a one-item sequence, and `- key: x` alone a sequence whose
one item is a keyed value. A single item without a dash matches the whole
value.

Lambda parameters accept nested patterns too:

```
let add = (do |(x1 y1) (x2 y2)| [x1 + x2, y1 + y2])
```

A `def`'s parameters must be plain names, so unpack them in the body instead.
Annotations go on the names inside a nested pattern, not on the pattern, and a
nested pattern has no default of its own.

## Conditional Matching

After `if` or `while`, `let` and `bind` test whether the pattern matches. When
it does, the body runs with the pattern's names in scope. When it doesn't, the
`else` branch runs:

```
if let a b = [1, 2]
  echo "matched $a $b"
else
  echo "no match"
```

`if bind` lays out its pattern vertically, as the `bind` statement does, so its
items can have defaults. `do` on its own line ends the pattern and starts the
body:

```
if bind response
  :status
  :body = ""
do
  echo "$status $body"
else
  echo "unexpected shape"
```

When the pattern follows `=>` on the header line, the body is an indented
block, or follows a second `=>` on the same line:

```
if bind response => :status :body = ""
  echo "$status $body"

if bind entry => key value = "" => echo "$key=$value"
```

If that pattern ends with a `C $` type test, its block is followed by `do` or
`=>`, as a vertical pattern is.

Both forms work with `while`, and the loop ends the first time the pattern
doesn't match:

```
let i = 0
while let a b = pairs.get(i)
  echo "$a $b"
  i = (i + 1)
```

Both also work where `if` appears in
[vertical layout](./vertical-layout.md), building arrays, dictionaries, or
argument lists:

```
let parts = $
  always
  if let a b = pair
    $a $b
  else
    "no pair"
```

### What Counts as a Match

A pattern fails to match, taking the `else` branch, when:

- an unpack finds too few or too many positional items,
- a key is missing or unexpected, or
- a type test or constant comparison fails.

A default fills in a missing item, so an item with a default never fails for
being missing. A nested pattern matches only if every level does.

Any other error propagates as usual. In particular, unpacking a value that
can't be unpacked raises:

```
# Takes the else branch: [1, 2] unpacks, but not into three items
if let a b c = [1, 2]
  echo unreachable
else
  echo "wrong arity"

# Raises: an Int cannot be unpacked
if let a b = 42
  echo unreachable
else
  echo also-unreachable
```

### Binding a Single Name

A pattern that is a single name doesn't unpack. It binds the scrutinee itself,
and matches when the scrutinee is truthy:

```
if let value = lookup key
  echo "found $value"
else
  echo "not found"
```

To match a sequence of exactly one item instead, put the name in parentheses:
`if let (value) = items`.

### `let` / `bind` with `else`

A standalone `let` or `bind` can have an `else` that runs when its pattern
doesn't match. Matching follows the same rules as `if let` and `if bind`,
including the truthiness test for a single name. The pattern's names stay in
scope after the statement:

```
let k v = s.split "="
else throw std.ValueError "expected KEY=VALUE: $s"
echo "$k -> $v"

bind response
  :status
  :body = ""
else
  return nil
```

The `else` body must not finish: it has to return, throw, break, continue, or
call something that never returns. Reaching its end raises `std.RuntimeError`
naming the statement, and the type checker warns when it finds a path there.
The pattern's names aren't in scope inside `else`, though an outer variable of
the same name is.

`else` goes on its own line at the statement's indentation, followed by an
indented block or one statement on the same line. `else if` isn't accepted;
put an `if` in an indented body instead. When the right-hand side of a `let` is
an `if`, an `else` belongs to the `if`, and the `let` can't have one of its
own.

In a [strict](../types/strict.md) file, `else` is how to destructure a value
that may not match.

## Constant Patterns

A constant in a pattern binds no name. It matches a value equal to it, compared
with `==`. A bare word is still a name, so quote a string to match it:

```
if let "GET" path = request
  echo $path

if bind response
  status: 200
  :body
do
  echo $body
```

A constant on its own tests the whole value: `if let "GET" = method` matches
when `method == "GET"`. In parentheses it is an item, so `if let ("GET") =
args` matches a one-item sequence holding `"GET"`. Constants nest in other
patterns too, as in `Array("GET" path)`.

The truthiness test for a single name doesn't apply to constants:
`if let false = flag` matches when `flag` is `false`.

A constant that doesn't match takes the `else` branch of a conditional
pattern, and raises `std.TypeError` in a plain `let`, `bind` or `for`.

A constant can't have an annotation or default, and can't be a `def`
parameter. Interpolated strings and other expressions that aren't constants are
rejected.

## Type-Test Patterns

`C(pattern)` tests whether the value is an instance of class `C`, and if so
matches the pattern inside the parentheses against the same value:

```
let Int(n) = 42
let Int(x) Str(label) = [1, "first"]
let Point(x: px y: py) = point
```

A single name inside the parentheses binds the whole value instead of
unpacking it, so `Int(n)` binds the integer. A default doesn't change that:
`Int(n = 0)` also binds the integer, and its default is used only when an
enclosing [`?`](#optional-sub-patterns) finds the item absent. A single name
binds the whole value in the same way at the top of a `let`, `bind` or `match`
pattern, as one alternative, and in a vertical block without a dash.

To unpack a single item, use parentheses or a dash, which always unpack:
`Array((item))`, `(n = 0)`, `- n = 0`. `Array()` matches an empty array.

The parentheses must touch the class name. `Int(n)` is a type test, but
`Int (n)` binds a name `Int` and unpacks the next item into `n`. The class may
be a dotted runtime name such as `std.Int`, and takes no type arguments.
Checker-only imports, aliases and protocols aren't runtime classes, so they
can't be tested. The class can't be a name the same pattern or parameter list
binds, as in `let C C(x) = ...`, but a later pattern may use it.

In vertical layout, `C $` introduces the inner pattern as an indented block:

```
bind point
  Point $
    x: px
    y: py
```

`C $` works in a `bind` block, at the end of a `bind … =>` pattern, and in
nested vertical patterns. Use `C(pattern)` everywhere else. Names inside either
form may have annotations and defaults, but the type test itself may not.
`def` parameters accept neither form.

A failed type test raises `TypeError` in an ordinary binding or lambda
parameter, and takes the `else` branch in a conditional one. A type test isn't
a truthiness test: `if let Bool(value) = false` matches and binds `false`. The
inner pattern can still fail to match on its own.

## Non-symbol Keys

`:key` and `key: name` match **symbol** keys. External input such as decoded
JSON usually has string keys. To match those, write the key as a constant:

```playground
#> import test:
#>   - assert_eq
import json

let payload = json.decode r|
  {"name": "Alice", "age": 30}

let Dict("name": name "age": age) = payload
assert_eq $name "Alice"
assert_eq $age 30
```

## Alternatives

`|` separates alternative patterns. They are tried in order, and the first
that matches binds the names:

```playground
#> import test:
#>   - assert_eq
def port_of address
  let Int(port) | _ Int(port) = address
  port

assert_eq (port_of 8080) 8080
let address = ["example.com", 443]
assert_eq (port_of address) 443
```

At the top level of `let`, `|` splits the whole pattern: `let a b | c = value`
tries `a b`, then `c`, each against the whole value. A single name as one
alternative binds the value itself, as it would as the whole pattern. In
`let Array((n)) | n = value`, the first alternative unpacks a one-item array
and the second binds `value` as it is.

Anywhere else, parentheses group alternatives into one item of the enclosing
pattern:

```
if let ("GET" | "HEAD") path = request
  echo $path
```

A group doesn't unpack, so to match a one-item sequence against alternatives,
put the group in a second pair of parentheses: `(("GET" | "HEAD"))`. Lambda
parameters accept alternatives only in such a group, and `def` parameters don't
accept alternatives.

In vertical layout, a block can list alternatives, each starting with `|` and
a space. An alternative continues on lines indented two columns past its `|`:

```playground
#> import test:
#>   - assert_eq
bind {status: 200, headers: {"Content-Type": "text/plain"}}
  :status
  headers:
    | "content-type": type
    | "Content-Type": type
assert_eq $type "text/plain"
```

A block is either entirely alternatives or has none. Alternatives on one line
of a block need parentheses.

Every alternative must bind the same names, in any order and position; `_` is
exempt. Defaults are evaluated only after the whole pattern matches, and only
those in the alternative that matched:

```
if bind args
  | :host
    :port = 80
  | host port
do
  echo "$host:$port"
```

When no alternative matches, a conditional pattern takes its `else` branch. A
plain `let`, `bind` or `for` raises the error the last alternative would raise
on its own. An error that isn't a failure to match (see
[What Counts as a Match](./patterns.md#what-counts-as-a-match)) propagates
without trying the remaining alternatives.

## Optional Sub-patterns

`?` before a nested pattern lets its item be absent. When it is, every name
inside takes its default, at any depth. Defaults are evaluated in item order and
may read names bound before them:

```playground
#> import test:
#>   - assert_eq
let ?"settings": ("port": port = 80 "next": next = (port + 1)) ... = {}
assert_eq $port 80
assert_eq $next 81

let first ?(second = 2) = [1]
assert_eq $first 1
assert_eq $second 2
```

A present item must still match its pattern. A present `nil` isn't absent:
unpacking it raises an error, even in a conditional pattern. `?()` matches an
absent item or an empty value.

`?` applies only to its own item. In the following, `k` may be absent, but a
`k` that is present must contain `j`. Writing `?j:` would make `j` optional as
well:

```
let ?k: (j: (value = 0)) = data
```

Every name under `?` needs a default. With alternatives under `?`, only the
first needs defaults, because an absent item takes the first alternative's
defaults without testing it:

```playground
#> import test:
#>   - assert_eq
let ?k: (Int(n = 0) | Str(n)) = {}
assert_eq $n 0
```

Restrictions:

- A rest under `?` can't have a name; an anonymous `...` is allowed.
- `?` can't come before a plain name or `:key`. Give it a default instead,
  such as `x = 0` or `:key = 0`.
- Required positional items must come before optional ones.

In vertical layout, `?` and a space start an optional positional item whose
pattern is the block after it. A `?` touching a key makes that key item
optional. `- ?(...)` is another way to write an optional positional item:

```
bind data
  - first
  ? - second = 2
    - third = 3
  ?settings:
    :port = 80
    ...
```

Here the second item of `data`, if present, is a sequence unpacked into
`second` and `third`.

In lambda parameters, `?` makes that parameter optional in the lambda's
signature:

```
let port_of = do |?settings: (:port = 80)| port
```

## `match`

`match` tries a value against a list of arms in order, and runs the body of
the first arm that matches. An arm is a pattern followed by its body, either an
indented block or `=>` and a statement on the same line:

```playground
#> import test:
#>   - assert_eq
def describe value
  match value
    Int(n) => "int $n"
    "GET" | "HEAD" => "read"
    Array(first _)
      let inner = describe $first
      "array of $inner"
    _ => "other"

assert_eq (describe 3) "int 3"
assert_eq (describe "HEAD") "read"
assert_eq (describe [1, 2]) "array of int 1"
assert_eq (describe nil) "other"
```

An arm's pattern is written as a `let` pattern is, with `|` splitting the whole
pattern, and its items may have defaults. Its names are in scope only in that
arm.

An arm starting with `|` lists its alternatives vertically, as a vertical
[alternatives](./patterns.md#alternatives) block does. Then `do` on its own
line ends them and starts the body as a block, or `=>` there takes a body on
the same line:

```playground
#> import test:
#>   - assert_eq
def summary response
  match response
    | Int(code)
    | Str(code)
    => "code $code"
    | :status
      :body = ""
    | status body
    do
      "$status $body"

assert_eq (summary {status: 200}) "200 "
assert_eq (summary [404, "missing"]) "404 missing"
assert_eq (summary 500) "code 500"
```

A single name matches any value, even `false` or `nil`, and binds it: there's
no truthiness test as in `if let`. So `_` makes a catch-all arm.

An arm whose body is a lambda puts it after `=>`, as in `_ => do |x| x + 1`.

A failure to match moves on to the next arm, but other errors propagate, as
described in [What Counts as a Match](./patterns.md#what-counts-as-a-match).
Unpacking a value that can't be unpacked raises, so when the value may not be a
sequence or keyed value, test its type first:

```playground
#> import test:
#>   - assert_eq
def size value
  match value
    Array(_ _) => 2
    Array((_)) => 1
    _ => 0

assert_eq (size [1, 2]) 2
assert_eq (size 5) 0
```

### Guards

`if` after an arm's pattern adds a guard: a condition checked after the
pattern matches, which can use the arm's names. If the guard fails, matching
continues with the next arm. The body follows as an indented block, or after a
`=>` that ends the condition:

```playground
#> import test:
#>   - assert_eq
def sign n
  match n
    m if (m < 0)
      :NEGATIVE:
    0 => :ZERO:
    m if (m > 100) => :LARGE:
    _ => :POSITIVE:

assert_eq (sign -3) :NEGATIVE:
assert_eq (sign 0) :ZERO:
assert_eq (sign 4) :POSITIVE:
assert_eq (sign 500) :LARGE:
```

A guard can also be `if let` or `if bind`, which matches a pattern of its own
as in [Conditional Matching](./patterns.md#conditional-matching). Its names
are in scope in the body along with the arm's. An `if bind` guard's vertical
pattern ends with `do` or `=>`, as a `|` list does. When the guard's pattern
follows `=>` on the same line, a second `=>` starts the body:
`h if bind rest => a b = 0 => [h, a, b]`.

### Fallback and the Result

A last arm whose pattern is a single name, such as `_`, runs when no other arm
matches. A guard on such an arm adds a further test, as `else if` does after
`if`:

```playground
#> import test:
#>   - assert_eq
def lookup table key
  match key
    Str(k) if let value = table.get(k)
      "$k is $value"
    _ if (type key Int) => "index $key"
    _ => "not found"

assert_eq (lookup {"a": 1} "a") "a is 1"
assert_eq (lookup {"a": 1} 4) "index 4"
assert_eq (lookup {"a": 1} "b") "not found"
```

Like `if`, `match` can be the right-hand side of `let` or an assignment. Its
result is that of the body that ran, or `nil` when no arm matches. The value
being matched is evaluated once, before the first arm. In
[vertical layout](./vertical-layout.md#match-in-vertical-layout), the bodies
hold items instead.

The [type checker](../types/checking.md#narrowing) narrows the value by each
arm that doesn't match, so a later arm sees only what remains:

```
def run step @ ((:CMD:, Str) | (:WAIT:, Int))
  match step
    :CMD: line => echo "run $line"
    _
      # step is (:WAIT:, Int)
      let _ secs = step
      echo "wait $secs"
```
