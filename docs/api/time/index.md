# time

The `time` module provides UTC date/time instants, signed durations, and sleep.

## Functions

### `sleep duration`

Suspends the current strand.

**Parameters:**

| Name       | Type                                                                                | Description                                                |
| ---------- | ----------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `duration` | [`Duration`](./duration.md) \| [`int`](../std/int.md) \| [`float`](../std/float.md) | Sleep duration. Numeric values are interpreted as seconds. |

**Notes:**

- Duration must be non-negative.
- Floating-point values must be finite.

```
sleep 0.25
sleep (DateTime.from_unix(10) - DateTime.from_unix(10))
```

## Types

- [DateTime](./datetime.md)
- [Duration](./duration.md)
