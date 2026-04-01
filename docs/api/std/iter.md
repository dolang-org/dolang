# Iter

Abstract type for iterators. Values that implement the iteration protocol
are instances of `Iter`, which can be used for type testing.

`Iter` is not constructible directly. See
[Classes](../../language/classes.md) for defining custom iterators.

```
assert_eq (type $ [1, 2, 3].iter()) $Iter
```

## Methods

### `next :default? :else?`

Returns the next value from the iterator.

**Parameters:**

| Name       | Type | Description                                 |
| ---------- | ---- | ------------------------------------------- |
| `default:` |      | value to return if the iterator is empty    |
| `else:`    |      | callable to invoke if the iterator is empty |

**Errors:** Raises [`IterStop`](./iter-stop.md) when exhausted and no fallback
is provided.

### `all pred?`

Returns `true` if every yielded value is truthy.

When `pred` is provided, it tests `pred(value)` instead.

Empty iterators return `true`.

### `any pred?`

Returns `true` if any yielded value is truthy.

When `pred` is provided, it tests `pred(value)` instead.

Empty iterators return `false`.

### `count`

Consumes the iterator and returns the number of yielded values.

### `fold init func`

Consumes the iterator left-to-right, repeatedly applying `func(acc, value)`.

Returns `init` unchanged if the iterator is empty.

### `chain ...values`

Returns an iterator that yields this iterator followed by each additional
iterable in sequence.

### `zip ...values`

Returns an iterator that yields one tuple for each step across this iterator
and the additional iterables.
The zipped iterator stops as soon as any input is exhausted.

### `map func`

Creates a wrapper `Iter` which yields `func(value)` for each `value` yielded by
the wrapper iterator.

### `filter pred`

Creates a wrapper `Iter` which yields each `value` from the wrapper iterator
only if `pred(value)` is truthy.

### `min :default?`

Consumes the iterator and returns the minimum yielded value.

**Errors:** Raises [`IterStop`](./iter-stop.md) if the iterator is empty and no
`default:` is provided.

### `max :default?`

Consumes the iterator and returns the maximum yielded value.

**Errors:** Raises [`IterStop`](./iter-stop.md) if the iterator is empty and no
`default:` is provided.
