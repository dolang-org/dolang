# Captures

Represents a successful regex match along with any capture groups. `Captures`
objects are returned by [`Regex.match()`](./regex.md#match-haystack) and
yielded by [`Find`](./find.md).

Coercing `Captures` value to `str` produces the text of the overall match.

## Fields

### `start`

The byte offset of the start of the overall match within the haystack.

#### Type

[`int`](../std/int.md)

### `end`

The byte offset of the end of the overall match within the haystack.

#### Type

[`int`](../std/int.md)

## Index Access

Capture groups can be retrieved by integer index or by name.

### By integer

| Index | Description               |
| ----- | ------------------------- |
| `0`   | The overall match         |
| `1`   | First capture group       |
| `2`   | Second capture group      |
| `n`   | nth capture group         |

#### Returns

[`Match`](./match.md)

#### Errors

| Exception                             | Condition                                                                     |
| ------------------------------------- | ----------------------------------------------------------------------------- |
| [`IndexError`](../std/index-error.md) | The group index is out of range or the group did not participate in the match |

```
let date = Regex r"(\d{4})-(\d{2})-(\d{2})"
let caps = date.match "2024-03-15"

echo $caps       # => 2024-03-15  (overall match)
echo $caps[0]    # => 2024-03-15  (overall match)
echo $caps[1]    # => 2024
echo $caps[2]    # => 03
echo $caps[3]    # => 15
```

### By name

Named groups (defined with `(?<name>...)`) can be accessed by their name.

#### Returns

[`Match`](./match.md)

#### Errors

| Exception                             | Condition                                                                    |
| ------------------------------------- | ---------------------------------------------------------------------------- |
| [`IndexError`](../std/index-error.md) | No group with that name exists or the group did not participate in the match |

```
let date = Regex r"(?<year>\d{4})-(?<month>\d{2})-(?<day>\d{2})"
let caps = date.match "2024-03-15"

echo $caps["year"]   # => 2024
echo $caps["month"]  # => 03
echo $caps["day"]    # => 15
```
