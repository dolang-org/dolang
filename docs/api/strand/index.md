# strand

Concurrency primitives.

## Types

| Type               | Description                        |
| ------------------ | ---------------------------------- |
| [`Key`](./key.md)  | Strand-local storage key           |

## Functions

### `limit count block`

Runs `block` with a transitive fork budget.

`strand.limit` caps the total number of active `fork` workers anywhere under
the scoped block. Nested `strand.limit` scopes compose by adding another
constraint.

#### Parameters

| Name    | Type                   | Description                                 |
| ------- | ---------------------- | ------------------------------------------- |
| `count` | [`int`](../std/int.md) | maximum active descendant fork workers      |
| `block` | func                   | code to run under the transitive limit      |

#### Returns

The block result.

```
let results = strand.limit 256 do
  fork limit: 16
    - do fetch_user 1
    - do fetch_user 2
    - do fetch_user 3
```

Here `fork limit:` controls immediate fan-out at that call site, while
`strand.limit` controls the total active `fork` workers across the nested
subtree.

### `fork ...blocks :limit?`

Executes multiple blocks concurrently and returns their results as an array.

#### Parameters

| Name        | Type                   | Description                                                      |
| ----------- | ---------------------- | ---------------------------------------------------------------- |
| `...blocks` | func                   | callables to execute concurrently                                |
| `limit`     | [`int`](../std/int.md) | maximum number of worker strands to run at once (default: all)   |

#### Returns

`array` -- results in the same order as the blocks

```
let results = fork
  - do 42
  - do "hello"
  - do (1 + 2)

assert_eq $results [42, "hello", 3]
```

With `limit`, work is still returned in input order, but only up to that many
worker strands run concurrently:

```
let results = fork limit: 2
  - do fetch_user 1
  - do fetch_user 2
  - do fetch_user 3
```

This `limit` only applies to that fork site. Use
[`strand.limit`](#limit-count-block) to cap total nested fork work across a
larger scope.

### `pipeline stage ...stages :input? :output?`

Creates a data processing pipeline by connecting multiple stages together. Each
stage runs concurrently in its own strand, with channels connecting the output
of one stage to the input of the next.

#### Parameters

| Name              | Type   | Description                                    |
| ----------------- | ------ | ---------------------------------------------- |
| `stage`, `stages` | func   | Pipeline stages to execute                     |
| `input`           | input  | Optional input source for the first stage      |
| `output`          | output | Optional output destination for the last stage |

Pipeline stages are callables that read from their implicit input and write to
their implicit output. The `from`, `where`, `each`, and `collect` functions are
designed to work as pipeline stages.

**Examples:**

```
let result = pipeline
  do from [1, 2, 3, 4, 5]
  do where do |x| (x > 2)
  do each do |x| (x * 2)
  do collect()

assert_eq $result [6, 8, 10]
```

With explicit input and output:

```
import fs:
  - open

# Process lines from a file, writing results to another file
open input.txt r do |in| open output.txt w do |out|
  pipeline input: $in output: $out
    do where do |line| line.contains "ERROR"
    do each do |line| line.trim()
```

### `spawn func`

Spawns `func` to run concurrently in a background strand, returning a
[Strand](strand.md) handle for managing it.

#### Parameters

| Name   | Type   | Description             |
| ------ | ------ | ----------------------- |
| `func` | `func` | the callable to execute |

#### Returns

[Strand](strand.md) -- a handle to the background strand

```
let worker = spawn do
  echo "Background task running"
  42

echo "Main task continuing"
let result = worker.join()
echo "Result: $result"
```

The strand runs independently in the background. Use the returned Strand's
[`join`](strand.md#join) method to wait for completion and get the result,
or [`cancel`](strand.md#cancel) to request early termination.

See [Strand](strand.md) for the handle's fields and methods.

### `stream func`

Spawns `func` as a background strand with input and output channels
pre-wired, returning a [Stream](./stream.md) handle. The callable runs with its
ambient input and output connected to the stream's channels, so it can use
`next` to read values fed in from outside and `put` to send values out.

#### Parameters

| Name   | Type   | Description             |
| ------ | ------ | ----------------------- |
| `func` | `func` | the callable to execute |

#### Returns

[Stream](./stream.md)

```
let s = stream do each do |x| (x * 2)
let input = s.sink()
let output = s.iter()

let results = fork
  do
    input.put 1
    input.put 2
    input.put 3
  do
    let r1 = output.next()
    let r2 = output.next()
    let r3 = output.next()
    [r1, r2, r3]

s.join()
assert_eq $results[1] [2, 4, 6]
```

See [Stream](./stream.md) for the handle's fields and methods.

### `channel buffer?`

Creates a new channel for communication between strands. Returns a
`[sender, receiver]` pair.

#### Parameters

| Name     | Type  | Description                                |
| -------- | ----- | ------------------------------------------ |
| `buffer` | `int` | Buffer capacity (default: 1, unbuffered)   |

#### Returns

`[sender, receiver]`

```
let send recv = channel()  # unbuffered channel
let send recv = channel 10 # buffered with capacity 10
```

See [Sender](sender.md) and [Receiver](receiver.md) for the types returned.

### `from value`

A pipeline stage that emits all values from an iterable to its output.

#### Parameters

| Name    | Type  | Description                     |
| ------- | ----- | ------------------------------- |
| `value` | input | an iterable to emit values from |

### `where predicate`

A pipeline stage that filters values. Reads from input, tests each value with
the predicate, and writes passing values to output.

#### Parameters

| Name        | Type | Description                               |
| ----------- | ---- | ----------------------------------------- |
| `predicate` | func | a callable returning a truthy/falsy value |

### `each func`

A pipeline stage that transforms values. Reads from input, calls `func` on
each value, and writes the result to output.

#### Parameters

| Name   | Type | Description                               |
| ------ | ---- | ----------------------------------------- |
| `func` | func | a callable that transforms a single value |

### `collect target?`

A pipeline stage that collects all input values into an array (or another
target).

#### Parameters

| Name     | Type   | Description                                    |
| -------- | ------ | ---------------------------------------------- |
| `target` | output | collection to add to (defaults to a new array) |

#### Returns

The collection.
