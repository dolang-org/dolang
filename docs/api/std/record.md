# record

Records are similar to [dict](dict.md)s but support only symbol and integer
keys. They allow direct field access with dot syntax.

## `record` Constructor

Constructs a record verbatim from all arguments, with positional arguments
receiving incrementing integer keys starting from `0` and key arguments
becoming fields. Order and key multiplicity are preserved.

```
let r = record name: Alice age: 30
echo $r.name  # Alice
echo $r.age   # 30
```

## Field Access

Records support direct dot-syntax for symbol keys:

```
let r = record name: Alice age: 30
echo $r.name
r.age = 31
```

And indexing for both symbol and integer keys:

```
echo $r[:name:]
r[0] = "first"
```

## Ordering and Multi-Map

Like dicts, records preserve insertion order and support multi-map semantics
where applicable.

## Iteration

Records are iterable, yielding `[key, value]` pairs:

```
for k v = record name: Alice age: 30
  echo "$k: $v"
```

## Unpacking

Records support destructuring like dicts:

```
let :name :age = record name: Alice age: 30
```

## Type Methods

For programmatic access, the `record` type object provides methods. These are
called on the type, not on instances, as the instance field namespace is
entirely reserved for the user.

### `record.len rec`

Returns the number of fields.

#### Returns

`int`

### `record.clear rec`

Clears all fields.

### `record.insert rec key value`

Sets a field. Key must be a symbol or integer.

### `record.get rec key :instance? :default? :else?`

Gets a field value with optional default. Supports `instance:` for multi-map
access. Negative `instance:` indexes count from the end.

### `record.pop rec key :instance? :default? :else?`

Removes and returns a value for a field. Supports `instance:` for multi-map
access to remove a specific value by its position among values for that field.
Negative `instance:` indexes count from the end.

### `record.delete rec key`

Removes all values for a field.

Returns [`bool`](./index.md) indicating whether any values were removed.

### `record.keys rec`

Returns an iterator of keys. Each distinct key is yielded exactly once, in the
order its first pair was inserted.

If duplicate-key iteration is needed, use ordinary record iteration.

### `record.values rec key?`

Returns an iterator of values. With no `key`, it yields all stored values in
pair insertion order. With a `key`, it yields only the values associated with
that key, in that key's insertion order.

Missing keys return an empty iterator.

### `record.count rec key?`

Returns a count derived from the record's multi-map structure.

With no `key`, it returns the number of distinct keys. With a `key`, it returns
the number of values associated with that key.

Missing keys return `0`.

### `record.contains rec key value?`

Tests whether the record contains the given key. If a value is provided,
tests whether any value associated with that key matches.

#### Parameters

| Name    | Type           | Description                             |
| ------- | -------------- | --------------------------------------- |
| `rec`   | `record`       | the record to check                     |
| `key`   | `int` or `sym` | the key to check                        |
| `value` |                | optional value to check for (multi-map) |

#### Returns

`bool`

```
let r = record 1 2 3 a: "first"
r.insert :a: "second"

# Key-only check
assert (record.contains $r 0)
assert (record.contains $r :a:)
assert (!record.contains $r 10)

# Key + value check
assert (record.contains $r :a: "first")
assert (record.contains $r :a: "second")
```
