# args

Iterator over an argument pack.

`args` values are returned by variadic captures such as `...rest` and by
calling the `args` type object. Iterating an `args` value yields `[key, value]`
pairs. Positional items use their positional index as the key.

## Constructor

### `args ...`

Creates an argument-pack iterator from positional and keyed arguments.

**Returns:** `args`

```
let pack = args 1 name: Alice 2
assert_eq [...pack] [[0, 1], [:name:, "Alice"], [1, 2]]
```

## Fields

### `len`

Number of remaining items in the argument pack.

```
let rest = args 1 2 name: Alice
assert_eq $rest.len 3
```

## Methods

### `push ...args`

Appends arguments to the end of the pack.

```
let rest = args 1
rest.push 2 name: Alice
assert_eq [...rest] [[0, 1], [1, 2], [:name:, "Alice"]]
```

### `pos`

Consumes the remaining pack and returns an iterator over positional values.

Raises [`UnexpectedKeyError`](./unexpected-key-error.md) if any remaining item
is keyed.

```
let rest = args 1 2 3
let pos = rest.pos()
assert_eq [...pos] [1, 2, 3]
assert_eq (rest.next default: :done:) :done:
```

### `pos_keys`

Consumes the remaining pack and returns a tuple of positional values and keyed
arguments.

The first item is an iterator over positional values. The second item is an
`args` value containing only keyed items.

```
let pos keyed = (args 1 left: 2 3 right: 4).pos_keys()
assert_eq [...pos] [1, 3]
assert_eq [...keyed] [[:left:, 2], [:right:, 4]]
```
