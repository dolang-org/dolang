# toml

TOML serialization and deserialization.

## Functions

### `to_str value`

Serializes a Do value to a TOML string.

#### Parameters

| Name    | Type | Description            |
| ------- | ---- | ---------------------- |
| `value` |      | the value to serialize |

#### Returns

`str` -- TOML string

#### Errors

| Exception   | Condition                                                                                                                   |
| ----------- | --------------------------------------------------------------------------------------------------------------------------- |
| `TypeError` | A value is not TOML-representable, including `nil`, binary data, custom objects, or a table with non-string/non-symbol keys |

Type mapping:

| Do Type  | TOML Value |
| -------- | ---------- |
| `bool`   | boolean    |
| `int`    | integer    |
| `float`  | float      |
| `str`    | string     |
| `sym`    | string     |
| `array`  | array      |
| `tuple`  | array      |
| `dict`   | table      |
| `record` | table      |

Top-level `dict` and `record` values serialize as TOML documents. Other values
serialize as TOML values.

```
assert_eq (to_str 42) "42"
assert_eq (from_str $ to_str [1, 2, 3]) [1, 2, 3]

let doc = to_str {"name": "alice", "enabled": true}
assert_eq (from_str $doc) {"name": "alice", "enabled": true}
```

### `from_str toml`

Parses a TOML string into a Do value.

#### Parameters

| Name   | Type                  | Description         |
| ------ | --------------------- | ------------------- |
| `toml` | [`str`](./std/str.md) | TOML input to parse |

#### Returns

The parsed Do value.

#### Errors

| Exception    | Condition                                                                              |
| ------------ | -------------------------------------------------------------------------------------- |
| `ValueError` | The input is not valid TOML                                                            |
| `ValueError` | The input uses TOML datetime/date/time values, which are not mapped in this module yet |

Type mapping:

| TOML Value | Do Type |
| ---------- | ------- |
| boolean    | `bool`  |
| integer    | `int`   |
| float      | `float` |
| string     | `str`   |
| array      | `array` |
| table      | `dict`  |

`from_str` accepts both full TOML documents and bare TOML values such as
numbers, arrays, and inline tables.

```
assert_eq (from_str "42") 42
assert_eq (from_str "[1, 2, 3]") [1, 2, 3]

let doc = from_str |
  title = "Example"
  [server]
  port = 8080

assert_eq $doc["title"] "Example"
assert_eq $doc["server"]["port"] 8080
```
