# Projects

`doze` checks, tests and bundles a project. Run it as `dolang -m doze`, or
through a `doze` symlink to `dolang`, which an installation provides on Unix.

## Layout

A project is a directory, laid out by convention:

```
app/
  main.dol       # the program
  lib/
    util.dol     # module util
    util/
      net.dol    # module util.net
    store/
      mod.dol    # module store
  test/
    util.dol     # tests
```

Each `.dol` file under `lib/` is a module, named by its path: `lib/a/b.dol` or
`lib/a/b/mod.dol` is `a.b`. A path that isn't a module name, such as
`lib/my-thing.dol`, is an error, and so are two files naming the same module.
`main.dol` is the program, and is optional for a library. The `.dol` files under
`test/`, at any depth, are the tests.

Each command works on the project given with `--project`, before the command,
or else on the current directory. It doesn't search parent directories for one.

## Checking

`doze check` checks the types of every module under `lib/`, of `main.dol`, and
of the tests, together. Imports resolve to the project's modules, then to the
bundled modules. With `--strict`, warnings fail the check too.

```
doze check
doze --project path/to/app check
```

## Testing

`doze test` runs the tests with the
[`test` entrypoint](./index.md#bundled-entrypoints), with `lib/` on the module
path so the tests import the project's modules. Every argument after `test`
goes to it unchanged, and its exit status is `doze`'s. It runs in the project's
directory, so test IDs such as `test/util/parses` are the same wherever `doze`
was run from. For a project in `app`, these are the same:

```
doze --project app test --filter 'util/*'
(cd app && dolang --module-path lib -m test test --filter 'util/*')
```

`--list` lists the tests instead, and `--format` gives JSON for tools:

```
doze test --list --format json
```

## Bundling

`doze bundle` checks the project, then packs `main.dol` and every module under
`lib/` into a [script bundle](./index.md#script-bundles), each source beside its
bytecode. It needs `main.dol`, and writes nothing unless the check passes and
every source compiles. `--no-check` skips the check, so only compile errors
stop the bundle.

The bundle is written to `build/<name>.dolz` in the project, named after the
project's directory. `build/` holds only build output, so a repository can
ignore it. `-o` writes the bundle elsewhere, and `--shebang` starts it with a
`#!` line and makes it executable where the platform allows:

```
doze bundle
dolang build/app.dolz
doze bundle -o app --shebang '/usr/bin/env dolang'
./app
```

The `.dolz` extension is only a convention; `dolang` recognizes a bundle by its
contents.

## Reports

`--json FILE` writes the check's report to `FILE` as JSON, besides rendering it
on stderr. `doze check` and `doze bundle` both take it. The report is written
whenever the sources were compiled, whether or not they passed. Nothing is
written if the project can't be read, such as for an invalid module path or,
when bundling, a missing `main.dol`.

```
doze check --json build/report.json
```

The report is an object:

| Key           | Value                                                                 |
| ------------- | --------------------------------------------------------------------- |
| `sources`     | How many sources were compiled                                        |
| `checked`     | Whether their types were checked                                      |
| `errors`      | The number of errors                                                  |
| `warnings`    | The number of warnings                                                |
| `passed`      | Whether the report passed: no errors, and no warnings with `--strict` |
| `diagnostics` | An array of diagnostics                                               |

Each diagnostic has:

| Key           | Value                                                                                            |
| ------------- | ------------------------------------------------------------------------------------------------ |
| `path`        | The path of the source                                                                           |
| `severity`    | `error` or `warning`                                                                             |
| `message`     | The message                                                                                      |
| `span`        | The span the message is about                                                                    |
| `annotations` | Other highlighted spans, each with `kind` (`primary` or `context`), `message`, `path` and `span` |
| `notes`       | Each with `kind` (`info` or `help`) and `message`                                                |
| `patches`     | Suggested edits, each with `message`, `path`, `span`, and `sub`, the replacement text            |

A span is as `compile.Span` describes it: a `start` with zero-based `line` and
`column`, and a `len`, both in characters. It doesn't give an end; find one from
the source text.

```
{"start": {"line": 1, "column": 4}, "len": 3}
```
