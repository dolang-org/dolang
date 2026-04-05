# dict

Dictionaries are **ordered**, mutable key-value mappings. They preserve
insertion order and are **multi-maps**: a single key can have multiple values.

Keys can be any hashable type.

## Ordering

Dictionaries preserve insertion order. Iteration yields entries in the order
they were inserted.

## Multi-Map Semantics

A dict can store multiple values per key. This is primarily relevant when
constructing dicts from spreading or using methods with the `instance`
parameter:

- **Construction** with duplicate keys preserves all values
- **Plain indexing** (`d[key]`) returns only the first value for a key
- **Plain assignment** (`d[key] = value`) replaces all values for a key
- **`insert`** adds a new value without removing existing ones for that key
- **`get`** and **`pop`** accept an `instance` parameter to access specific
  values by their position (0-indexed) among values for that key; negative
  instance indexes count from the end

## Fields

### `len`

Returns the number of key-value pairs (counting each value in a multi-map
separately).

**Type:** [`int`](./index.md)

## Methods

### `clear`

Removes all key-value pairs.

```
let d = {a: 1, b: 2}
d.clear
assert_eq $d.len 0
```

### `insert key value`

Adds a key-value pair. Does **not** remove existing values for the same key
(multi-map insert).

**Parameters:**

| Name    | Type | Description |
| ------- | ---- | ----------- |
| `key`   |      | the key     |
| `value` |      | the value   |

### `get key instance? :default? :else?`

Retrieves the value for a key. Returns `nil` if the key is missing and no
alternative is provided. Negative `instance` indexes count from the end.

**Parameters:**

| Name       | Type                | Description                                                                                                 |
| ---------- | ------------------- | ----------------------------------------------------------------------------------------------------------- |
| `key`      |                     | the key to look up                                                                                          |
| `instance` | [`int`](./index.md) | which value to retrieve when a key has multiple values (0-indexed; negative counts from end; default: last) |
| `default:` |                     | value to return if missing                                                                                  |
| `else:`    |                     | callable to invoke if missing                                                                               |

**Returns:** The value, or the default/else result.

```
let d = {name: "Alice"}
assert_eq (d.get :name:) "Alice"
assert_eq ({name: "Alice", name: "Bob"}.get :name: -1) "Bob"
assert_eq (d.get :missing: default: "unknown") "unknown"
```

### `pop key instance? :default? :else?`

Removes and returns a value for a key. Raises an error if the key is missing
and no alternative is provided. Supports `instance` for multi-map access to
remove a specific value by its position among values for that key. Negative
`instance` indexes count from the end.

**Parameters:**

| Name       | Type  | Description                                                                                               |
| ---------- | ----- | --------------------------------------------------------------------------------------------------------- |
| `key`      |       | the key to remove                                                                                         |
| `instance` | `int` | which value to remove when a key has multiple values (0-indexed; negative counts from end; default: last) |
| `default:` |       | value to return if missing                                                                                |
| `else:`    |       | callable to invoke if missing                                                                             |

**Returns:** The removed value, or the default/else result.

```
let d = {a: 1, b: 2}
d.insert "multi" "first"
d.insert "multi" "second"

# Pop first instance (default)
assert_eq (d.pop "multi") "first"

# Pop specific instance
assert_eq (d.pop "multi" 0) "second"
```

### `delete key`

Removes all values for the key.

**Parameters:**

| Name  | Type | Description       |
| ----- | ---- | ----------------- |
| `key` |      | the key to remove |

**Returns:** [`bool`](./index.md) indicating whether any values were removed

```
let d = {a: 1, b: 2}
assert (d.delete :a:)
assert (!(d.delete :missing:))
assert_eq $d.len 1
```

### `pairs`

Returns an iterator yielding `[key, value]` pairs, the same as ordinary
iteration. This method is present to allow uniform key/value iteration
over both [`dict`](./dict.md) and [`array`](./array.md).

**Returns:** iterator of `[key, value]` pairs

### `keys`

Returns an iterator of keys. Each distinct key is yielded exactly once, in the
order its first pair was inserted.

If duplicate-key iteration is needed, use [`pairs`](#pairs) instead.

**Returns:** iterator of keys

### `values key?`

Returns an iterator of values. With no argument, it yields all stored values in
pair insertion order. With a `key`, it yields only the values associated with
that key, in that key's insertion order.

Missing keys return an empty iterator.

**Returns:** iterator of values

### `count key?`

Returns a count derived from the dictionary's multi-map structure.

With no `key`, it returns the number of distinct keys. With a `key`, it returns
the number of values associated with that key.

Missing keys return `0`.

**Returns:** [`int`](./index.md)

### `contains key value?`

Tests whether the dictionary contains the given key. If a value is provided,
tests whether any value associated with that key matches the given value
(multi-map aware).

**Parameters:**

| Name    | Type | Description                             |
| ------- | ---- | --------------------------------------- |
| `key`   |      | the key to check                        |
| `value` |      | optional value to check for (multi-map) |

**Returns:** [`bool`](./index.md)

```
let d = {a: 1, b: 2}
d.insert "multi" "first"
d.insert "multi" "second"

# Key-only check
assert (d.contains "a")
assert (!d.contains "z")

# Key + value check (multi-map aware)
assert (d.contains "multi" "first")
assert (d.contains "multi" "second")
assert (!d.contains "multi" "third")
```

## Operations

### Indexing

```
let d = {name: "Alice"}
assert_eq $d[:name:] "Alice"
d[:age:] = 30
```

Missing keys raise an error on access. Assignment replaces all values for the
key.

### Iteration

Iterating over a dictionary yields `[key, value]` pairs:

```
for pair = {a: 1, b: 2}
  echo $pair
```

### Unpacking

```
let name age: years = {name: "Alice", age: 30}
```
