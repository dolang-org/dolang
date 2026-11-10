# Concurrency

Do supports concurrent execution through **strands** - lightweight asynchronous
tasks. See the [`strand`](strand) module for full API
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
[`strand.fork`](strand.fork) and
[`strand.pipeline`](strand.pipeline))
are always joined before the function that creates them returns. You don't need
to manage them manually, and cancellation propagates automatically from parent
to child strands.

**Background strands** (created by
[`strand.spawn`](strand.spawn) and
[`strand.stream`](strand.stream)) are not tied to a scope.
The strand runs independently and may outlive the spawning context. You must
manage it manually through the returned [Task](strand.Task)
handle using [`join`](strand.Task.join) to wait for completion
and possibly [`cancel`](strand.Strand.cancel) to terminate it.

Held [`Resource`](strand.Resource)s are inherited by
scoped strand but *not* by background strand. Later scoped state changes in one
strand do not impact other strands.

## Spawning Strands

### `spawn`

The [`strand.spawn`](strand.spawn) function creates a new
background strand:

```playground
let worker = spawn do
  echo "Running in background"
  42

echo "Main strand continues"

let result = worker.join()
echo "Got result: $result"
```

The returned [Task](strand.Task) handle allows you to:

- Wait for completion with [`join`](strand.Task.join)
- Check if done with the [`done`](strand.Task.done) field
- Request cancellation with [`cancel`](strand.Strand.cancel)

## Concurrent Execution

### `fork`

The [`strand.fork`](strand.fork) function executes
multiple blocks concurrently and returns their results as a tuple:

```playground
#> import test:
#>   - assert_eq
let results = fork
  - do 42
  - do "hello"
  - do (1 + 2)

assert_eq $results (Tuple([42, "hello", 3]))
```

All blocks become runnable simultaneously and the function waits for all to
complete. Results are returned in the same order as the input blocks.

For small homogeneous fan-out, a `for` comprehension can construct the blocks:

```
let results = fork
  for server = build_servers
    do build_on $server
```

### `map`

[`strand.map`](strand.map) applies one
function concurrently to values pulled lazily from an iterator:

```
let results = []
strand.map 8 input: $jobs output: $results do |job|
  run_job $job
```

Results are emitted in completion order. To restore input order, enumerate the
input, preserve each index in the result, and sort after collecting.

[`strand.pool`](strand.pool) is the scoped
worker form for a known input when block results are not needed:

```
strand.pool 8 $jobs do |job|
  run_job $job
```

### Resources

[`strand.Resource`](strand.Resource) provides explicit admission
limits independent of strand structure:

```
let network = strand.Resource 16

network.with do
  fetch_data()
```

Resource scopes are reentrant and scoped child strands inherit their parent's
reservations. Background strands created with `spawn` or `stream` do not
inherit them.
When acquiring multiple resources, use a consistent order to avoid deadlock.
Resources do not guard program state and must not be used as mutexes.

## Pipelines

### `pipeline`

The
[`strand.pipeline`](strand.pipeline)
function connects multiple stages into a data processing pipeline:

```playground
#> import test:
#>   - assert_eq
let result = pipeline
  do strand.from [1, 2, 3, 4, 5]
  do strand.where do |x| (x > 2)
  do strand.each do |x| (x * 2)
  do strand.collect()

assert_eq $result [6, 8, 10]
```

Each stage runs in its own strand, with implicit channels connecting output to
input.

## Built-in Pipeline Stages

Several functions are designed to work as pipeline stages:

- [`strand.from`](strand.from) - emits values from an
  iterable
- [`strand.where`](strand.where) - filters values by a
  predicate
- [`strand.each`](strand.each) - transforms values
- [`strand.map`](strand.map) -
  transforms values concurrently
- [`strand.pool`](strand.pool) - runs scoped
  workers and discards their results
- [`strand.collect`](strand.collect) - gathers values
  into a collection

## Channel Communication

### `channel`

The [`strand.channel`](strand.channel) function creates a
sender/receiver pair for communicating between strands:

```playground
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

The [`strand.stream`](strand.stream) function creates a
background strand with channels pre-wired to its input and output, returning a
[Stream](strand.Stream) handle. A Stream implements `Iterable` for
its output side and `Sinkable` for its input side, making it easy to bridge
background processing with the rest of your program without manually creating
and threading channels.

```playground
#> import test:
#>   - assert_eq
let s = stream do strand.each do |x| (x * 2)
let input = s.sink()
let output = s.iter()

input.put 21
assert_eq (output.next()) 42
s.join()
```

Inside the function, the strand reads from input with `strand.next()` and writes
to output with `put` — the same as any pipeline stage. Pipeline stage
functions like `each` and `where` work unchanged.

## Error Handling

When a strand exits with an error:

- If you call [`join`](strand.Task.join), the error is
  re-raised
- In `fork` and `pipeline` strands, all sibling strands are canceled.
  After all strands complete, one of their errors is re-raised, in preference
  to the errors the cancellation itself causes: `CanceledError`, and in
  pipelines `IterStop` and `SinkStop` from closed stages.

## Cancellation

When a strand is cancelled (either by propagation from a parent strand or
explicitly), current and subsequent suspending operations fail with a
`CanceledError`. This effect is masked during `finally` blocks in
[`try`/`catch`/`finally`](./error-handling.md) statements to permit
possibly-suspending calls (e.g. to clean up temporary files).

## Inspecting Strands

[`strand.snapshot`](strand.snapshot) captures every running strand at once, as
[`StrandInfo`](strand.StrandInfo) values. Each one records the strand, its
scoped parent (`nil` for a top-level strand), and its backtrace at the time of
capture. Following the parents rebuilds the tree, which helps when debugging a
stalled program:

```
for info = strand.snapshot()
  let parent = (info.parent && info.parent.id || "-")
  echo "strand $(info.strand.id) (parent $parent)"
  for frame = info.backtrace
    echo "  $(frame.module).$(frame.receiver)"
```

A [`Strand`](strand.Strand) handle observes a strand without keeping it alive
or joining it. [`strand.current`](strand.current) returns one for the current
strand, and a [`Task`](strand.Task) is also one. Handles compare equal when they
observe the same strand, and [`id`](strand.Strand.id) is never reused, so
strands can be matched across snapshots. A handle can list the strand's
[`children`](strand.Strand.children), capture its
[`backtrace`](strand.Strand.backtrace), or
[`cancel`](strand.Strand.cancel) it. A strand that has exited raises
`std.StateError` for operations that need it running.
