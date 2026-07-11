# Iter

Abstract type for iterators. Values that implement the iteration protocol
are instances of `Iter`, which can be used for type testing.

`Iter` is not constructible directly. See
[Classes](../../language/classes.md) for defining custom iterators.

```
assert_eq (type $ [1, 2, 3].iter()) $Iter
```

## Inherits

- [`Iterable`](./iterable.md)

## Methods

### `next :default? :else?`

Returns the next value from the iterator.

**Parameters:**

| Name      | Type | Description                                 |
| --------- | ---- | ------------------------------------------- |
| `default` |      | value to return if the iterator is empty    |
| `else`    |      | callable to invoke if the iterator is empty |

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

### `take n`

Returns an iterator that yields at most `n` values.

**Parameters:**

| Name | Type | Description             |
| ---- | ---- | ----------------------- |
| `n`  | int  | maximum values to yield |

**Errors:** Raises [`TypeError`](./type-error.md) if `n` is not an `int`.
Raises [`ValueError`](./value-error.md) if `n` is negative.

### `skip n`

Returns an iterator that discards the first `n` values, then yields the rest.

**Parameters:**

| Name | Type | Description         |
| ---- | ---- | ------------------- |
| `n`  | int  | values to discard   |

**Errors:** Raises [`TypeError`](./type-error.md) if `n` is not an `int`.
Raises [`ValueError`](./value-error.md) if `n` is negative.

### `enumerate`

Returns an iterator that yields `[index, value]` tuples.

The first index is `0`.

### `kv`

Returns an iterator wrapper that preserves normal iteration, but opts into
key/value spreading.

When spread in a keyed context such as a dict literal or argument spread, each
yielded item must unpack into exactly two values.

```
let entries = ["x=1", "y=2"].iter().map do |e| e.split "="

assert_eq {...entries.kv()} {"x": "1", "y": "2"}
```

### `map func`

Creates a wrapper `Iter` which yields `func(value)` for each `value` yielded by
the wrapper iterator.

### `filter pred`

Creates a wrapper `Iter` which yields each `value` from the wrapper iterator
only if `pred(value)` is truthy.

### `find pred :default? :else?`

Consumes the iterator and returns the first value where `pred(value)` is truthy.

**Parameters:**

| Name      | Type | Description                              |
| --------- | ---- | ---------------------------------------- |
| `pred`    |      | callable used to test values             |
| `default` |      | value to return if no value matches      |
| `else`    |      | callable to invoke if no value matches   |

**Errors:** Raises [`RuntimeError`](./runtime-error.md) if no value matches and
no fallback is provided.

### `min :default?`

Consumes the iterator and returns the minimum yielded value.

**Errors:** Raises [`IterStop`](./iter-stop.md) if the iterator is empty and no
`default:` is provided.

### `max :default?`

Consumes the iterator and returns the maximum yielded value.

**Errors:** Raises [`IterStop`](./iter-stop.md) if the iterator is empty and no
`default:` is provided.
