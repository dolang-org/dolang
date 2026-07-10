# Getter

Abstract type for class-field getter objects.

For a computed field `obj.name`:

- reading `obj.name` calls `getter.get(obj)`
- calling `obj.name(...)` first calls `getter.get(obj)`, then calls the result

## Methods

### `get obj`

Returns the field value for `obj`.

**Parameters:**

| Name  | Type | Description             |
| ----- | ---- | ----------------------- |
| `obj` |      | instance being accessed |

**Returns:** field value
