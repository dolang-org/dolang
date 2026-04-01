# proc.run

The `proc.run` module provides access to external programs.

## Accessing Programs

Use `proc.run` as a namespace to access programs by name:

```
run.ls -la
run.git status
run["/bin/echo"] hello world
```

Or import specific programs:

```
import proc.run:
  - ls
  - git
  - curl

ls -la
git status
```

Or call it with the program as a first argument:

```
run cat Cargo.toml
```

## Program Execution

When a program object is called, it spawns the external program with the given
arguments. Arguments are converted to strings using
[`std.arg`](../api/std/index.md#arg-value).

```
run.echo hello world
# Runs: /usr/bin/echo hello world
```

### I/O Redirection

Programs participate in Do's I/O system:

- Program **stdout** is connected to the current output
- Program **stdin** is connected to the current input

This means programs work naturally in pipelines:

```
import strand

let result = strand.pipeline
  do run.cat /etc/passwd
  do run.grep nologin
  do strand.each do |line| [...line.split ":"]
  do strand.collect()
```

### Capturing Output

Use [`sub`](proc/index.md#sub-func-trim) to capture a program's output as a
string:

```
let kernel = sub do run.uname -r
echo "Kernel: $kernel"
```

### Environment

Programs inherit the current environment from [`sys.env`](sys/index.md#env). Use
the [`env`](sys/index.md#env) function to set variables for a specific
invocation:

```
env LANG: C do
  run.sort input.txt
```

## Program Methods

### `which()`

Returns the resolved path to the program executable, if found.

```
echo $run.ls.which()
# Prints: /usr/bin/ls

echo $run.nonexistent.which()
# Prints: nil
```
