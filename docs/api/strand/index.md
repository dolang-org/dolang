# strand

Concurrency primitives.

## Types

| Type                           | Description                         |
| ------------------------------ | ----------------------------------- |
| [`Key`](./key.md)              | Strand-local storage key            |
| [`Resource`](./resource.md)    | Scoped concurrency admission limit  |

## Functions

### `fork ...blocks`

Executes multiple blocks concurrently and returns their results as an array.

#### Parameters

| Name        | Type | Description                       |
| ----------- | ---- | --------------------------------- |
| `...blocks` | func | callables to execute concurrently |

#### Returns

`array` -- results in the same order as the blocks

```
let results = fork
  - do 42
  - do "hello"
  - do (1 + 2)

assert_eq $results [42, "hello", 3]
```

Use [`map`](#map-count-func-input-output) for bounded concurrent work over an
iterator and [`Resource`](./resource.md) for application-defined admission
limits.

### `map count func :input? :output?`

Applies `func` concurrently to values pulled lazily from an iterator.

#### Parameters

| Name     | Type                   | Description                                      |
| -------- | ---------------------- | ------------------------------------------------ |
| `count`  | [`int`](../std/int.md) | number of worker strands                         |
| `func`   | func                   | callable applied to each input value             |
| `input`  | input?                 | source; defaults to the strand-local iterator    |
| `output` | output?                | destination; defaults to the strand-local sink   |

Results are sent as workers complete. The function returns `nil` after the
input is exhausted and every worker has finished.

```
let results = []
map 4 input: (range 20) output: $results do |value|
  fetch $value
```

As a pipeline stage:

```
let results = pipeline
  do from (range 20)
  do map 4 do |value| fetch $value
  do collect()
```

### `pool count input func`

Executes `func` over an iterator with a fixed number of scoped worker strands.

#### Parameters

| Name    | Type                   | Description                           |
| ------- | ---------------------- | ------------------------------------- |
| `count` | [`int`](../std/int.md) | number of worker strands              |
| `input` | input                  | source consumed lazily by the workers |
| `func`  | func                   | callable applied to each input value  |

Block results are discarded. The function returns `nil` after the input is
exhausted and every worker has finished.

```
pool 4 $urls do |url|
  download $url
```

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

Runs `func` concurrently in a background strand, returning a
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

Use the returned `Strand`'s [`join`](strand.md#join) method to wait for
completion and get the result, or [`cancel`](strand.md#cancel) to request early
termination.

Background strands do not inherit active [`Resource`](./resource.md)
reservations or the strand-local values of `Key`s.

### `stream func`

Runs `func` in a background strand with its strand-local input `Iter` and
output `Sink` connected to channels. The returned [Stream](./stream.md) handle
can be used to communicate with it.

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

### `channel buffer?`

Creates a new channel for communication between strands.

#### Parameters

| Name     | Type  | Description                                |
| -------- | ----- | ------------------------------------------ |
| `buffer` | `int` | Buffer capacity (default: 1, unbuffered)   |

#### Returns

`(`[`Sender`](sender.md)`, `[`Receiver`](receiver.md)`)`

#### Example

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

A pipeline stage that filters values. Reads from its input, tests each value
with the predicate, and writes passing values to ts output.

#### Parameters

| Name        | Type | Description                               |
| ----------- | ---- | ----------------------------------------- |
| `predicate` | func | a callable returning a truthy/falsy value |

### `each func`

A pipeline stage that transforms values. Reads from its input, calls `func` on
each value, and writes the result to its output.

#### Parameters

| Name   | Type | Description                               |
| ------ | ---- | ----------------------------------------- |
| `func` | func | a callable that transforms a single value |

### `collect target?`

A pipeline stage that collects all input values into an array (or another
target).

#### Parameters

| Name     | Type   | Description                                      |
| -------- | ------ | ------------------------------------------------ |
| `target` | output | collection to add to (defaults to a new `array`) |

#### Returns

The collection.
