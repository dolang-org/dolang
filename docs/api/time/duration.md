# Duration

A signed time span with nanosecond precision.

## Fields

| Field   | Type                       | Description                  |
| ------- | -------------------------- | ---------------------------- |
| `secs`  | [`float`](../std/float.md) | Approximate seconds view     |
| `nanos` | [`int`](../std/int.md)     | Exact total nanoseconds view |

## String Form

`str(duration)` renders a compact seconds form (for example, `0s`, `2.5s`,
`-2.5s`).
