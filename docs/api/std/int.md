# int

128-bit signed integers.

## Constructor

Calling `int` converts a value to an integer.

```
assert_eq (int "42") 42
assert_eq (int 3.14) 3
assert_eq (int true) 1
assert_eq (int false) 0
```

**Errors:** Raises an error if the value cannot be converted (e.g. `nil` or
a non-numeric string).

## Operators

### Arithmetic

| Operator | Description                  | Result  |
| -------- | ---------------------------- | ------- |
| `+`      | Addition                     | `int`   |
| `-`      | Subtraction                  | `int`   |
| `*`      | Multiplication               | `int`   |
| `/`      | Division                     | `float` |
| `//`     | Euclidean (integer) division | `int`   |
| `%`      | Euclidean remainder          | `int`   |
| `-x`     | Negation                     | `int`   |

`/` always produces a `float`. `//` and `%` satisfy the identity
`x == (x // y) * y + (x % y)`.

### Bitwise

| Operator | Description |
| -------- | ----------- |
| `&`      | AND         |
| `\|`     | OR          |
| `^`      | XOR         |
| `~x`     | NOT         |
| `<<`     | Left shift  |
| `>>`     | Right shift |

### Comparison

`==`, `!=`, `<`, `>`, `<=`, `>=`

Mixed int/float comparisons are supported.
