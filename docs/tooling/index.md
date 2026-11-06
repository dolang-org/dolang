# Projects

A project is a directory holding a program, its modules and its tests.
[`doze`](./doze.md) checks, tests and bundles one, and the
[language server](./lsp.md) and [VS Code extension](./vscode.md) find the
project of each file they open.

## Layout

A project is laid out by convention:

```text
app/
  doze.toml      # configuration
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

An import resolves to the project's modules first, then to the bundled modules,
so a module under `lib/` takes the place of a bundled module of the same name.

[`example/eol`](https://github.com/dolang-org/dolang/tree/master/example/eol) is
a complete project: a client for the endoflife.date API, with a program, a
module and its submodule, and tests that run against a mock server.

## Finding the Root

`doze` works on the project given with `--project`, or else on the current
directory. It doesn't search parent directories for one.

Editors find a file's project by convention. Its root is the nearest directory
above the file that contains a `doze.toml` or `.git`, or else the file's own
directory. So a project nested in a repository is a root of its own, and so is a
submodule or worktree, whose `.git` is a file.

## Configuration

A `doze.toml` file at the root describes the project. It is optional: without
it, the project is its layout alone.

```toml
[project]
name = "app"
version = "0.1.0"
authors = ["A. Developer <dev@example.com>"]
license = "MIT"
description = "Deploys the app."
```

Every key in the `project` table is optional, as is the table itself:

| Key           | Value                                                                   |
| ------------- | ----------------------------------------------------------------------- |
| `name`        | The project's name, which names its bundle; by default, its directory's |
| `version`     | A string                                                                |
| `authors`     | An array of strings                                                     |
| `license`     | A string                                                                |
| `description` | A string                                                                |

Only `name` is used so far. A key or table `doze` doesn't know, a value of the
wrong type, and a `name` that can't name a file, such as one with a `/`, are
errors, so a misspelled key doesn't go unnoticed.
