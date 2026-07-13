# json

JSON serialization and deserialization.

## Functions

### `to_str value`

Serializes a Do value to a JSON string.

#### Parameters

| Name    | Type | Description            |
| ------- | ---- | ---------------------- |
| `value` |      | the value to serialize |

#### Returns

`str` -- JSON string

Type mapping:

| Do Type | JSON Type            |
| ------- | -------------------- |
| `nil`   | `null`               |
| `bool`  | boolean              |
| `int`   | number               |
| `float` | number               |
| `str`   | string               |
| `sym`   | string (symbol name) |
| `array` | array                |
| `dict`  | object               |

```
assert_eq (to_str 42) "42"
assert_eq (to_str "hello") "\"hello\""
assert_eq (to_str nil) "null"
```

### `from_str json`

Deserializes a JSON string to a Do value.

#### Parameters

| Name   | Type  | Description          |
| ------ | ----- | -------------------- |
| `json` | `str` | JSON string to parse |

#### Returns

The parsed Do value.

#### Errors

| Exception    | Condition           |
| ------------ | ------------------- |
| `ValueError` | The JSON is invalid |

Type mapping:

| JSON Type | Do Type |
| --------- | ------- |
| `null`    | `nil`   |
| boolean   | `bool`  |
| integer   | `int`   |
| float     | `float` |
| string    | `str`   |
| array     | `array` |
| object    | `dict`  |

```
assert_eq (from_str "null") nil
assert_eq (from_str "42") 42
assert_eq (from_str "[1, 2, 3]") [1, 2, 3]

let obj = from_str "{\"name\": \"Alice\", \"age\": 30}"
assert_eq $obj["name"] "Alice"
assert_eq $obj["age"] 30
```
