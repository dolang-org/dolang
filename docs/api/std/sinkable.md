# Sinkable

Abstract type for values that implement the `(sink)` protocol.
`Sinkable` values can be converted into sinks explicitly and used for type
testing.

`Sinkable` is not constructible directly.

```
assert (type [] Sinkable)
```

## Methods

### `sink`

Returns a sink over the value.

#### Returns

[`Sink`](./sink.md)
