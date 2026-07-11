# time

The `time` module provides UTC date/time instants, signed durations, sleeping,
and scoped timeouts.

## Functions

### `sleep duration`

Suspends the current strand.

#### Parameters

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

### `timeout duration block`

Runs `block` with a scoped timeout.

#### Parameters

| Name       | Type                                                                                | Description                                                  |
| ---------- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `duration` | [`Duration`](./duration.md) \| [`int`](../std/int.md) \| [`float`](../std/float.md) | Timeout duration. Numeric values are interpreted as seconds. |
| `block`    | [`func`](../std/func.md)                                                            | Block to run under the timeout scope.                        |

#### Returns

block result

#### Errors

| Exception                                    | Condition                                                                                |
| -------------------------------------------- | ---------------------------------------------------------------------------------------- |
| [`TimedOutError`](../std/timed-out-error.md) | The timeout is observed at a suspend or interrupt-check point before the block completes |

```
timeout 1 do
  sleep 10
```

## Types

- [DateTime](./datetime.md)
- [Duration](./duration.md)
