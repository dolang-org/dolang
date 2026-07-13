# set

Sets are ordered, mutable collections with unique membership semantics.

Iteration preserves insertion order. Reinserting an existing value is a no-op
and does not move it to the end.

## Fields

### `len`

Returns the number of members.

#### Type

[`int`](./index.md)

## Methods

### `add value`

Adds `value` if it is not already present.

#### Parameters

| Name    | Type | Description         |
| ------- | ---- | ------------------- |
| `value` |      | the value to insert |

```
let s = set [1, 2]
s.add 3
s.add 2
assert_eq [...s] [1, 2, 3]
```

### `delete value`

Removes `value` if present.

#### Parameters

| Name    | Type | Description         |
| ------- | ---- | ------------------- |
| `value` |      | the value to remove |

#### Returns

[`bool`](./index.md) indicating whether a value was removed

### `clear`

Removes all members.

```
let s = set [1, 2]
s.clear()
assert_eq $s.len 0
```

### `copy`

Returns a shallow copy of the set.

Insertion order is preserved. Contained values are *not* recursively copied.

#### Returns

[`set`](./set.md)

### `contains value`

Tests whether the set contains `value`.

#### Parameters

| Name    | Type | Description       |
| ------- | ---- | ----------------- |
| `value` |      | the value to test |

#### Returns

[`bool`](./index.md)

### `union other`

Returns a new set containing all members from `self`, then first-seen members
from `other`.

#### Parameters

| Name    | Type              | Description |
| ------- | ----------------- | ----------- |
| `other` | [`set`](./set.md) | other set   |

#### Returns

[`set`](./set.md)

### `intersect other`

Returns a new set containing members that are present in both sets, in
the receiver's insertion order.

#### Parameters

| Name    | Type              | Description |
| ------- | ----------------- | ----------- |
| `other` | [`set`](./set.md) | other set   |

#### Returns

[`set`](./set.md)

### `diff other`

Returns a new set containing members from the receiver that are not present in
`other`.

#### Parameters

| Name    | Type              | Description |
| ------- | ----------------- | ----------- |
| `other` | [`set`](./set.md) | other set   |

#### Returns

[`set`](./set.md)

### `sym_diff other`

Returns a new set containing members present in exactly one of the two
sets.

#### Parameters

| Name    | Type              | Description |
| ------- | ----------------- | ----------- |
| `other` | [`set`](./set.md) | other set   |

#### Returns

[`set`](./set.md)

### `is_subset other`

Returns `true` when every member of the receiver is present in `other`.

#### Parameters

| Name    | Type              | Description |
| ------- | ----------------- | ----------- |
| `other` | [`set`](./set.md) | other set   |

#### Returns

[`bool`](./index.md)

### `is_superset other`

Returns `true` when every member of `other` is present in the receiver.

#### Parameters

| Name    | Type              | Description |
| ------- | ----------------- | ----------- |
| `other` | [`set`](./set.md) | other set   |

#### Returns

[`bool`](./index.md)

## Operations

### Iteration

Iterating a set yields values in insertion order:

```
let s = set [3, 1, 2, 1]
assert_eq [...s] [3, 1, 2]
```
