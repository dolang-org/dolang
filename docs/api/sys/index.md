# sys

The `sys` module exposes facts about the current target system and system error
types.

## Types

| Type                                                    | Description                                   |
| ------------------------------------------------------- | --------------------------------------------- |
| [`Error`](./error.md)                                   | Error raised for system and I/O failures      |
| [`NotFoundError`](./not-found-error.md)                 | Subtype for missing files, paths, or programs |
| [`PermissionDeniedError`](./permission-denied-error.md) | Subtype for permission failures               |
| [`AlreadyExistsError`](./already-exists-error.md)       | Subtype for existing-path conflicts           |
| [`TimedOutError`](./timed-out-error.md)                 | Subtype for timed-out system operations       |

## Functions

### `os_info()`

Returns a record describing the current operating system target.

The record includes values from Rust's
[`std::env::consts`](https://doc.rust-lang.org/std/env/consts/index.html)
module:

| Field    | Meaning                   | Typical values                    |
| -------- | ------------------------- | --------------------------------- |
| `os`     | Specific operating system | `:linux:`, `:macos:`, `:windows:` |
| `family` | Operating system family   | `:unix:`, `:windows:`             |

```
if (sys.os_info().family == :windows:)
  echo "running on Windows"
```

### `cpu_info()`

Returns a record describing the current CPU target.

The record includes values from Rust's
[`std::env::consts`](https://doc.rust-lang.org/std/env/consts/index.html) module
and
[`std::thread::available_parallelism`](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html).

| Field           | Meaning             | Typical values          |
| --------------- | ------------------- | ----------------------- |
| `arch`          | Target architecture | `:x86_64:`, `:aarch64:` |
| `logical_count` | Logical CPU count   | >= 1                    |

```
let info = sys.cpu_info()
echo "running on $info.arch with $info.logical_count logical CPUs"
```
