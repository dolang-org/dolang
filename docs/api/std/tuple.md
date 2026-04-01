# tuple

Tuples are immutable, ordered sequences of values. They are produced by
certain operations such as iterating over key-value pairs. The `tuple` type
object is in the prelude, so `tuple(iterable)` is available without an
explicit import.

## Constructor

### `tuple iterable`

Builds a tuple from an iterable.

**Parameters:**

| Name       | Type | Description        |
| ---------- | ---- | ------------------ |
| `iterable` |      | values to collect  |

**Returns:** `tuple`

```
let tup = tuple([1, 2, 3])
assert_eq $tup[1] 2

for k v = {name: "Alice", age: 30}
  # each pair is a tuple
  echo "$k: $v"
```

## Fields

### `len`

Returns the number of elements.

**Type:** `int`

## Methods

### `get index :default? :else?`

Retrieves the value at the given index. Returns `nil` if out of bounds and no
alternative is provided.

**Parameters:**

| Name       | Type  | Description                         |
| ---------- | ----- | ----------------------------------- |
| `index`    | `int` | the index to access                 |
| `default:` |       | value to return if out of bounds    |
| `else:`    |       | callable to invoke if out of bounds |

**Returns:** The value, or the default/else result.

### `contains element`

Tests whether the tuple contains the given element (by equality).

**Parameters:**

| Name      | Type | Description        |
| --------- | ---- | ------------------ |
| `element` |      | the value to check |

**Returns:** `bool`

### `pairs`

Returns an iterator yielding `[index, value]` pairs.

**Returns:** iterator of `[int, value]` pairs

## Operations

### Indexing

```
let pair = {name: "Alice"}.pairs().next()
assert_eq $pair[0] :name:
assert_eq $pair[1] "Alice"
```

Tuples are immutable; indexed assignment is not supported.

### Unpacking

```
for k v = {name: "Alice", age: 30}
  echo "$k = $v"
```
