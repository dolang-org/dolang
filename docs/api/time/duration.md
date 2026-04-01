# Duration

A signed time span with nanosecond precision.

## Fields

| Field         | Type                   | Description                                   |
| ------------- | ---------------------- | --------------------------------------------- |
| `seconds`     | [`int`](../std/int.md) | Floor-divided whole seconds component         |
| `nanoseconds` | [`int`](../std/int.md) | Non-negative fractional nanoseconds component |

## String Form

`str(duration)` renders a compact seconds form (for example, `0s`, `2.5s`,
`-2.5s`).
