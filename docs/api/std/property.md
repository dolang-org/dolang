# property

Descriptor helper for computed class fields.

The `property` type object is in the prelude, so `property(...)` can be used
without an explicit import.

## Constructor

### `property getter setter?`

Builds a property descriptor from a getter and optional setter.

**Parameters:**

| Name     | Type    | Description                       |
| -------- | ------- | --------------------------------- |
| `getter` | `func`  | callable used for field reads     |
| `setter` | `func`? | optional callable used for writes |

**Returns:** `property`

```
class Config
  pub let _port = 8080
  #[property]
  pub def port obj
    obj._port

  #[port.setter]
  pub def port obj value
    obj._port = value
```

## Inherits

[`Descriptor`](./descriptor.md)

## Methods

### `get obj`

Calls the stored getter with `obj`.

**Parameters:**

| Name  | Type | Description              |
| ----- | ---- | ------------------------ |
| `obj` |      | instance being accessed  |

**Returns:** getter result

### `set obj value`

Calls the stored setter with `obj` and `value`.

**Parameters:**

| Name    | Type | Description              |
| ------- | ---- | ------------------------ |
| `obj`   |      | instance being assigned  |
| `value` |      | new field value          |

**Returns:** setter result

**Errors:**

- Raises a type error if the property has no setter.

### `setter fn`

Replaces the stored setter and returns the property itself. This is intended
for decorator use inside a class body.

**Parameters:**

| Name | Type   | Description       |
| ---- | ------ | ----------------- |
| `fn` | `func` | setter to install |

**Returns:** `self`
