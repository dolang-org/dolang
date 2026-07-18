# Guid

Windows globally unique identifier.

## Constructor

### `Guid(value)`

Parses a canonical GUID string or native Windows GUID packet.

**Parameters:**

| Name    | Type                                           | Description                        |
| ------- | ---------------------------------------------- | ---------------------------------- |
| `value` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | GUID text or 16-byte native packet |

**Returns:** `Guid`

**Errors:**

- Raises `ValueError` when the text or packet is malformed.

```
let id = Guid 00112233-4455-6677-8899-aabbccddeeff
echo $id
```

## Methods

### `to_bin()`

Returns the 16-byte native Windows GUID representation.

**Returns:** [`bin`](../std/bin.md)

## Operators

GUIDs support equality and hashing by value.
