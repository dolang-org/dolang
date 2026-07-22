# `SecDesc`

Windows security descriptor.

## Constructor

### `SecDesc value`

Parses a self-relative security descriptor.

**Parameters:**

| Name    | Type                      | Description                 |
| ------- | ------------------------- | --------------------------- |
| `value` | [`bin`](../../std/bin.md) | Native self-relative packet |

**Errors:**

- Raises `ValueError` when the packet is malformed or is not self-relative.

### `SecDesc ...options`

Constructs a security descriptor from components and control fields.

## Component options

The constructor and [`with`](#with-options) accept these options:

| Name                         | Type                             | Description                   |
| ---------------------------- | -------------------------------- | ----------------------------- |
| `owner`                      | [`Sid`](./sid.md)\|`nil`         | Owner, or loaded absent owner |
| `group`                      | [`Sid`](./sid.md)\|`nil`         | Group, or loaded absent group |
| `dacl`                       | [`Acl`](./acl.md)\|`nil`         | DACL, or present null DACL    |
| `sacl`                       | [`Acl`](./acl.md)\|`nil`         | SACL, or present null SACL    |
| `owner_defaulted`            | [`bool`](../../std/bool.md)      | Owner defaulted flag          |
| `group_defaulted`            | [`bool`](../../std/bool.md)      | Group defaulted flag          |
| `dacl_present`               | [`bool`](../../std/bool.md)      | DACL presence                 |
| `dacl_defaulted`             | [`bool`](../../std/bool.md)      | DACL defaulted flag           |
| `dacl_auto_inherit_required` | [`bool`](../../std/bool.md)      | DACL inheritance request      |
| `dacl_auto_inherited`        | [`bool`](../../std/bool.md)      | DACL inherited flag           |
| `dacl_protected`             | [`bool`](../../std/bool.md)      | DACL protection               |
| `sacl_present`               | [`bool`](../../std/bool.md)      | SACL presence                 |
| `sacl_defaulted`             | [`bool`](../../std/bool.md)      | SACL defaulted flag           |
| `sacl_auto_inherit_required` | [`bool`](../../std/bool.md)      | SACL inheritance request      |
| `sacl_auto_inherited`        | [`bool`](../../std/bool.md)      | SACL inherited flag           |
| `sacl_protected`             | [`bool`](../../std/bool.md)      | SACL protection               |
| `rm_control`                 | [`int`](../../std/int.md)\|`nil` | RM control byte, or clear it  |

Specifying ACL presence as `false` when using [`with`](#with-options) clears
that field. Specifying `true` for control flags requires the corresponding field
to be present or supplied.

## Fields

### `revision`

Security descriptor revision.

### `mask`

Native `SECURITY_INFORMATION` mask recording which fields are present.

Descriptors parsed from a self-relative packet have owner, group, DACL, and
SACL marked as loaded because the native packet does not carry a separate
mask. Descriptors derived from an object or file on Windows may have partial
information depending on what was queried.

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

### `dacl`

Discretionary [`Acl`](./acl.md), or `nil` for a present null ACL.

Raises `FieldError` when the DACL was not loaded or is not present.

### `sacl`

System [`Acl`](./acl.md), or `nil` for a present null ACL.

Raises `FieldError` when the SACL was not loaded or is not present.

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

### `with ...options`

Returns a descriptor with selected components or control fields replaced.

**Returns:** `SecDesc`

### `to_bin()`

Returns a canonical self-relative security descriptor packet.

**Returns:** [`bin`](../../std/bin.md)
