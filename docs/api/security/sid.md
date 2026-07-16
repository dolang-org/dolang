# Sid

Windows security identifier.

## Constructor

### `Sid(value)`

Constructs a SID from its canonical string or native binary representation.

**Parameters:**

| Name    | Type                                           | Description        |
| ------- | ---------------------------------------------- | ------------------ |
| `value` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | SID representation |

**Returns:** `Sid`

## Fields

### `revision`

SID revision number.

### `identifier_authority`

The 48-bit identifier authority as an integer.

### `sub_authority_count`

Number of sub-authorities.

### `sub_authorities`

Sub-authorities as an immutable [`tuple`](../std/tuple.md).

## Methods

### `to_bin()`

Returns the native Windows packet representation.

**Returns:** [`bin`](../std/bin.md)

```
let sid = Sid S-1-5-32-544
echo $sid.identifier_authority
let encoded = sid.to_bin()
```
