# Expressions

Do has three expression contexts: full expressions (inside delimiters), compact
expressions (after `$`), and string interpolation (inside string literals).

## Full Expressions

Within parentheses `()`, brackets `[]`, and braces `{}`, Do uses C-like syntax
where whitespace is insignificant and operators are available:

```
let result = (1 + 2 * 3)     # 7
let arr = [1, 2, 3]
let dict = {name: "Alice", age: 30}
```

Full expressions support:

- Arithmetic operators: `+`, `-` (including unary negation), `*`, `/`, `//`, `%`
- Comparison operators: `==`, `!=`, `<`, `<=`, `>`, `>=`
- Logical operators: `&&`, `||`, `!`
- Bitwise operators: `&`, `|`, `^`, `~`, `<<`, `>>`
- Range expressions: `a..b`, `..b`, `a..`, `..`
- Function calls:
    - Juxtaposition: `echo "hello" "world"`
    - C-style: `echo("hello", "world")`, `func()`
- Indexing: `arr[0]`, `dict["key"]`
- Field access: `obj.field`

Full expressions can span multiple lines:

```
let value = (
  some_long_computation(x, y) +
  another_value * factor
)
```

### Operator Precedence

From lowest to highest:

1. `$` (low-precedence call)
2. `||`
3. `&&`
4. `==`, `!=`, `<`, `<=`, `>`, `>=`
5. Range: `a..b`, `..b`, `a..`, `..`
6. `|`
7. `^`
8. `&`
9. `<<`, `>>`
10. `+`, `-`
11. `*`, `/`, `//`, `%`
12. Unary `-`, `!`, `~`
13. Call, index, field access

### Ternary-Style Expressions

There is no dedicated ternary operator, but `&&` and `||` short-circuit and
return their operands (as in Lua or Python), so you can write:

```
let label = (condition && "yes" || "no")
```

### Range Expressions

`..` constructs a [`Range`](std.Range) value.

```
let bounded = 1..5
let from_start = ..5
let to_end = 1..
let all = ..
```

`a..b` is half-open: it includes `a` and excludes `b`.

Open-ended forms are primarily used for slicing:

```
let arr = [0, 1, 2, 3]
assert_eq $arr[1..3] [1, 2]
assert_eq $arr[..2] [0, 1]
assert_eq $arr[2..] [2, 3]
assert_eq $arr[..] [0, 1, 2, 3]
```

Bounded ranges are iterable. `a..` is also iterable and unbounded. `..b` and
`..` are not iterable because they have no starting value.

## Compact Expressions

The `$` prefix introduces a compact expression at statement level. It supports:

- Variable access: `$name`
- Field access: `$person.name`
- Indexing: `$arr[0]`
- Range expressions: `$start..end`, `$start..`, `$..end`, `$..`
- C-style calls: `$func(arg1, arg2)`
- Chaining: `$obj.method(arg).field[0]`
- Boolean not: `$!flag`

```
let person = {name: "Alice", age: 30}
echo $person.name
echo $person["age"]
echo $Str(person.age)
```

### Implicit

Some special statement forms expect compact expression without `$` to avoid
needing to write it in those cases:

- The right-hand side of a `let` or assignment
- The condition of an `if` or `while`
- The iteratee of a `for`
- The scrutinee of a `bind`
- After `return` or `throw`

In fact, using `$` unnecessarily in these contexts is a syntax error.

## String Interpolation

Quoted strings, here strings, and binary strings interpolate values with `$`,
using a syntax of their own that is narrower than a compact expression. See
[Strings](strings.md#interpolation).
