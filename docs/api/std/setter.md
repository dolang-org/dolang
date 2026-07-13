# Setter

Abstract type for class-field setter objects.

For a computed field assignment `obj.name = value`:

- writing `obj.name = value` calls `setter.set(obj, value)`

## Methods

### `set obj value`

Updates the field value for `obj`.

#### Parameters

| Name    | Type | Description             |
| ------- | ---- | ----------------------- |
| `obj`   |      | instance being assigned |
| `value` |      | new field value         |

#### Returns

setter result
