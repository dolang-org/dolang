# json

JSON serialization and deserialization.

## Functions

### `decode json`

Deserializes a JSON string to a Do value.

#### Parameters

| Name   | Type  | Description          |
| ------ | ----- | -------------------- |
| `json` | `Str` | JSON string to parse |

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
| boolean   | `Bool`  |
| integer   | `Int`   |
| float     | `Float` |
| string    | `Str`   |
| array     | `array` |
| object    | `dict`  |

#### Example

```
assert_eq (decode "null") nil
assert_eq (decode "42") 42
assert_eq (decode "[1, 2, 3]") [1, 2, 3]

let obj = decode "{\"name\": \"Alice\", \"age\": 30}"
assert_eq $obj["name"] "Alice"
assert_eq $obj["age"] 30
```

### `encode value :indent?`

Serializes a Do value to a JSON string.

#### Parameters

| Name     | Type                   | Description                             |
| -------- | ---------------------- | --------------------------------------- |
| `value`  |                        | the value to serialize                  |
| `indent` | [`Int`](./std/int.md)? | spaces per level; one line when omitted |

#### Returns

`Str` -- JSON string

Type mapping:

| Do Type | JSON Type            |
| ------- | -------------------- |
| `nil`   | `null`               |
| `Bool`  | boolean              |
| `Int`   | number               |
| `Float` | number               |
| `Str`   | string               |
| `Sym`   | string (symbol name) |
| `array` | array                |
| `dict`  | object               |

#### Example

```
assert_eq (encode 42) "42"
assert_eq (encode "hello") "\"hello\""
assert_eq (encode nil) "null"
assert_eq (encode [1, 2] indent: 2) "[\n  1,\n  2\n]"
```

Object keys are written in the order the dict holds them.
