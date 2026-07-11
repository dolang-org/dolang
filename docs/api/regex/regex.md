# Regex

A compiled regular expression.

## Constructor

### `Regex pattern`

Compiles a regular expression pattern.

#### Parameters

| Name      | Type                   | Description            |
| --------- | ---------------------- | ---------------------- |
| `pattern` | [`str`](../std/str.md) | RE2-compatible pattern |

#### Errors

| Exception    | Condition              |
| ------------ | ---------------------- |
| `ValueError` | The pattern is invalid |

#### Example

```
let digits = Regex r"\d+"
let date = Regex r"(?<year>\d{4})-(?<month>\d{2})-(?<day>\d{2})"
```

## Methods

### `match haystack`

Searches for the first match of this pattern anywhere in `haystack`.

#### Parameters

| Name       | Type                   | Description             |
| ---------- | ---------------------- | ----------------------- |
| `haystack` | [`str`](../std/str.md) | The string to search in |

#### Returns

A [`Captures`](./captures.md) object if the pattern matches,
or `nil` if there is no match.

#### Example

```
let pattern = Regex r"\d+"
let caps = pattern.match "abc 42 def"
echo $caps  # => 42

let no_match = pattern.match "no digits"
echo $no_match  # => nil
```

### `find haystack`

Returns an iterator over all non-overlapping matches of this pattern in
`haystack`.

#### Parameters

| Name       | Type                   | Description             |
| ---------- | ---------------------- | ----------------------- |
| `haystack` | [`str`](../std/str.md) | The string to search in |

#### Returns

A [`Find`](./find.md) that yields a
[`Captures`](./captures.md) for each match.

#### Example

```
let pattern = Regex r"\d+"
for caps = pattern.find "one 1 two 2 three 3"
  echo $caps  # => 1, then 2, then 3

# Collect all matches into an array
let matches = [...pattern.find "1 22 333"]
echo $matches.len  # => 3
```

### `replace haystack replacement [limit: int]`

Replaces matches of this pattern in `haystack`. The `replacement` is either a
string template or a callback function.

**String replacement** supports backreferences:

- `$1`, `$2`, … — numbered capture groups
- `${name}` — named capture groups
- `$$` — literal `$`

**Callback replacement** receives a [`Captures`](./captures.md) object for each
match and must return a `str`.

The optional `limit` controls how many replacements are performed:

- Omitted — replace all matches.
- `limit: N` (positive) — replace at most N matches.
- `limit: 0` — replace nothing (returns `haystack` unchanged).

Negative limits are not supported.

#### Parameters

| Name          | Type                                     | Description                           |
| ------------- | ---------------------------------------- | ------------------------------------- |
| `haystack`    | [`str`](../std/str.md)                   | the string to search in               |
| `replacement` | [`str`](../std/str.md) or function       | template string or callback           |
| `limit`       | [`int`](../std/index.md)                 | max replacements (default: unlimited) |

#### Returns

A new `str` with replacements applied.

#### Example

```
let re = Regex r"(\w+)@(\w+)"
echo $ re.replace "a@b c@d" r"$2=$1"  # => b=a d=c

# Named groups
let date_re = Regex r"(?<m>\d{2})/(?<d>\d{2})/(?<y>\d{4})"
echo $ date_re.replace "03/22/2026" r"${y}-${m}-${d}"  # => 2026-03-22

# With limit
let comma = Regex r","
echo $ comma.replace "a,b,c,d" ";" limit: 2  # => a;b;c,d

# Callback replacement
let upper = Regex r"[a-z]+"
echo $ upper.replace "hello world" do |caps|
  caps.0.upper
# => HELLO WORLD
```

### `split haystack [limit: int]`

Splits `haystack` around matches of this pattern. Returns an iterator that
yields the `str` substrings between matches.

The optional `limit` controls how many splits are performed and from which end:

- `limit: N` (positive) — split at most N times from the **left**; the last
  element is the unsplit remainder.
- `limit: -N` (negative) — split at most N times from the **right**, but still
  yield segments left-to-right. Useful for splitting off a known-length suffix.
- Omitted — split fully with no limit.

When possible, iteration is lazy (segments are computed on demand). Negative
limits require buffering all matches up front.

#### Parameters

| Name       | Type                     | Description                                 |
| ---------- | ------------------------ | ------------------------------------------- |
| `haystack` | [`str`](../std/str.md)   | the string to split                         |
| `limit`    | [`int`](../std/index.md) | max splits; negative means split from right |

#### Returns

An iterator over the segments.

#### Example

```
let ws = Regex r"\s+"
assert_eq [...ws.split "hello  world  foo"] ["hello", "world", "foo"]

# Positive limit: 1 split from the left
assert_eq [...ws.split "a b c" limit: 1] ["a", "b c"]

# Negative limit: 1 split from the right
let head tail = ws.split "a b c" limit: -1
assert_eq $head "a b"
assert_eq $tail "c"

# Destructuring
let first ...rest = ws.split "a b c d"
assert_eq $first "a"
assert_eq [...rest] ["b", "c", "d"]
```

### `rsplit haystack [limit: int]`

Like `split`, but yields segments in **right-to-left** order (rightmost segment
first).

The optional `limit` controls how many splits are performed and from which end:

- `limit: N` (positive) — split at most N times from the **right**; the last
  element yielded is the unsplit left remainder.
- `limit: -N` (negative) — split at most N times from the **left**, but still
  yield segments right-to-left.
- Omitted — split fully with no limit.

`rsplit` always buffers all matches internally (the regex engine only scans
forward).

#### Parameters

| Name       | Type                     | Description                                |
| ---------- | ------------------------ | ------------------------------------------ |
| `haystack` | [`str`](../std/str.md)   | the string to split                        |
| `limit`    | [`int`](../std/index.md) | max splits; negative means split from left |

#### Returns

An iterator over the segments in reverse order.

#### Example

```
let comma = Regex r","
assert_eq [...comma.rsplit "a,b,c"] ["c", "b", "a"]

# Positive limit: 1 split from the right
assert_eq [...comma.rsplit "a,b,c" limit: 1] ["c", "a,b"]

# Negative limit: 1 split from the left
assert_eq [...comma.rsplit "a,b,c" limit: -1] ["b,c", "a"]
```
