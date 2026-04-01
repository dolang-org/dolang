# base64

Base64 encoding and decoding.

## Functions

### `encode data`

Encodes a string or binary value using standard RFC 4648 base64 with padding.

**Parameters:**

| Name   | Type                                         | Description      |
| ------ | -------------------------------------------- | ---------------- |
| `data` | [`str`](./std/str.md)\|[`bin`](./std/bin.md) | data to encode   |

**Returns:** [`str`](./std/str.md) - Base64 text

```
assert_eq (encode "") ""
assert_eq (encode "hello") "aGVsbG8="
assert_eq (encode b"hello") "aGVsbG8="
assert_eq (encode b"\x00\x01\x02") "AAEC"
```

### `decode text`

Decodes standard RFC 4648 base64 text with padding and returns the raw bytes.

**Parameters:**

| Name   | Type                  | Description           |
| ------ | --------------------- | --------------------- |
| `text` | [`str`](./std/str.md) | base64 text to decode |

**Returns:** [`bin`](./std/bin.md) - Decoded bytes

**Errors:**

- Raises a type error if `text` is not a string
- Raises an error if the input is not valid base64

```
assert_eq (decode "aGVsbG8=") b"hello"
assert_eq (decode $ encode "hello") b"hello"
assert_eq (decode $ encode b"\x00\x01\x02") b"\x00\x01\x02"
```
