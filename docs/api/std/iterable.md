# Iterable

Abstract type for iterable values. Values that implement the `(iter)` protocol
are instances of `Iterable`, which can be used for type testing.

`Iterable` is not constructible directly.

```
assert (type [1, 2, 3] Iterable)
```

## Methods

### `iter`

Returns an iterator over the value.

#### Returns

[`Iter`](./iter.md)
