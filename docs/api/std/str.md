# str

Strings are immutable sequences of UTF-8 bytes.

## Fields

### `len`

Returns the byte length of the string.

**Type:** [`int`](./index.md)

```
assert_eq $"hello".len 5
assert_eq $"".len 0
```

## Methods

### `starts_with prefix`

Tests whether the string starts with the given prefix.

**Parameters:**

| Name     | Type              | Description       |
| -------- | ----------------- | ----------------- |
| `prefix` | [`str`](./str.md) | the prefix string |

**Returns:** [`bool`](./index.md)

```
assert ("foobar".starts_with "foo")
assert (!("foobar".starts_with "bar"))
```

### `without_prefix prefix`

Returns the string with the prefix removed if it matches, otherwise returns
the original string.

**Parameters:**

| Name     | Type              | Description          |
| -------- | ----------------- | -------------------- |
| `prefix` | [`str`](./str.md) | the prefix to remove |

**Returns:** [`str`](./str.md)

```
assert_eq ("foobar".without_prefix "foo") "bar"
assert_eq ("foobar".without_prefix "baz") "foobar"
```

### `ends_with suffix`

Tests whether the string ends with the given suffix.

**Parameters:**

| Name     | Type              | Description       |
| -------- | ----------------- | ----------------- |
| `suffix` | [`str`](./str.md) | the suffix string |

**Returns:** [`bool`](./index.md)

```
assert ("foobar".ends_with "bar")
```

### `without_suffix suffix`

Returns the string with the suffix removed if it matches, otherwise returns
the original string.

**Parameters:**

| Name     | Type              | Description          |
| -------- | ----------------- | -------------------- |
| `suffix` | [`str`](./str.md) | the suffix to remove |

**Returns:** [`str`](./str.md)

```
assert_eq ("foobar".without_suffix "bar") "foo"
```

### `split delimiter [limit: int]`

Splits the string by the delimiter, returning an iterator that yields segments
in **left-to-right** order.

The optional `limit` controls how many splits are performed and from which end:

- `limit: N` (positive) — split at most N times from the **left**; the last
  element is the unsplit remainder.
- `limit: -N` (negative) — split at most N times from the **right**, but still
  yield segments left-to-right. Useful for splitting off a known-length suffix
  (e.g. a file extension).
- Omitted — split fully with no limit.

**Parameters:**

| Name        | Type                | Description                                 |
| ----------- | ------------------- | ------------------------------------------- |
| `delimiter` | [`str`](./str.md)   | the delimiter string                        |
| `limit`     | [`int`](./index.md) | max splits; negative means split from right |

**Returns:** iterator of [`str`](./str.md)

```
assert_eq [..."a,b,c".split ","] ["a", "b", "c"]
assert_eq [..."a,b,c".split "," limit: 1] ["a", "b,c"]

# Negative limit: split from the right, yield left-to-right
let base ext = "archive.tar.gz".split "." limit: -1
assert_eq $base "archive.tar"
assert_eq $ext "gz"
```

### `rsplit delimiter [limit: int]`

Like `split`, but yields segments in **right-to-left** order (rightmost segment
first).

The optional `limit` controls how many splits are performed and from which end:

- `limit: N` (positive) — split at most N times from the **right**; the last
  element yielded is the unsplit left remainder.
- `limit: -N` (negative) — split at most N times from the **left**, but still
  yield segments right-to-left.
- Omitted — split fully with no limit.

**Parameters:**

| Name        | Type                | Description                                |
| ----------- | ------------------- | ------------------------------------------ |
| `delimiter` | [`str`](./str.md)   | the delimiter string                       |
| `limit`     | [`int`](./index.md) | max splits; negative means split from left |

**Returns:** iterator of [`str`](./str.md)

```
assert_eq [..."a,b,c".rsplit ","] ["c", "b", "a"]
assert_eq [..."a,b,c".rsplit "," limit: 1] ["c", "a,b"]

# Negative limit: split from the left, yield right-to-left
assert_eq [..."a,b,c".rsplit "," limit: -1] ["b,c", "a"]
```

### `join iter?`

Joins values from an input source using this string as a separator.

**Parameters:**

| Name    | Type | Description                                      |
| ------- | ---- | ------------------------------------------------ |
| `input` |      | iterable to join (uses default input if omitted) |

**Returns:** [`str`](./str.md)

```
assert_eq (",".join ["a", "b", "c"]) "a,b,c"
```

### `trim chars?`

Removes whitespace (or specified characters) from both ends.

**Parameters:**

| Name    | Type  | Description                                 |
| ------- | ----- | ------------------------------------------- |
| `chars` | `str` | characters to trim (defaults to whitespace) |

**Returns:** [`str`](./str.md)

```
assert_eq ("  hello  ".trim()) "hello"
assert_eq ("xxhelloxx".trim "x") "hello"
```

### `trim_start chars?`

Removes whitespace (or specified characters) from the start.

**Parameters:**

| Name    | Type              | Description        |
| ------- | ----------------- | ------------------ |
| `chars` | [`str`](./str.md) | characters to trim |

**Returns:** [`str`](./str.md)

```
assert_eq ("  hello  ".trim_start()) "hello  "
```

### `trim_end chars?`

Removes whitespace (or specified characters) from the end.

**Parameters:**

| Name    | Type              | Description        |
| ------- | ----------------- | ------------------ |
| `chars` | [`str`](./str.md) | characters to trim |

**Returns:** [`str`](./str.md)

```
assert_eq ("  hello  ".trim_end()) "  hello"
```

### `contains needle`

Tests whether the string contains the given substring.

**Parameters:**

| Name     | Type              | Description           |
| -------- | ----------------- | --------------------- |
| `needle` | [`str`](./str.md) | the substring to find |

**Returns:** [`bool`](./index.md)

```
assert ("foobar".contains "foo")
assert ("foobar".contains "bar")
assert (!"foobar".contains "baz")
assert ("foobar".contains "")  # empty string is always contained
```

### `replace from to`

Returns a new string with all non-overlapping occurrences of `from` replaced
with `to`.

**Parameters:**

| Name   | Type              | Description                |
| ------ | ----------------- | -------------------------- |
| `from` | [`str`](./str.md) | substring to replace       |
| `to`   | [`str`](./str.md) | replacement string         |

**Returns:** [`str`](./str.md)

```
assert_eq ("foo bar foo".replace "foo" "baz") "baz bar baz"
assert_eq ("banana".replace "na" "") "ba"
assert_eq ("abc".replace "" "-") "-a-b-c-"
```

### `sub start end?`

Returns a substring from `start` to `end` (exclusive). If `end` is omitted,
returns from `start` to the end of the string. Negative indexes count from the
end using byte offsets.

**Parameters:**

| Name    | Type                | Description                   |
| ------- | ------------------- | ----------------------------- |
| `start` | [`int`](./index.md) | starting byte index           |
| `end`   | [`int`](./index.md) | ending byte index (exclusive) |

**Returns:** [`str`](./str.md)

```
assert_eq ("foobar".sub 3) "bar"
assert_eq ("foobar".sub 2 4) "ob"
assert_eq ("foobar".sub -3) "bar"
assert_eq ("foobar".sub 1 -1) "ooba"
```

### `upper`

Returns the string converted to uppercase.

**Returns:** [`str`](./str.md)

```
assert_eq ("hello".upper()) "HELLO"
assert_eq ("Hello World".upper()) "HELLO WORLD"
```

### `lower`

Returns the string converted to lowercase.

**Returns:** [`str`](./str.md)

```
assert_eq ("HELLO".lower()) "hello"
assert_eq ("Hello World".lower()) "hello world"
```

### `repeat count`

Returns the string repeated `count` times.

**Parameters:**

| Name    | Type                | Description                     |
| ------- | ------------------- | ------------------------------- |
| `count` | [`int`](./index.md) | non-negative repetition count   |

**Returns:** [`str`](./str.md)

```
assert_eq ("ab".repeat 3) "ababab"
assert_eq ("ab".repeat 0) ""
```
