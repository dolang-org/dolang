# yaml

YAML serialization and deserialization.

## Functions

### `to_str value`

Serializes a Do value to a YAML string.

#### Parameters

| Name    | Type | Description            |
| ------- | ---- | ---------------------- |
| `value` |      | the value to serialize |

#### Returns

`str` -- YAML string

#### Errors

| Exception   | Condition                                                                 |
| ----------- | ------------------------------------------------------------------------- |
| `TypeError` | A value is not YAML-representable, such as binary data or a custom object |

Type mapping:

| Do Type | YAML Value |
| ------- | ---------- |
| `nil`   | `null`     |
| `bool`  | boolean    |
| `int`   | integer    |
| `float` | float      |
| `str`   | string     |
| `sym`   | string     |
| `array` | sequence   |
| `dict`  | mapping    |

```
assert_eq (to_str nil) "~"
assert_eq (to_str [1, 2, 3]) "- 1\n- 2\n- 3"
assert_eq (from_str $ to_str [1, 2, 3]) [1, 2, 3]
```

### `from_str yaml`

Parses a YAML string into a Do value.

#### Parameters

| Name   | Type                  | Description            |
| ------ | --------------------- | ---------------------- |
| `yaml` | [`str`](./std/str.md) | YAML document to parse |

#### Returns

The parsed Do value.

#### Errors

| Exception    | Condition                                                                      |
| ------------ | ------------------------------------------------------------------------------ |
| `ValueError` | The input is not valid YAML                                                    |
| `ValueError` | The input contains more than one document                                      |
| `ValueError` | The input uses currently unsupported YAML features, including tags and aliases |

Type mapping:

| YAML Value | Do Type |
| ---------- | ------- |
| `null`     | `nil`   |
| boolean    | `bool`  |
| integer    | `int`   |
| float      | `float` |
| string     | `str`   |
| sequence   | `array` |
| mapping    | `dict`  |

```
let doc = from_str |
  name: Alice
  enabled: true
  ports:
    - 80
    - 443

assert_eq $doc["name"] "Alice"
assert_eq $doc["enabled"] true
assert_eq $doc["ports"] [80, 443]
```
