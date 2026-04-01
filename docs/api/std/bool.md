# bool

Boolean values: `true` and `false`.

## Constructor

Calling `bool` converts a value to a boolean based on its truthiness.

```
assert_eq (bool 0) false
assert_eq (bool 1) true
assert_eq (bool nil) false
assert_eq (bool "") false
assert_eq (bool "hello") true
```

Only `false` and `nil` are falsy. All other values, including `0` and `""`,
are truthy when used in boolean context (e.g. `if` conditions). Calling `bool`
explicitly converts based on the value's own truthiness rules: zero numbers and
empty strings are false.

## Operators

| Operator | Description                                              |
| -------- | -------------------------------------------------------- |
| `!x`     | Logical NOT                                              |
| `&&`     | Short-circuit AND (returns first falsy operand, or last) |
| `\|\|`   | Short-circuit OR (returns first truthy operand, or last) |
