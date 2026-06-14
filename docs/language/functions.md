# Functions

Functions in Do are first-class values. They close over their lexical scope and
can be stored in variables, passed as arguments, and returned from other
functions.

## `def`

Define named functions with `def`:

```
def greet name
  echo "Hello, $name!"

greet Alice
# prints: Hello, Alice!
```

### Decorators

Definitions may be preceded by one or more decorators using `#[expr]` syntax:

```
#[memoize]
def fib n
  if (n < 2)
    n
  else
    (fib(n - 1) + fib(n - 2))
```

Each decorator expression is evaluated in the surrounding scope. After the
function value is created, decorators are applied from bottom to top, with each
decorator receiving the current value and returning a replacement.

The same syntax also applies to `class` definitions. See
[Classes](./classes.md#computed-fields-with-property) for the common
`#[field.setter]` property pattern.

## Implicit Return

A function returns the result of its statement or block implicitly. Every
statement produces a result:

| Statement                   | Result                                        |
| --------------------------- | --------------------------------------------- |
| command                     | function return value                         |
| `let`                       | value of right-hand side                      |
| `bind`                      | value of scrutinee                            |
| `if` (with final `else`)    | result of branch                              |
| `if` (without final `else`) | `nil`                                         |
| `try`/`catch`               | result of `try` (no error) or invoked `catch` |

All other statements have a `nil` result. The result of the last statement in a
block becomes the block's result, and thus that of the function call.

Use `return` for early exit:

```
def abs x
  if (x < 0)
    return (-x)
  x
```

## Parameters

### Positional Parameters

```
def add a b
  (a + b)
```

### Keyword Parameters

Keyword parameters use `key: param` syntax in the definition:

```
def create_user name age: user_age
  echo "$name is $user_age"

create_user Alice age: 30
```

### Ditto Key Shorthand

The `:key` shorthand declares a key parameter bound to a variable of the
same name:

```
def create_user :name :age
  echo "$name is $age years old"

create_user name: Alice age: 30
```

The shorthand also works at call sites to pass a variable as a key argument:

```
let name = Alice
let age = 30
create_user :name :age
# equivalent to: create_user name: $name age: $age
```

### Default Values

Both positional and key parameters support defaults:

```
def greet name = "World"
  echo "Hello, $name!"

greet()        # Hello, World!
greet Alice    # Hello, Alice!

def connect :host = localhost :port = 8080
  echo "Connecting to $host:$port"

connect()                      # localhost:8080
connect port: 3000             # localhost:3000
connect host: example.com      # example.com:8080
```

Defaults are instantiated on every invocation of the function, so the following
function always returns a fresh empty `array` if called with no arguments:

```
def default_empty arg = []
  arg
```

### Variadic Parameters

Use `...` to accept extra arguments:

```
def log level ...rest
  echo "[$level]" ...rest

log INFO hello world
# prints: [INFO] hello world
```

The `rest` parameter receives an argument iterator that yields positional and
key arguments in invocation order. When iterating arguments manually, each item
is a `[key, value]` pair where the key is the symbol for arguments and the
positional argument index (0-origin) for positional arguments.

```
def echo_all ...args
  for k v = args
    echo "$k: $v"
echo_all foo bar: 1 baz
# prints:
# 0: foo
# bar: 1
# 1: baz
```

### Argument Spreading

Spread an iterable into a call:

```
let args = [1, 2, 3]
func ...args
# equivalent to: func 1 2 3

let kwargs = {name: "Alice", age: 30}
func ...kwargs
# equivalent to:
# func name: "Alice" age: 30
```

### Vertical Parameter Layout

Parameters in `def` can use vertical layout:

```
pub def build
  :from
  :pull = true
  :tag
  ...args
do
  echo "Building $tag from $from"
```

## `do` Blocks

Anonymous functions (blocks and lambdas) are created with `do`:

### Statement Context

In statement context, `do` without a following newline creates a one-statement
block:

```
let greet = do echo "hello"
greet()  # prints: hello
```

With parameters:

```
let double = do |x| echo (x * 2)
double 5  # prints: 10
```

If an immediate newline and indented block follows, it creates a multi-statement
block:

```
let process = do |x|
  let doubled = (x * 2)
  echo "Result: $doubled"
  doubled
```

### Expression Context

In expression context, `do` creates a lambda where the body is an expression:

```
assert_eq ((do |x| x * 2) 5) 10
```

### Non-Local Flow Control

`break`, `continue`, and `return` work through `do` blocks:

- `break` exits the innermost enclosing loop
- `continue` skips to the next iteration of the innermost enclosing loop
- `return` exits the innermost enclosing `def`

This allows natural flow control in callbacks and higher-order functions:

```
def validate_record record
  for field = ["id", "name", "email"]
    record.get field else: do
      return {valid: false, missing: field}
  {valid: true}
```

A non-local branch will only be effective for the duration of the statement
that introduces its containing closure; after this it will propagate a runtime
error. The compiler requires that a closure containing non-local branches be
in argument position to reduce the likelihood of this sort of error:

```
# Valid: closure is passed as an argument
def validate record
  record.get "name" else: do
    return false
  true

# Invalid: closure is bound to a variable first
def bad_example
  # Compiler will reject this line
  let closure = do return false
  record.get "name" else: $closure
```

## Public Functions

Use `pub def` to export a function from a module:

```
pub def helper x
  (x + 1)
```

See [Modules](./modules.md) for details on the module system.
