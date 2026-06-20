# args

Immutable argument pack.

`args` values are returned by variadic captures such as `...rest` and by
calling the `args` type object. Iterating an `args` value yields `[key, value]`
pairs. Positional items use their positional index as the key. Iterating or
spreading a pack does not consume it.

## Constructor

### `args ...`

Creates an argument pack from positional and keyed arguments.

**Returns:** `args`

```
let pack = args 1 name: Alice 2
assert_eq [...pack] [[0, 1], [:name:, "Alice"], [1, 2]]
```

## Fields

### `len`

Number of items in the argument pack.

```
let pack = args 1 2 name: Alice
assert_eq $pack.len 3
```

## Methods

### `pos_only`

Returns an iterator over positional values.

Raises [`UnexpectedKeyError`](./unexpected-key-error.md) if the pack contains
any keyed items.

```
let pack = args 1 2 3
let pos = pack.pos_only()
assert_eq [...pos] [1, 2, 3]
assert_eq [...pack] [[0, 1], [1, 2], [2, 3]]
```

### `pos_keys`

Returns a tuple of positional values and keyed entries.

The first item is an iterator over positional values. The second item is an
iterator over keyed `[key, value]` pairs.

```
let pack = args 1 left: 2 3 right: 4
let pos keyed = pack.pos_keys()
assert_eq [...pos] [1, 3]
assert_eq [...keyed] [[:left:, 2], [:right:, 4]]
assert_eq [...pack] [[0, 1], [:left:, 2], [1, 3], [:right:, 4]]
```
