# float

64-bit floating point numbers.

## Constructor

Calling `float` converts a value to a float.

```
assert_eq (float 42) 42.0
assert_eq (float "3.14") 3.14
assert_eq (float true) 1.0
assert_eq (float false) 0.0
```

### Errors

Raises an error if the value cannot be converted (e.g. `nil` or
a non-numeric string).

## Operators

### Arithmetic

| Operator | Description                  | Result  |
| -------- | ---------------------------- | ------- |
| `+`      | Addition                     | `float` |
| `-`      | Subtraction                  | `float` |
| `*`      | Multiplication               | `float` |
| `/`      | Division                     | `float` |
| `//`     | Euclidean (integer) division | `int`   |
| `%`      | Euclidean remainder          | `float` |
| `-x`     | Negation                     | `float` |

`//` always returns an `int`. `//` and `%` satisfy the identity
`x == (x // y) * y + (x % y)`.

### Comparison

`==`, `!=`, `<`, `>`, `<=`, `>=`

Mixed int/float comparisons are supported.
