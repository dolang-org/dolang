# `Ace`

Immutable view of a native Windows access-control entry.

## Class Methods

### `allow sid mask ...options`

Constructs an access-allowed ACE.

**Parameters:**

| Name                    | Type                                             | Description               |
| ----------------------- | ------------------------------------------------ | ------------------------- |
| `sid`                   | [`Sid`](./sid.md)                                | Trustee                   |
| `mask`                  | [`int`](../../std/int.md)                        | Access mask               |
| `flags`                 | [`int`](../../std/int.md)?                       | Native ACE flags          |
| `object_type`           | [`sys.windows.Guid`](../../sys/windows/guid.md)? | Object type               |
| `inherited_object_type` | [`sys.windows.Guid`](../../sys/windows/guid.md)? | Inherited object type     |
| `callback`              | [`bool`](../../std/bool.md)?                     | Build a callback ACE      |
| `application_data`      | [`bin`](../../std/bin.md)?                       | Trailing application data |

**Returns:** `Ace`

Application data is zero-padded to DWORD (32-bit) alignment.

### `deny sid mask ...options`

Constructs an access-denied ACE. Parameters match
[`allow`](#allow-sid-mask-options).

**Returns:** `Ace`

### `audit sid mask :successful :failed ...options`

Constructs a system-audit ACE.

**Parameters:**

| Name         | Type                        | Description             |
| ------------ | --------------------------- | ----------------------- |
| `sid`        | [`Sid`](./sid.md)           | Trustee                 |
| `mask`       | [`int`](../../std/int.md)   | Access mask             |
| `successful` | [`bool`](../../std/bool.md) | Audit successful access |
| `failed`     | [`bool`](../../std/bool.md) | Audit failed access     |

The remaining optional parameters match
[`allow`](#allow-sid-mask-options).

**Returns:** `Ace`

**Errors:**

- Raises `ValueError` when both outcomes are false or `flags` contains audit
  outcome bits.

## Fields

### `type`

Symbolic native ACE type, or `:UNKNOWN:` for an unrecognized type code.

| Code | Symbol                             |
| ---- | ---------------------------------- |
| 0    | `:ACCESS_ALLOWED:`                 |
| 1    | `:ACCESS_DENIED:`                  |
| 2    | `:SYSTEM_AUDIT:`                   |
| 3    | `:SYSTEM_ALARM:`                   |
| 4    | `:ACCESS_ALLOWED_COMPOUND:`        |
| 5    | `:ACCESS_ALLOWED_OBJECT:`          |
| 6    | `:ACCESS_DENIED_OBJECT:`           |
| 7    | `:SYSTEM_AUDIT_OBJECT:`            |
| 8    | `:SYSTEM_ALARM_OBJECT:`            |
| 9    | `:ACCESS_ALLOWED_CALLBACK:`        |
| 10   | `:ACCESS_DENIED_CALLBACK:`         |
| 11   | `:ACCESS_ALLOWED_CALLBACK_OBJECT:` |
| 12   | `:ACCESS_DENIED_CALLBACK_OBJECT:`  |
| 13   | `:SYSTEM_AUDIT_CALLBACK:`          |
| 14   | `:SYSTEM_ALARM_CALLBACK:`          |
| 15   | `:SYSTEM_AUDIT_CALLBACK_OBJECT:`   |
| 16   | `:SYSTEM_ALARM_CALLBACK_OBJECT:`   |
| 17   | `:SYSTEM_MANDATORY_LABEL:`         |
| 18   | `:SYSTEM_RESOURCE_ATTRIBUTE:`      |
| 19   | `:SYSTEM_SCOPED_POLICY_ID:`        |
| 20   | `:SYSTEM_PROCESS_TRUST_LABEL:`     |
| 21   | `:SYSTEM_ACCESS_FILTER:`           |

### `type_code`

Native numeric ACE type code.

### `flags`

Native ACE flags byte.

### `size`

Declared ACE packet size.

### `mask`

Native access mask.

Raises `FieldError` for an ACE layout without a projected mask.

### `sid`

Trustee [`Sid`](./sid.md).

Raises `FieldError` for an ACE layout without a projected SID.

### `object_flags`

Native object ACE flags.

Raises `FieldError` for a non-object ACE.

### `object_type`

Object-type [`sys.windows.Guid`](../../sys/windows/guid.md), or `nil` when the
object flag is clear.

Raises `FieldError` for a non-object ACE.

### `inherited_object_type`

Inherited-object-type [`sys.windows.Guid`](../../sys/windows/guid.md), or `nil`
when the object flag is clear.

Raises `FieldError` for a non-object ACE.

### `application_data`

Exact bytes after the projected SID. The value can be empty.

Raises `FieldError` when the ACE body is not interpreted.

### `object_inherit`

Whether non-container child objects inherit this ACE.

### `container_inherit`

Whether container child objects inherit this ACE.

### `no_propagate_inherit`

Whether inherited copies stop propagating after one generation.

### `inherit_only`

Whether this ACE applies only through inheritance.

### `inherited`

Whether this ACE was inherited.

### `critical`

Whether the native critical flag is set.

### `successful_access`

Whether an audit or alarm ACE selects successful access.

Raises `FieldError` for other ACE types.

### `failed_access`

Whether an audit or alarm ACE selects failed access.

Raises `FieldError` for other ACE types.

### `trust_protected_filter`

Whether an access-filter ACE has the trust-protected flag.

Raises `FieldError` for other ACE types.

## Methods

### `to_bin()`

Returns the exact native ACE packet, including application or unknown data.

**Returns:** [`bin`](../../std/bin.md)
