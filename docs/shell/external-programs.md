# External Programs

External programs are available through [`proc.run`](proc.run).
Programs inherit the current strand's shell context, including its
working directory, environment, and standard streams.

```
let :git :sort ... = run

git status --short
sort stdin: $["c", "a", "b"].crimp()
```

## Program Lookup

Call `run` directly, index it with a name or path, or destructure it to bind
identifier-safe program names. The resulting [`Program`](proc.Program)
proxy is a function:

```
run git status
run["/usr/local/bin/tool"] --version

let :make ... = run

make -j4
```

Alternatively, call `run` directly with a path or name:

```
run gcc -o main -O2 main.c
```

Calling a program resolves its executable using the current VFS and `PATH`. Use
[`.which()`](proc.Program.which) to resolve it without running it. It
returns the executable's path, or `nil` if the program is not found:

```
let git = run["git"].which()
if git
  echo "using $git"
else
  echo "git is not installed"
```

## Working Directory and Environment

Programs inherit the current strand's working directory and environment. Use
[`cd`](shell.cd) and
[`env`](shell.env) to override them for a scoped block:

```
cd build do
  env LANG: C do
    run make
```

## Redirecting I/O

Without explicit redirects, a program's stdin/stdout is connected to the
current strand's input `Iter` and output `Sink`, while stderr is connected to
the current [console](term.Console). Use `stdin:`, `stdout:`, and
`stderr:` to replace those connections for one invocation.

Each redirect accepts a path or an `Iter`/`Sink`.
[`std.null`](std.Null) provides empty input and discards output:

```
import std

# Read and write files.
run sort stdin: unsorted.txt stdout: sorted.txt

# Supply values and collect output values.
let lines = []
run sort stdin: ["c\n", "a\n", "b\n"] stdout: $lines

# Discard a stream
run tool stdout: $std.null
# Merge stderr into stdout.
run tool stdout: combined.log stderr: :STDOUT:
```

An [`fs.File`](fs.File) can be used directly as an input or output.

See [Terminal Output](./terminal-output.md) for the distinction between
the output stream and the console.

## Capturing Output

Use [`sub`](proc.sub) when the result should be one
string. By default, it removes one trailing line ending from the completed
capture:

```
let revision = sub do run git rev-parse HEAD
let exact = sub chomp: false do run tool
```

`sub` captures the strand output stream, not the console. A program's stderr
still goes to the console unless it is redirected. Merge stderr into stdout to
capture both:

```
let transcript = sub do run tool stderr: :STDOUT:
```

Redirect stdout or stderr to a sink when the caller needs individual values,
binary chunks, or a streaming consumer:

```
let lines = []
run git status --short stdout: $lines

let bytes = BinBuf()
run gzip -c data.txt stdout: $bytes mode: :CHUNK:
```

In the first example, `lines` receives one `Str` per line. In the second,
`bytes` receives `Bin` chunks and retains the exact compressed output. The
[`mode:`](#output-mode) argument chooses between these forms.

Use [`term.sub`](term.sub) when the
goal is to capture any console-bound output, including a program's unredirected
stderr:

```
let diagnostics = term.sub do run tool
```

## Pipelines

External programs integrate with
[`strand.pipeline`](strand.pipeline)

```
import 
  strand:
    - pipeline
let :cat :grep :sort ... = run

pipeline
  do cat messages.log
  do grep ERROR
  do sort
```

When two external program stages are adjacent, the pipeline connects their
stdio streams directly. Bytes pass from the first program's stdout to the
second program's stdin without becoming Do values.

Connections between stages use a
[`proc.PipeReceiver`](proc.PipeReceiver) and
[`proc.PipeSender`](proc.PipeSender). The same pipe endpoints are the
implicit input and output of a
[`strand.stream`](strand.stream) strand; its `Stream`
handle exposes wrappers around them through `iter()` and `sink()`. Use
`PipeReceiver.lines()` or `PipeReceiver.chunks()` to choose how a Do stage
receives bytes from an external program.

A Do stage adjacent to an external program receives discrete values. In this
example, `each` receives `Str` values for each line from `cat`, and `collect`
returns an array:

```
import
  strand:
    - pipeline
    - each
    - collect
let :cat ... = run

let lines = pipeline
  do cat messages.log
  do each do |line| line.upper()
  do collect()
```

The `input:` and `output:` arguments set the endpoints of the whole pipeline.
See [Pipelines](../language/concurrency.md#pipelines) for built-in Do stages,
custom stages, cancellation, and error handling.

## Termination

A nonzero exit status raises [`proc.Error`](proc.Error). Canceling a
strand running a program terminates that program. Background programs launched
under `strand.spawn` or `strand.stream` are terminated as a process group on
Unix.

Use
[`proc.with_policy`](proc.with_policy)
to change termination defaults for a block, or `policy:` for one invocation:

```
run worker policy: {signal: :INT:, grace: 10.0, force: true}
```

Windows uses `CTRL_BREAK_EVENT`; `signal:` overrides apply only to Unix
targets.

## External I/O and Do values

An external program reads and writes byte streams, while a Do pipeline stage
reads from an [`Iter`](std.Iter) and writes to a
[`Sink`](std.Sink). At a boundary between the two, `run` translates
in each direction:

- For stdin, `run` takes values from the input `Iter` and writes each value's
  bytes to the program. `Bin` values are written verbatim, while other types
  are coerced to `Str`. It adds no separator or line ending.
- For stdout and stderr, `run` reads bytes from the program, divides them into
  values, and puts those values into the output `Sink`.

This applies both to the implicit `Iter` and `Sink` in a pipeline and to values
given explicitly with `stdin:`, `stdout:`, or `stderr:`. Certain types pass the
underlying stream directly when possible to avoid the round-trip through a Do
value, such as `File` handles and the `shell.stdin` and `shell.stdout`
singletons.

### Output Mode

The `mode:` argument controls how stdout or stderr bytes are divided into
values:

| Mode      | Yields                                              |
| --------- | --------------------------------------------------- |
| `:LINE:`  | one [`Str`](std.Str) per line, line ending included |
| `:CHUNK:` | arbitrary-sized [`Bin`](std.Bin) values             |

`:LINE:` is the default. A stream whose last line has no terminator simply
yields a final value without one. Concatenating the values from either mode
reproduces the program's output byte for byte.

Adding or removing a line ending is a separate, explicit step. Use
[`chomp`](std.Iter.chomp) and
[`crimp`](std.Iter.crimp) on an iterator, or
[`prechomp`](std.Sink.prechomp) and
[`precrimp`](std.Sink.precrimp) on a sink:

```
let sorted = []
run sort stdin: $["c", "a", "b"].crimp() stdout: $sorted.prechomp()
# sorted == ["a", "b", "c"]
```
