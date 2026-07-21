# `Acl`

Immutable view of a native Windows access-control list.

## Constructor

### `Acl aces revision: nil`

Constructs an ACL from an iterable of [`Ace`](./ace.md) values.

**Parameters:**

| Name       | Type                       | Description             |
| ---------- | -------------------------- | ----------------------- |
| `aces`     | iterable                   | Entries in packet order |
| `revision` | [`int`](../../std/int.md)? | Native revision 2 or 4  |

Revision 4 is selected when an object ACE is present; otherwise revision 2 is
selected. Supplying revision 2 with an object ACE raises `ValueError`.

## Fields

### `revision`

Native ACL revision.

### `size`

Declared ACL packet size.

### `ace_count`

Number of access-control entries.

### `aces`

Immutable array-like view of [`Ace`](./ace.md) values.

## Methods

### `to_bin()`

Returns the exact native ACL packet.

**Returns:** [`bin`](../../std/bin.md)
