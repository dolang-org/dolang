# std

The `std` module provides core language facilities.

## Types

| Name                                              | Description                                |
| ------------------------------------------------- | ------------------------------------------ |
| [`args`](./args.md)                               | Immutable argument pack                    |
| [`array`](./array.md)                             | Mutable ordered sequence                   |
| [`bin`](./bin.md)                                 | Immutable binary data                      |
| [`BytecodeError`](./bytecode-error.md)            | Bytecode verification error                |
| [`bool`](./bool.md)                               | Boolean (`true` / `false`)                 |
| [`CanceledError`](./canceled-error.md)            | Strand cancellation                        |
| [`CompileError`](./compile-error.md)              | Compilation error                          |
| [`ConcurrencyError`](./concurrency-error.md)      | Concurrent access violation                |
| [`CyclicImportError`](./cyclic-import-error.md)   | Cyclic module dependency                   |
| [`dict`](./dict.md)                               | Mutable ordered dictionary                 |
| [`Descriptor`](./descriptor.md)                   | Abstract descriptor protocol type          |
| [`set`](./set.md)                                 | Mutable ordered set                        |
| [`Error`](./error.md)                             | Abstract base error type                   |
| [`FieldError`](./field-error.md)                  | Nonexistent field access                   |
| [`float`](./float.md)                             | 64-bit floating point                      |
| [`func`](./func.md)                               | Callable value                             |
| [`ImmutableError`](./immutable-error.md)          | Mutation of an immutable value             |
| [`ImportError`](./import-error.md)                | Module import failure                      |
| [`IndexError`](./index-error.md)                  | Out-of-bounds index access                 |
| [`int`](./int.md)                                 | 128-bit signed integer                     |
| [`InterruptError`](./interrupt-error.md)          | External interruption                      |
| [`Iter`](./iter.md)                               | Abstract iterator type                     |
| [`Iterable`](./iterable.md)                       | Abstract iterable type                     |
| [`IterStop`](./iter-stop.md)                      | Error raised when an iterator is exhausted |
| [`MissingKeyError`](./missing-key-error.md)       | Required keyword argument not provided     |
| [`MissingPosError`](./missing-pos-error.md)       | Required positional argument not provided  |
| [`Nil`](./nil.md)                                 | Type object for `nil`                      |
| [`OverflowError`](./overflow-error.md)            | Integer overflow                           |
| [`property`](./property.md)                       | Descriptor helper for computed fields      |
| [`range`](./range.md)                             | Numeric range for iteration                |
| [`record`](./record.md)                           | Record with dot-syntax access              |
| [`RuntimeError`](./runtime-error.md)              | Ordinary runtime failure supertype         |
| [`StateError`](./state-error.md)                  | Invalid operation for current state        |
| [`Sinkable`](./sinkable.md)                       | Abstract sinkable type                     |
| [`Sink`](./sink.md)                               | Abstract sink type                         |
| [`SinkStop`](./sink-stop.md)                      | Error raised when a sink is closed         |
| [`str`](./str.md)                                 | Immutable UTF-8 string                     |
| [`sym`](./sym.md)                                 | Interned symbol                            |
| [`tuple`](./tuple.md)                             | Immutable ordered sequence                 |
| [`type`](./type.md)                               | Type of types                              |
| [`TypeError`](./type-error.md)                    | Wrong type for an operation                |
| [`UnexpectedKeyError`](./unexpected-key-error.md) | Unexpected keyword argument                |
| [`UnexpectedPosError`](./unexpected-pos-error.md) | Unexpected positional argument             |
| [`UnsupportedError`](./unsupported-error.md)      | Unsupported operation                      |
| [`ValueError`](./value-error.md)                  | Invalid value for an operation             |
| [`value`](./value.md)                             | Abstract supertype of all values           |
| [`ZeroDivError`](./zero-div-error.md)             | Integer division or modulo by zero         |

## Values

### `nulliter`

Acts as both an iterator and a sink that performs no operations.

- **As an iterator:** `nulliter` never yields any items.
- **As a sink:** `nulliter` silently discards all items.

## Functions

### `dbg value`

Converts a value to its debug representation. Shows internal structure (e.g.
quotes strings, shows type tags).

**Parameters:**

| Name    | Type | Description          |
| ------- | ---- | -------------------- |
| `value` |      | the value to convert |

**Returns:** [`str`](./str.md)

### `record ...`

Creates a record from keyword arguments.

**Parameters:**

| Name         | Type | Description   |
| ------------ | ---- | ------------- |
| keyword args |      | become fields |

**Returns:** [`record`](./record.md)

```
let r = record name: Alice age: 30
echo $r.name  # Alice
```

### `arg value`

Converts a value to its external argument representation. Preserves the literal
textual form of values where possible, which is useful for passing values as
command-line arguments to external programs.

**Parameters:**

| Name    | Type | Description          |
| ------- | ---- | -------------------- |
| `value` |      | the value to convert |

**Returns:** [`str`](./str.md)

### `hash ...values`

Returns a hash code computed over all supplied values in sequence. Passing
multiple values is useful for combining fields in a `(hash)` implementation:

```
def (hash) self
  hash $self.x $self.y $self.z
```

**Parameters:**

| Name        | Type | Description                |
| ----------- | ---- | -------------------------- |
| `...values` |      | one or more values to hash |

**Returns:** `int`
