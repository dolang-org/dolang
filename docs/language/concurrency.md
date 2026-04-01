# Concurrency

Do supports concurrent execution through **strands** - lightweight asynchronous
tasks. See the [`strand`](../api/strand/index.md) module for full API
details.

## Strand Concepts

Key features of strands:

- **Lightweight**: Strands are not full OS threads
- **Asynchronous**: Strands suspend when performing operations that would
  otherwise block, such as file I/O, allowing other strands to run. This is
  handled transparently, without explicit `async` or `await` syntax. Strands
  are not preemptible in general, so tight CPU-bound loops should be avoided.
- **Cancellable**: You can terminate a strand early, in which case it will
  raise a `CanceledError` when it next attempts to suspend.

### Scoped vs Background Strands

Do distinguishes between two kinds of strands:

**Scoped strands** (used by
[`strand.fork`](../api/strand/index.md#fork-blocks-limit) and
[`strand.pipeline`](../api/strand/index.md#pipeline-stage-stages-input-output))
are always joined before the function that creates them returns. You don't need
to manage them manually, and cancellation propagates automatically from parent
to child strands.

**Background strands** (created by
[`strand.spawn`](../api/strand/index.md#spawn-func) and
[`strand.stream`](../api/strand/index.md#stream-func)) are not tied to a scope.
The strand runs independently and may outlive the spawning context. You must
manage it manually through the returned [Strand](../api/strand/strand.md)
handle using [`join`](../api/strand/strand.md#join) to wait for completion
and possibly [`cancel`](../api/strand/strand.md#cancel) to terminate it.

## Spawning Strands

### `spawn`

The [`strand.spawn`](../api/strand/index.md#spawn-func) function creates a new
background strand:

```
import strand

let worker = strand.spawn do
  echo "Running in background"
  42

echo "Main thread continues"

let result = worker.join()
echo "Got result: $result"
```

The returned [Strand](../api/strand/strand.md) handle allows you to:

- Wait for completion with [`join`](../api/strand/strand.md#join)
- Check if done with the [`done`](../api/strand/strand.md#done) field
- Request cancellation with [`cancel`](../api/strand/strand.md#cancel)

## Concurrent Execution

### `fork`

The [`strand.fork`](../api/strand/index.md#fork-blocks-limit) function executes
multiple blocks concurrently and returns their results as an array:

```
import strand

let results = strand.fork
  - do 42
  - do "hello"
  - do (1 + 2)

assert_eq $results [42, "hello", 3]
```

All blocks become runnable simultaneously and the function waits for all to
complete. Results are returned in the same order as the input blocks.

## Pipelines

### `pipeline`

The
[`strand.pipeline`](../api/strand/index.md#pipeline-stage-stages-input-output)
function connects multiple stages into a data processing pipeline:

```
import strand

let result = strand.pipeline
  do strand.from [1, 2, 3, 4, 5]
  do strand.where do |x| (x > 2)
  do strand.each do |x| (x * 2)
  do strand.collect()

assert_eq $result [6, 8, 10]
```

Each stage runs in its own strand, with implicit channels connecting output to
input.

## Channel Communication

### `channel`

The [`strand.channel`](../api/strand/index.md#channel-buffer) function creates a
sender/receiver pair for communicating between strands:

```
import strand

let send recv = strand.channel()

let worker = strand.spawn do
  send.put 1
  send.put 2
  send.put 3
  send.close()

for value = recv
  echo $value

worker.join()
```

Channels have a fixed capacity (default 1).

## Streams

### `stream`

The [`strand.stream`](../api/strand/index.md#stream-func) function creates a
background strand with channels pre-wired to its input and output, returning a
[Stream](../api/strand/stream.md) handle. A Stream acts as both an input
iterator (you can read values it produces) and an output iterator (you can feed
values into it), making it easy to bridge background processing with the rest
of your program without manually creating and threading channels.

```
import strand

let s = strand.stream do strand.each do |x| (x * 2)

s.put 21
assert_eq (s.next()) 42
s.join()
```

Inside the callable, the strand reads from input with `strand.next()` and writes
to output with `strand.put` — the same as any pipeline stage. Pipeline stage
functions like `each` and `where` work unchanged.

## Built-in Pipeline Stages

Several functions are designed to work as pipeline stages:

- [`strand.from`](../api/strand/index.md#from-value) - emits values from an
  iterable
- [`strand.where`](../api/strand/index.md#where-predicate) - filters values by a
  predicate
- [`strand.each`](../api/strand/index.md#each-func) - transforms values
- [`strand.collect`](../api/strand/index.md#collect-target) - gathers values
  into a collection

## Error Handling

When a strand exits with an error:

- If you call [`join`](../api/strand/strand.md#join), the error is
  re-raised
- In `fork` and `pipeline` strands, all sibiling strands are canceled.
  After all strands complete, an arbitrary error among all failed strands is
  re-raised. Errors that were not caused by sibling cancellation (e.g.
  `CanceledError`, or `IterStop` and `SinkStop` errors in pipelines) are
  prioritized.

## Cancellation

When a strand is cancelled (either by propagation from a parent strand or
explicitly), current and subsequent suspending operations fail with a
`CanceledError`. This effect is masked during `finally` blocks in
[`try`/`catch`/`finally`](./error-handling.md) statements to permit
possibly-suspending calls (e.g. to clean up temporary files).
