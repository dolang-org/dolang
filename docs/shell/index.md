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

Other options:

- `-m`, `--main` -- run a bundled entrypoint instead of a script
- `--check` -- check syntax without executing
- `--compile OUTPUT` -- compile to bytecode file
- `--module-path PATH` -- add a module search path (repeatable)
- `--import MODULE[=NAME]` -- add a module to the prelude (repeatable)
- `--import-item MODULE.ITEM[=NAME]` -- add a module item to the prelude
    (repeatable)
- `--no-cache` -- disable reading and writing the bytecode cache
- `-h`, `--help` -- show command-line help

Explicit module paths are searched in command-line order before the `site/`
directory and bundled modules. See [Modules and Caching](./modules.md).

Prelude options supplement [the default](#shell-prelude). An alias after `=`
changes the name bound in the script:

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
The executed script path is available as `shell.program`.

### Bundled Entrypoints

`-m` runs an entrypoint bundled with `dolang`:

```
dolang -m dodo --list
dolang -m test -- test
```

Symlink aliases such as `dodo` and `dolang-test` select the corresponding
entrypoint implicitly when available.

### Companion Programs

A complete installation contains three main executables:

- `dolang` -- script executor, bundled entrypoints, and REPL
- `dolang-lsp` -- language server
- `dolang-vfs` -- VFS server

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

| Name                       | Description                                      |
| -------------------------- | ------------------------------------------------ |
| [`exit`](shell.exit)       | Exit with a status code (default: 0)             |
| [`cd`](shell.cd)           | Change directory; optionally run func in new dir |
| [`env`](shell.env)         | Access environment variables                     |
| Module as [`shell`](shell) | Shell context and control                        |

### `term`

| Name                  | Description                                   |
| --------------------- | --------------------------------------------- |
| [`echo`](term.echo)   | Print sanitized arguments separated by spaces |
| [`print`](term.print) | Print concatenated terminal output            |

### `proc`

| Name              | Description                       |
| ----------------- | --------------------------------- |
| [`sub`](proc.sub) | Capture func's output as a string |
| [`run`](proc.run) | Run external programs             |
