# range

Ranges represent a sequence of numbers from start to end (exclusive) with a
step. They are primarily used for iteration.

## Creating Ranges

Call the `range` type object with positional or keyword arguments:

### Positional Arguments

```
# Just a end value (start defaults to 0, step to 1)
let r = range 5

# Start and end
let r = range 0 10

# With step
let r = range 0 20 2
```

### Keyword Arguments

```
# Just a end value (start defaults to 0, step to 1)
let r = range end: 5

# Start and end
let r = range start: 0 end: 10

# With step
let r = range start: 0 end: 20 step: 2
```

### Mixed Arguments

Positional and keyword arguments can be mixed:

```
# Positional end with keyword step
let r = range 10 step: 2

# Positional start with keyword end and step
let r = range 0 end: 20 step: 5
```

**Parameters:**

| Name    | Type  | Description                 | Position |
| ------- | ----- | --------------------------- | -------- |
| `start` | `int` | starting value (default: 0) | 0        |
| `end`   | `int` | ending value (exclusive)    | 1        |
| `step`  | `int` | step size (default: 1)      | 2        |

## Fields

### `start`

The starting value.

### `end`

The endping value (exclusive).

### `step`

The step size.

## Iteration

Ranges are iterable:

```
for i = range 5
  echo $i
# 0, 1, 2, 3, 4

for i = range 0 10 3
  echo $i
# 0, 3, 6, 9

for i = range start: 1 end: 10 step: 3
  echo $i
# 1, 4, 7
```

## Methods

### `contains value`

Tests whether the given value is within the range interval. For increasing
ranges (start < end), checks if value is in \[start, end). For decreasing
ranges (start > end), checks if value is in (end, start].

**Parameters:**

| Name    | Type | Description          |
| ------- | ---- | -------------------- |
| `value` |      | the value to check   |

**Returns:** `bool`

```
# Increasing range [0, 5)
let r1 = range 0 5
assert (r1.contains 0)
assert (r1.contains 4)
assert (!r1.contains 5)

# Decreasing range (0, 5]
let r2 = range 5 0 -1
assert (r2.contains 5)
assert (r2.contains 1)
assert (!r1.contains 0)
```
