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
alternative is provided. Negative indexes count from the end.

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
assert_eq $pair[-1] "Alice"
```

Tuples are immutable; indexed assignment is not supported.

Tuples also accept [`range`](./range.md) values for slicing:

```
let tup = tuple([0, 1, 2, 3])
assert_eq $tup[1..3] (tuple [1, 2])
assert_eq $tup[..2] (tuple [0, 1])
assert_eq $tup[2..] (tuple [2, 3])
assert_eq $tup[..] (tuple [0, 1, 2, 3])
assert_eq $tup[range 0 4 2] (tuple [0, 2])
assert_eq $tup[range nil nil -1] (tuple [3, 2, 1, 0])
```

Slice indexing returns a new tuple.
Omitted `start` means `0`, omitted `end` means the tuple length, and negative
`start` and `end` values count from the end. Negative steps reverse the slice.

### Unpacking

```
for k v = {name: "Alice", age: 30}
  echo "$k = $v"
```
