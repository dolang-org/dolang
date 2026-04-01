# Strand

A Strand is a handle to a background strand spawned by
[`spawn`](./index.md#spawn-func).

## Fields

### `done`

A read-only boolean indicating whether the strand has completed execution.

## Methods

### `join()`

Waits until the strand completes and returns its result. If the strand
terminates normally, returns the value returned by the callable. If the strand
exited with an error, this method re-raises it.

### `cancel()`

Requests cancellation of the strand, causing a `Canceled` error to be raised
if the strand is suspended and on all subsequent suspension attempts.

### `wait()`

Waits until the strand completes. Unlike [`join()`](#join), this method does
not return the strand's return value or re-raise any error. Use this when you
only care that the strand finished, not what it returned.
