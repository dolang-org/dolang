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

- `-m`, `--main` -- run a bundled entrypoint by module name
- `--check` -- check syntax without executing
- `--compile <OUTPUT>` -- compile to bytecode file
- `--module-path <PATH>` -- add a module search path
- `--import <MODULE[=NAME]>` -- add a module to the prelude
- `--import-item <MODULE.ITEM[=NAME]>` -- add a module item to the prelude
- `--no-cache` -- disable reading and writing the bytecode cache
- `-h`, `--help` -- show command-line help

`--module-path` is repeatable. Explicit paths are searched in command-line
order before the `site/` directory and bundled modules. See
[Modules and Caching](./modules.md).

Prelude options are repeatable. An alias after `=` changes the name bound in
the script:

```
dolang --import fs \
  --import-item fs.open \
  --import-item fs.append=append_file \
  script.dol
```

Scripts can use a shebang for direct execution:

```
#!/usr/bin/env -S dolang --strict
echo Hello from Do!
```

Arguments after the script path are available as `shell.args`.
The executed script path is available as `shell.program`; when using `-m`, it is
the module name instead. In the REPL, `shell.program` is `nil`.

### Bundled Entrypoints

`-m` runs an entrypoint bundled with `dolang`:

```
dolang -m dodo --list
dolang -m test -- test
```

Installed aliases such as `dodo` and `dolang-test` select the corresponding
entrypoint implicitly when available.

### Companion Programs

A complete source installation builds three executables:

- `dolang` -- script executor, bundled entrypoints, and REPL
- `dolang-lsp` -- language server
- `dolang-vfs` -- target-side helper for VFS contexts

Keep `dolang-vfs` from the same build or release as `dolang`. Container helpers
locate it beside the interpreter, while SSH and WSL transitions require a
matching helper on the destination command path.

## REPL

Launch an interactive REPL with no arguments:

```bash
dolang
```

The REPL provides an interactive environment where you can evaluate Do
expressions and statements. Variables and definitions persist across lines
within a session.

## Shell Prelude

The shell prelude extends the
[core-language prelude](../language/prelude.md). Every core prelude value
remains available, and the shell additionally imports the following functions
and objects.

### `shell`

| Name                                       | Description                                             |
| ------------------------------------------ | ------------------------------------------------------- |
| [`exit`](../api/shell/index.md#exit-code)  | Exit with a status code (default: 0)                    |
| [`cd`](../api/shell/index.md#cd-path-func) | Change directory; optionally run func in new dir        |
| [`env`](../api/shell/index.md#env)         | Access environment variables                            |
| [`args`](../api/shell/index.md#args)       | Command-line arguments ([`array`](../api/std/array.md)) |
| [`program`](../api/shell/index.md#program) | Script [`Path`](../api/fs/path.md) or `-m` module name  |

### `term`

| Name                                               | Description                                   |
| -------------------------------------------------- | --------------------------------------------- |
| [`echo`](../api/term/index.md#echo-args)           | Print sanitized arguments separated by spaces |
| [`print`](../api/term/index.md#print-options-args) | Print concatenated terminal output            |

### `proc`

| Name                                        | Description                       |
| ------------------------------------------- | --------------------------------- |
| [`sub`](../api/proc/index.md#sub-func-trim) | Capture func's output as a string |

### `proc.run`

| Name                                  | Description              |
| ------------------------------------- | ------------------------ |
| Module as [`run`](../api/proc-run.md) | Access external programs |
