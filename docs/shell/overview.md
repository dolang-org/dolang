# Overview

The `dolang-shell` crate provides the `dolang` script executor and REPL for
the Do language. It extends the core language with shell-oriented features like
process spawning, environment variable access, and file I/O.

## Running Scripts

Run a Do script from the command line:

```bash
dolang script.dol
```

With the `--strict` flag, compiler warnings are treated as errors (runtime
errors always propagate if uncaught, regardless of this flag):

```bash
dolang --strict script.dol
```

Other flags:

- `--check` -- check syntax without executing
- `--compile <OUTPUT>` -- compile to bytecode file

Scripts can use a shebang for direct execution:

```
#!/usr/bin/env -S dolang --strict
echo Hello from Do!
```

Arguments after the script path are available as `shell.args`.
The executed script path is available as `shell.program`; when using `-m`, it is
the module name instead. In the REPL, `shell.program` is `nil`.

## REPL

Launch an interactive REPL with no arguments:

```bash
dolang
```

The REPL provides an interactive environment where you can evaluate Do
expressions and statements. Variables and definitions persist across lines
within a session.

## Shell Prelude

The shell automatically imports a set of functions and objects into scope.

### `shell`

| Name                                       | Description                                             |
| ------------------------------------------ | ------------------------------------------------------- |
| [`echo`](../api/shell/index.md#echo-args)  | Print arguments to terminal, separated by spaces        |
| [`exit`](../api/shell/index.md#exit-code)  | Exit with a status code (default: 0)                    |
| [`cd`](../api/shell/index.md#cd-path-func) | Change directory; optionally run func in new dir        |
| [`env`](../api/shell/index.md#env)         | Access environment variables                            |
| [`args`](../api/shell/index.md#args)       | Command-line arguments ([`array`](../api/std/array.md)) |
| [`program`](../api/shell/index.md#program) | Script [`Path`](../api/fs/path.md) or `-m` module name  |

### `proc`

| Name                                        | Description                       |
| ------------------------------------------- | --------------------------------- |
| [`sub`](../api/proc/index.md#sub-func-trim) | Capture func's output as a string |

### `proc.run`

| Name                                  | Description              |
| ------------------------------------- | ------------------------ |
| Module as [`run`](../api/proc-run.md) | Access external programs |
