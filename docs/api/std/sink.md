# Sink

Abstract type for sinks. Values that implement the sink protocol
are instances of `Sink`, which can be used for type testing.

`Sink` is not constructible directly.

```
assert_eq (type $ [].sink()) $Sink
```

## Inherits

- [`Sinkable`](./sinkable.md)

## Methods

### `put value`

Writes a value to the sink.

**Parameters:**

| Name    | Type | Description        |
| ------- | ---- | ------------------ |
| `value` |      | the value to write |

### `map func`

Creates a wrapper `Sink` which puts `func(value)` into the wrapped sink for each
`value`.

### `filter pred`

Creates a wrapper `sink` which only puts each `value` if `pred(value)` is
truthy.
