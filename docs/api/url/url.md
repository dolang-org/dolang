# Url

Structured URL value with parsed access to components, decoded path/query
iteration, and path-segment construction via `/`.

## Constructor

### `Url value`

Parses a URL string or copies an existing `Url`.

#### Parameters

| Name    | Type                     | Description          |
| ------- | ------------------------ | -------------------- |
| `value` | `str`\|[`Url`](./url.md) | URL to parse or copy |

#### Errors

| Exception    | Condition                     |
| ------------ | ----------------------------- |
| `ValueError` | The string is not a valid URL |

#### Example

```
let base = Url "https://example.com/api"
let copy = Url $base
```

## Fields

### `scheme`

The URL scheme, such as `"https"` or `"file"`.

### `username`

The decoded username, or `nil` if not present.

### `password`

The decoded password, or `nil` if not present.

### `host`

The host string, or `nil` if the URL has no host.

### `port`

The explicit port number, or `nil` if not present.

### `path`

The serialized path component.

This preserves percent-encoding. Use [`segments()`](#segments) for decoded
path segments.

### `name`

The decoded last path component, or `nil` if the URL has no identifiable
filename.

This returns `nil` for URLs whose path is empty or ends with `/`.

### `query_raw`

The raw query string without the leading `?`, or `nil` if absent.

### `fragment`

The decoded fragment string, or `nil` if absent.

## Methods

### `segments()`

Returns a fresh iterator of decoded path segments.

#### Returns

iterator of [`str`](../std/str.md)

#### Example

```
let url = Url "https://example.com/a%20b/c"
assert_eq [...url.segments()] ["a b", "c"]
```

### `query()`

Returns a fresh iterator of decoded query pairs.

Duplicate keys and original ordering are preserved.

#### Returns

iterator of `(str, str)` tuples

#### Example

```
let url = Url "https://example.com?q=a+b&q=c"
let pairs = [...url.query()]
assert_eq $pairs[0][0] "q"
assert_eq $pairs[0][1] "a b"
assert_eq $pairs[1][1] "c"
```

### `with_query_raw query`

Returns a new `Url` with its raw query replaced.

Pass `nil` to remove the query.

#### Parameters

| Name    | Type         | Description               |
| ------- | ------------ | ------------------------- |
| `query` | `str`\|`nil` | Raw query string or `nil` |

#### Returns

`Url`

### `with_fragment fragment`

Returns a new `Url` with its fragment replaced.

Pass `nil` to remove the fragment.

#### Parameters

| Name       | Type         | Description              |
| ---------- | ------------ | ------------------------ |
| `fragment` | `str`\|`nil` | Fragment string or `nil` |

#### Returns

`Url`

### `with_query pairs`

Returns a new `Url` with its query rebuilt from decoded key/value pairs.

The input may be any iterable of 2-element values.

#### Parameters

| Name    | Type  | Description                         |
| ------- | ----- | ----------------------------------- |
| `pairs` | input | Iterable of decoded key/value pairs |

#### Returns

`Url`

#### Example

```
let url = (Url "https://example.com").with_query_pairs [
  ["q", "a b"]
  ["tag", "x/y"]
]

assert_eq $url.query_raw "q=a+b&tag=x%2Fy"
```

## Operations

### String Conversion

Converting a `Url` to string yields the canonical serialized URL.

```
let url = Url "https://example.com/a%20b"
assert_eq (str url) "https://example.com/a%20b"
```

### Equality

Two `Url` values compare equal when their parsed underlying URLs are equal.

```
assert_eq (Url "https://example.com") (Url "https://example.com")
```

### Path Append: `/`

Appending with `/` usually adds one decoded path segment and returns a new
`Url`.

The appended segment is percent-encoded as needed.

```
let base = Url "https://example.com"
let child = (base / "a b" / "c/d")
assert_eq $child.path "/a%20b/c%2Fd"
```

If the right-hand side starts with `/`, it is treated as an absolute path (with
optional query/fragment) relative to the current scheme/authority, replacing
everything from the path onward.

```
let base = Url "https://example.com/old/path?old=1#old"
let href = (base / "/new/path?x=1#frag")
assert_eq (str href) "https://example.com/new/path?x=1#frag"
```
