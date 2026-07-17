# Stream

A `Stream` is a handle to a background strand created by
[`stream`](./index.md#stream-func). Unlike a plain
[`Strand`](./strand.md), a `Stream` has input and output channels wired up
automatically. It implements [`Iterable`](../std/iterable.md) for its output
side and [`Sinkable`](../std/sinkable.md) for its input side.

The background strand does not inherit active
[`Resource`](./resource.md) reservations from its creator.

## Fields

### `done`

A read-only boolean indicating whether the strand has completed execution.

## Methods

### `join()`

Closes both channels, then waits for the strand to complete. If the strand
exited normally, returns its result. If the strand exited with an error,
re-raises it.

Closing the channels before waiting prevents deadlock when the strand is
blocked waiting for input or waiting for a consumer to read its output.

### `wait()`

Waits for the strand to complete. Unlike [`join()`](#join), this method does
not close the channels, does not return the strand's result, and does not
re-raise any error.

### `cancel()`

Requests cancellation of the strand, causing a `Canceled` error to be raised
at the strand's next suspension point.

## Error Propagation

When the stream strand exits with an error:

- **`s.iter().next()`** re-raises the error once the output channel is exhausted
  (instead of raising a generic `IterStop` error).
- **`s.sink().put <item>`** still raises `SinkStop` when the input channel is
  closed.
- **`s.join()`** re-raises the strand error directly.

This means consumer-side errors still surface naturally from the output
receiver, while producer-side callers continue to see ordinary sink shutdown.
