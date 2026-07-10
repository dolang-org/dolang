# Getter

Abstract type for class-field getter objects.

`Getter` is not constructible directly.

## Behavior

When a class body contains one member value that is a nominal subtype of
`Getter`, that member name becomes a computed field.

For a computed field `obj.name`:

- reading `obj.name` calls `getter.get(obj)`
- calling `obj.name(...)` first calls `getter.get(obj)`, then calls the result

Class creation pairs at most one getter and at most one setter with the same
member name. Any other duplicate class member name is an error.

Assigning a getter object into an instance field later does not change that
field into a computed field.

## Methods

### `get obj`

Returns the field value for `obj`.

**Parameters:**

| Name  | Type | Description             |
| ----- | ---- | ----------------------- |
| `obj` |      | instance being accessed |

**Returns:** field value

## Example

```
class Twice: Getter
  pub def get self obj
    (obj.base * 2)

class Value
  pub let base = 21
  pub let doubled = Twice()

assert_eq $Value().doubled 42
```
