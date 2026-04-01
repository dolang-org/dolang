# Match

Represents a single matched span within a string. `Match` objects are returned
by indexing a [`Captures`](./captures.md) value.

Coercing a `Match` value to `str` produces the matched text.

## Fields

### `start`

The byte offset of the start of the match within the haystack.

**Type:** [`int`](../std/int.md)

### `end`

The byte offset of the end of the match within the haystack.

**Type:** [`int`](../std/int.md)

```
let date = Regex r"(?<year>\d{4})-(?<month>\d{2})-(?<day>\d{2})"
let caps = date.match "prefix 2024-03-15 suffix"

let year = caps["year"]
echo year          # => 2024
echo $year.start   # => 7
echo $year.end     # => 11
```
