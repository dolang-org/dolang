# Variables

Variables in Do are dynamically typed and lexically scoped. They must be
declared before use.

## Declaration with `let`

Introduce new variables with `let`:

```
let x = 42
let greeting = "Hello, world!"
```

The right-hand side of `let` is a compact expression context, so variables and
calls work without `$`:

```
let doubled = func(x)
let items = [1, 2, 3]
```

It can also be a complete command statement:

```
let response = http.get https://google.com
```

## Assignment

Reassign an existing variable with `=`:

```
let count = 0
count = 1
count = (count + 1)
```

Assignment does not declare a new variable; the variable must already be bound
by `let`, a function parameter, `for`, `bind`, or another binding form.

## Scoping

Variables are lexically scoped to the block where they are declared:

```
let x = 1
if true
  let y = 2
  echo $x $y  # both visible
# y is no longer in scope here
echo $x
```

Functions close over their defining scope:

```
let x = 42
let f = do
  echo $x  # captures x
f()  # prints: 42
```

Closures can mutate captured variables:

```
let counter = 0
let inc = do
  counter = (counter + 1)
inc()
inc()
echo $counter  # 2
```
