# Acl

Immutable view of a native Windows access-control list.

## Fields

### `revision`

Native ACL revision.

### `size`

Declared ACL packet size.

### `ace_count`

Number of access-control entries.

### `aces`

Lazy immutable array view of [`Ace`](./ace.md) values.

## Methods

### `to_bin()`

Returns the exact native ACL packet.

**Returns:** [`bin`](../std/bin.md)
