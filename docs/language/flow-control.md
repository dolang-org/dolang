# Flow Control

## Conditionals

### `if` / `else`

```
if (score >= 70)
  echo pass
else
  echo fail
```

The condition is a compact expression or a full command:

```
if flag
  echo "flag is set"

if func "argument"
  echo success
```

### `else if`

```
if (score >= 90)
  echo A
else if (score >= 80)
  echo B
else if (score >= 70)
  echo C
else
  echo F
```

### Same-Line Bodies

`=>` after a condition puts the body on the same line, as in a `match` arm.
`else` takes a body on its own line directly:

```
if (n < 0) => return "negative"
else if (n == 0) => return "zero"
else return "positive"
```

The body is a single statement. A separator is needed because the condition
accepts command arguments, so `if func arg body` would pass `body` to `func`.
Same-line and indented bodies can be mixed in one `if`.

Where the lines after a same-line body continue its statement, as an `else`
continues an `if` after a branch and a `catch` continues a `try`, the body can't
hold an `if`, `match` or `try`, even as the value of `let` or an assignment.
Those lines could belong to either statement. Use an indented block, or
`else if` for a chain. A `let` or `bind` there leaves an `else` after it to the
enclosing statement.

A body that ends its statement, such as an `else` body or that of a `for`, has
no such restriction, and neither does a [`match`](#match) arm.

### `if` in `let` and Assignments

`if` statements can be the right-hand side of `let` bindings and assignments:

```
let max = if (a > b)
  a
else
  b

let msg = if (count == 0)
  "empty"
else if (count == 1)
  "one"
else
  "many"

# Also works with assignment
result = if error
  get_error_message $error
else
  data
```

An `if` gives the result of the branch that runs, or `nil` if no branch runs:

```
let value = if true
  42
# value == 42

value = if false
  1
# value == nil
```

### `if let` / `if bind`

A `let` or `bind` pattern after `if` makes the pattern match the condition.
The bindings are in scope for the branch body, and the `else` branch runs when
the pattern does not match:

```
if let a b = pair
  echo "matched $a $b"
else
  echo "no match"
```

`if bind` puts the pattern in vertical layout after the scrutinee, with `do`
introducing the body, and so also accepts default values:

```
if bind response
  :status
  :body = ""
do
  echo "$status $body"
```

After `=>`, the pattern goes on the header line instead, and still accepts
defaults. The body follows as an indented block, or after a second `=>`:

```
if bind response => :status :body = ""
  echo "$status $body"

if bind entry => key value = "" => echo "$key=$value"
```

Both forms are also available where `if` appears in
[vertical layout](./vertical-layout.md).

An unpacking mismatch or a failed type test or constant comparison takes the
failure branch; other errors propagate. See
[Conditional Matching](./patterns.md#conditional-matching) for the details.

### `let` / `bind` with `else`

Use a diverging `else` to leave on a mismatch and keep the bindings in the
surrounding scope:

```
let k v = s.split "="
else throw std.ValueError "expected KEY=VALUE: $s"
echo "$k -> $v"
```

`else` accepts an inline statement or an indented block. Its body must return,
throw, break, continue, or otherwise never return; falling through raises
`std.RuntimeError`. See [Patterns](./patterns.md#let-bind-with-else) for scope
and matching rules.

### `match`

`match` runs the body of the first arm whose pattern matches a value. `if`
after an arm's pattern adds a guard, and `else` runs when no arm matches:

```
let label = match response
  Int(code) if (code >= 400)
    "error $code"
  Int(_) => "ok"
  Dict(:status :body) => "$status: $body"
else "unknown"
```

See [`match`](./patterns.md#match) for arms, guards, and alternatives.

## Loops

### `while`

```
let count = 0
while (count < 5)
  echo $count
  count = (count + 1)
```

`=>` gives `while` a same-line body, as it does for `if`:

```
while (count < 10) => count = (count + 1)
```

### `while let` / `while bind`

`while` accepts the same pattern forms as `if`. The loop ends the first time the
pattern fails to match:

```
let i = 0
while let a b = pairs.get(i)
  echo "$a $b"
  i = (i + 1)
```

### `for`

Iterate over arrays, dictionaries, ranges, and other iterables:

```playground
for item = [1, 2, 3]
  echo $item

for pair = {name: "Alice", age: 30}
  echo $pair

for i = Range 5
  echo $i
```

Dictionaries iterate as `(key, value)` tuples. Use a pattern to unpack them:

```
for k v = {name: "Alice", age: 30}
  echo "$k: $v"
```

`=>` after the iteratee gives `for` a same-line body, as it does for `while`:

```
for item = items => echo $item
```

See [Pattern Matching](patterns.md) for more on `for` unpacking.

## Flow Control Statements

### `break`

Exit the innermost loop.

```
for i = Range 100
  if (i >= 5)
    break
  echo $i
```

`break` works through intervening `do` blocks:

```
def find_incomplete configs
  for config = configs
    config.get :host: else: do
      break
    process $config
  "done"
```

### `continue`

Skip to the next iteration.

```
for i = Range 10
  if (i % 2 == 0)
    continue
  echo $i  # prints odd numbers
```

`continue` works through intervening `do` blocks:

```
for item = items
  let id = item.get :id: else: do
    continue
  echo "Processing $id"
```

### `return`

Return a value from a function early. A script may also return from its top
level. Modules and the REPL disallow returns that exit their top level,
including returns inside nested control flow or `do` blocks.

```playground
#> import test:
#>   - assert_eq
def find_first arr pred
  for item = arr
    if pred $item
      return item
  nil

let result = find_first [1, 2, 3, 4] do |x| (x > 2)
assert_eq $result 3
```

`return` exits the innermost enclosing `def`, even when called from within a
`do` block:

```
def validate_record record
  for field = [:id:, :name:, :email:]
    record.get $field else: do
      return {valid: false, missing: field}
  {valid: true}

let result = validate_record {name: "Alice", email: "alice@example.com"}
# Returns: {valid: false, missing: :id:}
```

### `throw`

Raises an error, unwinding the call stack until the error is caught
by a `try`/`catch` block.

```
throw "something went wrong"
```

See [Error Handling](error-handling.md) for full details on raising and catching
errors.
