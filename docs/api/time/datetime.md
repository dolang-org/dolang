# DateTime

A UTC instant represented as Unix seconds + nanoseconds.

## Type Methods

### `now()`

Returns the current UTC time.

```
echo $DateTime.now()
```

### `from_unix seconds :nanoseconds?`

Creates a `DateTime` from Unix epoch components.

**Parameters:**

| Name          | Type                   | Description                               |
| ------------- | ---------------------- | ----------------------------------------- |
| `seconds`     | [`int`](../std/int.md) | Whole seconds since Unix epoch            |
| `nanoseconds` | [`int`](../std/int.md) | Optional fractional nanoseconds component |

```
echo $ DateTime.from_unix 1700000000 123000000 
```

### `parse_rfc3339 text`

Parses an RFC3339 timestamp.

**Parameters:**

| Name   | Type                   | Description         |
| ------ | ---------------------- | ------------------- |
| `text` | [`str`](../std/str.md) | RFC3339 input text  |

**Returns:** [`DateTime`](./datetime.md)

**Errors:**

- The input is not a valid RFC3339 timestamp.

```
let dt = DateTime.parse_rfc3339("2024-01-02T03:04:05Z")
echo $dt.rfc3339()
```

## Fields

| Field         | Type                   | Description                               |
| ------------- | ---------------------- | ----------------------------------------- |
| `seconds`     | [`int`](../std/int.md) | Whole Unix seconds                        |
| `nanoseconds` | [`int`](../std/int.md) | Nanoseconds in range `[0, 1_000_000_000)` |

## Operators

- `DateTime - DateTime -> Duration`

## Methods

### `rfc3339()`

Returns the RFC3339 representation.

**Returns:** [`str`](../std/str.md)

```
let dt = DateTime.from_unix(1700000000)
echo $dt.rfc3339()
```

## String Form

`str(datetime)` renders the same UTC RFC3339 timestamp as `datetime.rfc3339()`.
