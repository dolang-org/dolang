# Testing

A test module is a `.dol` file, conventionally under `test/`, that registers
tests with the [`test`](test) module as it loads. `dolang -m test` loads each
module and runs the tests it registered.

## Writing Tests

The [`test`](test.test) decorator registers a function taking no arguments:

```
import test:
  - assert_eq
  - assert_throws
  - test

#[test]
def adds()
  assert_eq (1 + 2) 3

#[test name: "rejects mixed types"]
def rejects_mixed()
  assert_throws $std.TypeError do (1 + "a")
```

A test is named after its function unless `name:` gives another name. Its
[ID](test.TestInfo.id) is `module/name`, where `module` is the file's path
without its extension: `test/math/adds` for the first test above in
`test/math.dol`.

A test fails when it raises an error. The assertion functions raise
[`Assert`](test.Assert):

| Function                                 | Asserts                               |
| ---------------------------------------- | ------------------------------------- |
| [`assert`](test.assert)                  | A value is truthy                     |
| [`assert_not`](test.assert_not)          | A value is falsy                      |
| [`assert_eq`](test.assert_eq)            | Two values are equal                  |
| [`assert_ne`](test.assert_ne)            | Two values are not equal              |
| [`assert_throws`](test.assert_throws)    | A block raises an error of a type     |
| [`assert_type`](test.assert_type)        | A value has a type                    |

`assert_throws` returns the error, for further checks.

Modules load concurrently, and so do tests. A test or module reads no input,
and its output is captured and shown when it fails. A module that compiles with
warnings fails like one with errors.

### Skipping Tests

[`skip`](test.skip) ends a test and reports it as skipped.
[`ignore`](test.ignore) ends it and leaves it out of the report, for tests that
don't apply where they run. Called while the module loads, either one applies to
the whole module:

```
if (shell.env.get("DATABASE_URL") == nil)
  skip "set DATABASE_URL to run database tests"
```

### Tags

Tags label tests for [selection](./testing.md#selecting-tests). `test` takes
them with `tags:`, and [`tags`](test.tags) adds tags to every test the module
registers after it:

```
tags "integration"

#[test tags: ["slow"]]
def migrates()
  migrate()
```

## Fixtures

[`fixture`](test.fixture) turns a function into a decorator that wraps a test
with setup and teardown. The function receives the test and the arguments left
for it, and calls the test with those arguments and any keys it adds. The test
takes the added keys as keyword parameters:

```
import fs:
  - with_temp_dir
import test:
  - assert_eq
  - fixture
  - test

#[fixture]
def temp_dir block ...rest
  with_temp_dir do |dir|
    block dir: $dir ...rest

#[test]
#[temp_dir]
def writes_file :dir
  let path = (dir / "out.txt")
  path.write "hello"
  assert_eq $path.read_str() hello
```

Decorators apply from the bottom up, each wrapping the result of the one below,
so `test` goes on top. A fixture can call `skip` or `ignore` before the test
runs.

## Parametrization

[`parametrize`](test.parametrize) runs a test once for each value of an axis,
passing the value as the keyword argument of the axis's name. Stacked
`parametrize` decorators run every combination of their axes:

```
#[test]
#[parametrize size: [1, tagged(1000, "slow")]]
#[parametrize mode: ["text", "binary"]]
def round_trips :mode :size
  check_round_trip $mode $size
```

Each case's name adds its axes to the test's name, those closest to the `def`
first: `round_trips/mode=text/size=1`, `round_trips/mode=text/size=1000`, and
so on. [`tagged`](test.tagged) tags a single value, so only the cases with
`size=1000` are tagged `slow`.

A fixture below a `parametrize` receives its axes among the arguments it passes
on:

```
#[test]
#[parametrize mode: ["text", "binary"]]
#[temp_dir]
def writes_in_mode :dir :mode
  write_file (dir / "out") $mode
```

## Running Tests

`dolang -m test`, or its alias `dolang-test`, runs the tests in the given files
and directories, searching directories recursively for `.dol` files:

```bash
dolang -m test test
dolang -m test --filter 'test/math/*' test/math.dol
```

It runs them with [`test.run`](test.run), or lists them with
[`test.list`](test.list) under `--list`. In a project,
[`doze test`](../tooling/doze.md#testing) runs it with the project's `lib/` on
the module path.

### Options

| Option                  | Default | Description                                    |
| ----------------------- | ------- | ---------------------------------------------- |
| `-j`, `--jobs N`        | 8       | Maximum concurrent tests across all modules    |
| `--module-jobs N`       | 4       | Maximum concurrent modules                     |
| `--timeout SECS`        | 600     | Time limit for each test and each module load  |
| `--no-capture`          |         | Print output as it is written                  |
| `--filter GLOB`         | `**`    | Select tests whose ID matches                  |
| `--tags TAGS`           |         | Select tests by tag; repeatable                |
| `--id ID`               |         | Select a test by exact ID; repeatable          |
| `--list`                |         | List the selected tests without running them   |
| `--format json\|jsonl`  |         | Write a JSON listing or JSONL progress events  |
| `-o`, `--output FILE`   |         | Write `--format` output to `FILE`              |

`--format json` requires `--list`, and `--format jsonl` applies only to runs,
without `--no-capture`. `--output` requires `--format`.

### Selecting Tests

`--filter` takes a glob matched against each test's ID. `*` matches within a
path segment, and a `**` segment matches any number of them, so
`'**/round_trips/**'` selects every case of `round_trips`.

The repeatable `--tags` option splits each occurrence on commas: alternatives in
one occurrence are ORed, while occurrences are ANDed. Prefix a tag with `!` to
require its absence. Matching is exact and case-sensitive.

`--tags !windows,!macos` means “not Windows OR not macOS.” Use
`--tags !windows --tags !macos` to exclude both.

The repeatable `--id` option selects tests by exact ID, with no glob syntax to
escape. A test must match `--filter`, `--tags` and `--id` to be selected.

### Exit Status

A run exits with zero when every selected test passes, and one when any module
or test fails. Skipped and ignored tests don't fail a run.

With `--list`, the exit status is one when a module fails to compile or load,
and zero otherwise, including when a module skips while loading.

### Listing Tests

`--list` loads each module and reports the selected tests without running them.
`--jobs` and `--no-capture` don't apply.

By default the list is a report on the console. `--format json` instead
writes one JSON document to standard output, or to the file given by
`--output`:

```json
{
  "version": 1,
  "modules": [
    {
      "path": "test/foo.dol",
      "module": "test/foo",
      "status": "loaded",
      "tests": [
        {
          "id": "test/foo/connects/backend=sqlite",
          "module": "test/foo",
          "name": "connects/backend=sqlite",
          "base": "connects",
          "params": [["backend", "sqlite"]],
          "tags": ["slow"],
          "location": {"file": "test/foo.dol", "line": 12, "column": 0, "len": 12}
        }
      ]
    },
    {
      "path": "test/bar.dol",
      "module": "test/bar",
      "status": "failed",
      "error": {"kind": "compile", "message": "...", "diagnostics": [...]}
    }
  ]
}
```

- `version` changes when the format changes incompatibly.
- Modules appear in path order, and tests in registration order. Paths are
  as given on the command line, with directories expanded.
- `status` is `loaded`, `failed`, `skipped` or `ignored`. A skipped or ignored
  module has a `message`, which may be `null`.
- A test's fields are those of [`TestInfo`](test.TestInfo). `location` is
  `null` when unknown.
- A failed module's `error` has a `kind` of `compile` or `load` and a
  `message`. A compile error lists `diagnostics`, each with a `severity`
  (`error` or `warning`), `message` and `location`. A load error has
  `rendered`, the error as the run report prints it, with its backtrace. An
  `output` field holds what the module wrote while loading, when it wrote
  anything.
- Locations are zero-based, with columns and lengths in characters, as in
  [`inspect.Location`](inspect.Location).

### Progress Events

`--format jsonl` runs the tests and writes progress events to standard
output, or to the file given by `--output`, in place of the report. Each line
is one JSON object, written as events happen, so events from concurrent
modules and tests interleave. Output is always captured, so `--no-capture` is
rejected. The exit status is the same as for the report.

```json
{"event": "run-start", "version": 1, "modules": 1}
{"event": "module-start", "path": "test/foo.dol", "module": "test/foo"}
{"event": "module-end", "path": "test/foo.dol", "module": "test/foo", "status": "loaded", "tests": [...]}
{"event": "test-start", "id": "test/foo/adds", "module": "test/foo", "name": "adds", ...}
{"event": "test-end", "id": "test/foo/adds", "outcome": "failed", "duration": 0.002, "message": "assertion failed: 3 == 4", "error": {...}, "failure_location": {...}}
{"event": "run-end", "passed": 0, "failed": 1, "skipped": 0, "ignored": 0, "status": 1}
```

Each event's `event` field names it:

- `run-start` comes first, with the format `version` and the number of
  `modules`.
- `module-start` has the module's `path` and `module`.
- `module-end` follows when the module has loaded, or failed to. Its fields
  are those of a module in the `--list` document.
- `test-start` has the fields of a test in the `--list` document.
- `test-end` has the test's `id`, an `outcome` of `passed`, `failed`,
  `skipped` or `ignored`, and its `duration` in seconds. Ignored tests are
  reported, unlike in the report. Unless the test passed, it also has:
    - `message`: the [`Skip`](test.Skip) or [`Ignore`](test.Ignore) message,
      which may be `null`, or the string form of the error.
    - `failure_location`: the location of the innermost call in the test's
      file when the error was raised, or `null`. This is the failing line
      even when the error comes from an assertion function, a fixture or a
      helper.
    - `output`: what the test wrote, when it wrote anything.
    - `error`, for a failure: the error's `type`, the `rendered` error as the
      report prints it, and the `backtrace`, innermost call first. Each frame
      has the `module`, `receiver`, `method` and `location` of
      [`inspect.Frame`](inspect.Frame). An [`Assert`](test.Assert) error from
      a comparison adds its `expected` and `actual` values.
- `run-end` comes last, with the number of tests `passed`, `failed`,
  `skipped` and `ignored`, and the exit `status`.
