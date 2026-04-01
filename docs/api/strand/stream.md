# Stream

A `Stream` is a handle to a background strand created by
[`stream`](./index.md#stream-func). Unlike a plain
[`Strand`](./strand.md), a `Stream` has input and output channels wired up
automatically, so it acts as both an **iterator** (it produces values)
and a **sink** (you can feed values into it).

## Fields

### `done`

A read-only boolean indicating whether the strand has completed execution.

## Methods

### `close()`

Closes both the input and output channels. This signals to the strand that no
more values will be sent (its `next` will see end-of-input) and that no more
values will be read (its `put` will get an error). The strand is not waited
for; use [`wait()`](#wait) or [`join()`](#join) afterwards if needed.

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

- **`s.next()`** re-raises the error once the output channel is exhausted
  (instead of raising a generic `IterStop` error).
- **`s.put <item>`** re-raises the error when the input channel is closed
  (instead of raising a generic `SinkStop` error).

This means you can use a stream like any iterator and have errors surface
naturally at the call site.
