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

### `os()`

Returns a record describing the current operating system target.

The record includes:

- `kind`: the platform name reported by the Rust standard library, such as
  `:linux:`, `:macos:`, or `:windows:`
- `archetype`: `:unix:` on Unix-like targets, `:windows:` on Windows targets

```
if (sys.os().archetype == :windows:)
  echo "running on Windows"
```
