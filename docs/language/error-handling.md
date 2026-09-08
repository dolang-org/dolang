# Error Handling

Do provides structured error handling through the `try`/`catch`/`finally`
statement and the `throw` statement for raising errors.

## Raising Errors

Use `throw` to raise an error with any value:

```
throw "something went wrong"
throw {code: 404, message: "not found"}
```

## Try / Catch / Finally

The `try` statement executes a block and optionally catches errors and runs
cleanup code:

```
try
  risky_operation()
catch err
  echo "Error: $err"
finally
  cleanup()
```

### Basic Try/Catch

```
try
  let data = parse input
  process $data
catch err
  echo "Failed: $err"
```

The result of a `try` statement is the result of the body on success, or the
result of the executed catch handler if an error was caught:

```
let result = try
  parse input
catch err
  default_value
```

### Typed Catch Handlers

Errors can be caught by type. Handlers are tried in order:

```
import std:
  - TypeError
  - IndexError

try
  something()
catch TypeError: err
  echo "Type error: $err"
catch IndexError: err
  echo "Index error: $err"
catch err
  echo "Other error: $err"
```

Typed handlers use subtype matching, so a handler for a parent class will catch
errors of any child class. The catch-all handler (without a type) must be last
and catches any error not matched by a preceding handler.

### Subclassing Error Types

A module that wants callers to be able to catch its failures specifically can
define its own error class. Inherit from the closest `std` error type and chain
to its constructor, passing up the message the error should display:

```
import std:
  - RuntimeError
  - ValueError

class NoDomainError: RuntimeError
  pub field name = nil

  def (init) self name
    RuntimeError.(init) $self "domain does not exist: $name"
    self.name = name
```

The chained call is what gives the instance its string form, so the subclass
needs no `(str)` of its own. Subtype matching then works the whole way up the
chain — a handler for the specific type, for the `std` type it inherits from, or
for [`Error`](std.Error) will all catch it:

```
try
  throw NoDomainError "guest0"
catch NoDomainError: err
  echo "missing: $(err.name)"
```

Inherit from the type that describes the failure — `ValueError` for a malformed
value, `RuntimeError` when nothing more specific fits. Every `std` error type is
subclassable except [`AbortError`](std.AbortError) and
[`BytecodeError`](std.BytecodeError), which are sealed.

### Finally

The `finally` block runs regardless of whether the body succeeded or raised an
error. It runs after all other blocks but before any error is propagated:

```
let f = open_resource()
try
  process $f
finally
  f.close()
```

If the `finally` block itself raises an error, it takes priority over any
error from the body or catch handlers.

### Combining Catch and Finally

All three clauses can be used together:

```
import std:
  - TypeError

try
  let conn = connect()
  query $conn
catch TypeError: err
  echo "Bad query: $err"
  nil
catch err
  echo "Connection failed: $err"
  nil
finally
  disconnect()
```

### Try as Right-Hand Side

`try` can be used on the right-hand side of `let` and assignment, just like
`if`:

```
let value = try
  parse $dangerous_input
catch _
  "fallback"
```
