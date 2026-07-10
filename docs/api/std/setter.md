# Setter

Abstract type for class-field setter objects.

`Setter` is not constructible directly.

## Behavior

When a class body contains one member value that is a nominal subtype of
`Setter`, that member name contributes the write behavior for a computed field.

For a computed field assignment `obj.name = value`:

- writing `obj.name = value` calls `setter.set(obj, value)`

Class creation pairs at most one setter and at most one getter with the same
member name. Any other duplicate class member name is an error.

Assigning a setter object into an instance field later does not change that
field into a computed field.

## Methods

### `set obj value`

Updates the field value for `obj`.

**Parameters:**

| Name    | Type | Description             |
| ------- | ---- | ----------------------- |
| `obj`   |      | instance being assigned |
| `value` |      | new field value         |

**Returns:** setter result

## Example

```
class PortSetter: Setter
  pub def set self obj value
    obj.#port = value
```
