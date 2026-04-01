# sym

Symbols are interned identifiers used for dictionary keys and enum-like
values.

## Creating Symbols

Literal syntax with surrounding colons:

```
let s = :my_symbol:
```

From a string with `sym`:

```
let s = sym "my_symbol"
assert_eq $s :my_symbol:
```

## Operations

### Equality

Symbols are compared by identity:

```
assert_eq :foo: :foo:
assert (:foo: != :bar:)
```

### Comparison

Symbols compare lexicographically by name:

```
assert (:alpha: < :beta:)
```

### Common Uses

Enum-like values:

```
let status = :ok:
let mode = :verbose:
```

Dictionary keys:

```
let d = {}
d[:status:] = "active"
```
