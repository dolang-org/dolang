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

## Loops

### `while`

```
let count = 0
while (count < 5)
  echo $count
  count = (count + 1)
```

### `for`

Iterate over arrays, dictionaries, ranges, and other iterables:

```
for item = [1, 2, 3]
  echo $item

for pair = {name: "Alice", age: 30}
  echo $pair

for i = range 5
  echo $i
```

Dictionaries iterate as `[key, value]` pairs. Use destructuring to unpack them:

```
for k v = {name: "Alice", age: 30}
  echo "$k: $v"
```

See [Destructuring](destructuring.md) for more on `for` unpacking.

## Flow Control Statements

### `break`

Exit the innermost loop.

```
for i = range 100
  if (i >= 5)
    break
  echo $i
```

`break` works through intervening `do` blocks:

```
def find_incomplete configs
  for config = configs
    config.get "host" else: do
      break
    process $config
  "done"
```

### `continue`

Skip to the next iteration.

```
for i = range 10
  if (i % 2 == 0)
    continue
  echo $i  # prints odd numbers
```

`continue` works through intervening `do` blocks:

```
for item = items
  let id = item.get id else: do
    continue
  echo "Processing $id"
```

### `return`

Return a value from a function early.

```
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
  for field = ["id", "name", "email"]
    record.get field else: do
      return {valid: false, missing: field}
  {valid: true}

let result = validate_record {name: "Alice", email: "alice@example.com"}
# Returns: {valid: false, missing: "id"}
```

### `throw`

Raises an error, unwinding the call stack until the error is caught
by a `try`/`catch` block.

```
throw "something went wrong"
```

See [Error Handling](error-handling.md) for full details on raising and catching
errors.
