# bin

Binary data; an immutable sequence of bytes.

## Fields

### `len`

Returns the byte length of the binary data.

**Type:** [`int`](./index.md)

```
assert_eq b"hello".len 5
assert_eq b"".len 0
```

## Instance Methods

### `starts_with prefix`

Tests whether the binary data starts with the given prefix.

**Parameters:**

| Name     | Type              | Description      |
| -------- | ----------------- | ---------------- |
| `prefix` | [`bin`](./bin.md) | the prefix bytes |

**Returns:** [`bool`](./index.md)

```
assert (b"hello".starts_with b"he")
assert (!(b"hello".starts_with b"lo"))
```

### `without_prefix prefix`

Returns the binary data with the prefix removed if it matches, otherwise
returns the original data.

**Parameters:**

| Name     | Type              | Description          |
| -------- | ----------------- | -------------------- |
| `prefix` | [`bin`](./bin.md) | the prefix to remove |

**Returns:** [`bin`](./bin.md)

```
assert_eq (b"hello".without_prefix b"he") b"lo"
assert_eq (b"hello".without_prefix b"xx") b"hello"
```

### `ends_with suffix`

Tests whether the binary data ends with the given suffix.

**Parameters:**

| Name     | Type              | Description      |
| -------- | ----------------- | ---------------- |
| `suffix` | [`bin`](./bin.md) | the suffix bytes |

**Returns:** [`bool`](./index.md)

```
assert (b"hello".ends_with b"lo")
```

### `without_suffix suffix`

Returns the binary data with the suffix removed if it matches, otherwise returns
the original data.

**Parameters:**

| Name     | Type              | Description          |
| -------- | ----------------- | -------------------- |
| `suffix` | [`bin`](./bin.md) | the suffix to remove |

**Returns:** [`bin`](./bin.md)

```
assert_eq (b"hello".without_suffix b"lo") b"hel"
```

### `split delimiter [limit: int]`

Splits the binary data by the delimiter, returning an iterator that yields
segments in **left-to-right** order.

The optional `limit` works identically to
[`str.split`](./str.md#split-delimiter-limit-int): positive splits from the
left, negative splits from the right (but still yields left-to-right).

**Parameters:**

| Name        | Type                | Description                                 |
| ----------- | ------------------- | ------------------------------------------- |
| `delimiter` | [`bin`](./bin.md)   | the delimiter bytes                         |
| `limit`     | [`int`](./index.md) | max splits; negative means split from right |

**Returns:** iterator of [`bin`](./bin.md)

```
assert_eq [...b"a,b,c".split b","] [b"a", b"b", b"c"]
assert_eq [...b"a,b,c".split b"," limit: 1] [b"a", b"b,c"]
let base ext = b"archive.tar.gz".split b"." limit: -1
assert_eq $base b"archive.tar"
assert_eq $ext b"gz"
```

### `rsplit delimiter [limit: int]`

Like `split`, but yields segments in **right-to-left** order. Mirrors
[`str.rsplit`](./str.md#rsplit-delimiter-limit-int).

**Parameters:**

| Name        | Type                | Description                                |
| ----------- | ------------------- | ------------------------------------------ |
| `delimiter` | [`bin`](./bin.md)   | the delimiter bytes                        |
| `limit`     | [`int`](./index.md) | max splits; negative means split from left |

**Returns:** iterator of [`bin`](./bin.md)

```
assert_eq [...b"a,b,c".rsplit b","] [b"c", b"b", b"a"]
assert_eq [...b"a,b,c".rsplit b"," limit: 1] [b"c", b"a,b"]
```

### `join iter?`

Joins values from an input source using this binary data as a separator.

**Parameters:**

| Name    | Type | Description                                      |
| ------- | ---- | ------------------------------------------------ |
| `input` |      | iterable to join (uses default input if omitted) |

**Returns:** [`bin`](./bin.md)

```
assert_eq (b",".join [b"a", b"b", b"c"]) b"a,b,c"
```

### `trim chars?`

Removes bytes (or specified characters) from both ends.

**Parameters:**

| Name    | Type              | Description                                  |
| ------- | ----------------- | -------------------------------------------- |
| `chars` | [`bin`](./bin.md) | bytes to trim (defaults to whitespace bytes) |

**Returns:** [`bin`](./bin.md)

```
assert_eq b"  hello  ".trim() b"hello"
assert_eq (b"xxhelloxx".trim b"x") b"hello"
```

### `trim_start chars?`

Removes bytes (or specified characters) from the start.

**Parameters:**

| Name    | Type              | Description   |
| ------- | ----------------- | ------------- |
| `chars` | [`bin`](./bin.md) | bytes to trim |

**Returns:** [`bin`](./bin.md)

```
assert_eq b"  hello  ".trim_start() b"hello  "
```

### `trim_end chars?`

Removes bytes (or specified characters) from the end.

**Parameters:**

| Name    | Type              | Description   |
| ------- | ----------------- | ------------- |
| `chars` | [`bin`](./bin.md) | bytes to trim |

**Returns:** [`bin`](./bin.md)

```
assert_eq b"  hello  ".trim_end() b"  hello"
```

### `contains needle`

Tests whether the binary data contains the given bytes.

**Parameters:**

| Name     | Type              | Description       |
| -------- | ----------------- | ----------------- |
| `needle` | [`bin`](./bin.md) | the bytes to find |

**Returns:** [`bool`](./index.md)

```
assert (b"hello".contains b"ell")
assert (b"hello".contains b"lo"))
assert (!(b"hello".contains b"world"))
assert (b"hello".contains b""))
```

### `sub start end?`

Returns a slice from `start` to `end` (exclusive). If `end` is omitted,
returns from `start` to the end. Negative indexes count from the end.

**Parameters:**

| Name    | Type                | Description                   |
| ------- | ------------------- | ----------------------------- |
| `start` | [`int`](./index.md) | starting byte index           |
| `end`   | [`int`](./index.md) | ending byte index (exclusive) |

**Returns:** [`bin`](./bin.md)

```
assert_eq (b"hello".sub 2) b"llo"
assert_eq (b"hello".sub 1 4) b"ell"
assert_eq (b"hello".sub -2) b"lo"
assert_eq (b"hello".sub 1 -1) b"ell"
```

### `unpack`

Unpacks binary data into an array of byte values (integers from 0-255).

**Returns:** [`array`](./array.md) of [`int`](./index.md)

```
let bytes = b"hello"
assert_eq $bytes.unpack() [104, 101, 108, 108, 111]
```

### `hex`

Returns the binary data as a lowercase hexadecimal string.

**Returns:** [`str`](./str.md)

```
assert_eq (b"ABC".hex()) "414243"
assert_eq (b"\x00\x01\xff".hex()) "0001ff"
```

## Constructors

### `bin value`

Converts a value to binary data. If the value is already binary, returns it
directly. Otherwise, converts via string representation.

**Parameters:**

| Name    | Type | Description          |
| ------- | ---- | -------------------- |
| `value` |      | value to convert     |

**Returns:** [`bin`](./bin.md)

```
# From string
let data = b"hello"
echo $data  # hello
```

## Class Methods

### `pack array`

Packs an array of integers (0-255) into binary data.

**Parameters:**

| Name    | Type                  | Description               |
| ------- | --------------------- | ------------------------- |
| `array` | [`array`](./array.md) | array of integers (0-255) |

**Returns:** [`bin`](./bin.md)

```
let bytes = bin.pack [104, 101, 108, 108, 111]
assert_eq $bytes b"hello"
```

### `unpack value`

Unpacks any value that can be converted to binary into an array of byte values.

**Parameters:**

| Name    | Type | Description                        |
| ------- | ---- | ---------------------------------- |
| `value` |      | value to unpack (converted to bin) |

**Returns:** [`array`](./array.md) of [`int`](./index.md)

```
assert_eq (bin.unpack b"hello") [104, 101, 108, 108, 111]
```
