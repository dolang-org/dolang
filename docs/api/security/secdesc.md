# SecDesc

Windows security descriptor with opaque access-control lists.

## Constructor

### `SecDesc(value)`

Parses a self-relative security descriptor.

**Parameters:**

| Name    | Type                     | Description                       |
| ------- | ------------------------ | --------------------------------- |
| `value` | [`bin`](../std/bin.md)   | Native self-relative packet       |

**Returns:** `SecDesc`

**Errors:**

- Raises `ValueError` when the packet is malformed or is not self-relative.

## Fields

### `revision`

Security descriptor revision.

### `mask`

Native `SECURITY_INFORMATION` mask recording which components were loaded.

Descriptors parsed from a self-relative packet have owner, group, DACL, and
SACL marked as loaded because the native packet does not carry a separate
mask.

### `control`

Native control flags, excluding the self-relative storage flag.

### `rm_control_valid`

Whether the resource-manager control byte is valid.

### `rm_control`

Resource-manager-defined control byte.

Raises `FieldError` when `rm_control_valid` is false.

### `owner`

Owner [`Sid`](./sid.md).

Raises `FieldError` when the owner was not loaded or is absent.

### `group`

Primary group [`Sid`](./sid.md).

Raises `FieldError` when the group was not loaded or is absent.

### `owner_defaulted`

Whether the owner was supplied by a default mechanism.

### `group_defaulted`

Whether the group was supplied by a default mechanism.

### `dacl_present`

Whether the DACL is present. A present ACL can be null.

### `dacl_defaulted`

Whether the DACL was supplied by a default mechanism.

### `dacl_auto_inherit_required`

Whether DACL inheritance computation was requested.

### `dacl_auto_inherited`

Whether the DACL was produced through inheritance.

### `dacl_protected`

Whether the DACL is protected from inheritance.

### `sacl_present`

Whether the SACL is present. A present ACL can be null.

### `sacl_defaulted`

Whether the SACL was supplied by a default mechanism.

### `sacl_auto_inherit_required`

Whether SACL inheritance computation was requested.

### `sacl_auto_inherited`

Whether the SACL was produced through inheritance.

### `sacl_protected`

Whether the SACL is protected from inheritance.

ACL-related fields raise `FieldError` when the corresponding ACL was not
loaded.

## Methods

### `to_bin()`

Returns a canonical self-relative security descriptor packet.

**Returns:** [`bin`](../std/bin.md)
