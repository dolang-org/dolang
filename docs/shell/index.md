# Overview

`dolang` runs Do scripts and provides an interactive REPL. It extends the core
language with shell-oriented features like process spawning, environment
variable access, and file I/O.

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
- `--check` -- check a script and the modules it imports without executing
- `--module-path PATH` -- add a module search path (repeatable)
- `--import MODULE[=NAME]` -- add a module to the prelude (repeatable)
- `--import-item MODULE.ITEM[=NAME]` -- add a module item to the prelude
    (repeatable)
- `--no-cache` -- disable reading and writing the bytecode cache
- `--no-history` -- disable loading and saving REPL history
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

A script may also be precompiled bytecode, recognized by its `.dolc` extension
or its header. `dolang -m compile bytecode` writes it:

```bash
dolang -m compile bytecode script.dol -o script.dolc
dolang script.dolc
```

### Checking Scripts

`--check` runs the [type checker](../language/type-checking.md) on a
script's source without executing it:

```bash
dolang --check script.dol
```

The checker follows imports using the ordinary module search paths, including
any `--module-path` directories, and uses bundled type libraries when no source
module is found. With `--strict`, warnings fail the check too.

### Script Bundles

A script bundle packages a program and its modules as one file: a zip archive,
optionally preceded by a `#!` line. `dolang` runs a bundle as it runs a script:

```bash
dolang app world
./app world
```

The archive holds:

- `main.dol` -- the program
- `lib/` -- modules; `foo.bar` is `lib/foo/bar.dol` or `lib/foo/bar/mod.dol`
- `.dolc` files -- precompiled bytecode, used in place of a `.dol` source of
  the same name

Other members are ignored. Imports resolve inside the bundle first, then through
the [module search paths](./modules.md#module-search-paths). Each module in the
bundle runs once. Bytecode for `main.dol` is compiled as a script, and bytecode
for a module in module mode under the module's name, as
`dolang -m compile bytecode --module` does. Bytecode that fails verification is
recompiled from its source when the bundle has one. Sources in a bundle compile
with no prelude, whatever the prelude options and `--strict` say, so a bundle
runs the same however it is invoked.

`dolang -m compile bundle` builds a bundle from a main module and the modules
named with `--module NAME=PATH`, writing each source beside its bytecode:

```bash
dolang -m compile bundle src/main.dol \
  --module util=src/util.dol \
  --module util.net=src/util/net.dol \
  --shebang '/usr/bin/env dolang' \
  -o app
```

With `--shebang`, the bundle starts with that `#!` line and is made executable
where the platform allows. The builder only compiles; run
`dolang -m compile check` first to check types.

Archive offsets count from the start of the archive, so any zip tool can build a
bundle too, with the `#!` line prepended:

```bash
(cd src && zip -r ../app.zip main.dol lib)
{ echo '#!/usr/bin/env dolang'; cat app.zip; } > app
chmod +x app
```

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

`dolang -m repl` also starts the REPL. Use `dolang --no-history` to leave
history files untouched.

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
| [`sub`](shell.sub)         | Capture func's output as a string                |
| Module as [`shell`](shell) | Shell context and control                        |

### `term`

| Name                  | Description                                   |
| --------------------- | --------------------------------------------- |
| [`echo`](term.echo)   | Print sanitized arguments separated by spaces |
| [`print`](term.print) | Print concatenated terminal output            |

### `proc`

| Name              | Description           |
| ----------------- | --------------------- |
| [`run`](proc.run) | Run external programs |
