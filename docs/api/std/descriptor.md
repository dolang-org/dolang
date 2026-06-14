# Descriptor

Abstract type for descriptor-backed class fields.

`Descriptor` is not constructible directly.

## Behavior

When a class body contains a member which is an instance of `Descriptor`, that
field becomes descriptor-backed for all instances of the class.

For a descriptor-backed field `obj.name`:

- reading `obj.name` calls `descriptor.get(obj)`
- writing `obj.name = value` calls `descriptor.set(obj, value)`

Descriptor classification happens when the class is created. Assigning a
descriptor object into an instance field later does not turn that field into a
descriptor-backed field.

## Methods

### `get obj`

Returns the field value for `obj`.

**Parameters:**

| Name  | Type | Description              |
| ----- | ---- | ------------------------ |
| `obj` |      | instance being accessed  |

**Returns:** field value

### `set obj value`

Updates the field value for `obj`.

**Parameters:**

| Name    | Type | Description              |
| ------- | ---- | ------------------------ |
| `obj`   |      | instance being assigned  |
| `value` |      | new field value          |

**Returns:** setter result

## Example

```
class Twice: Descriptor
  pub def get self obj
    (obj.base * 2)

class Value
  pub let base = 21
  pub let doubled = Twice()

assert_eq $Value().doubled 42
```
