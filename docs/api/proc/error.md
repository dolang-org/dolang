# Error

`Error` is raised when an external program exits unsuccessfully.

```
try
  run.sh -c "exit 42"
catch Error: err
  assert_eq $err.rc 42
```

`Error` exposes:

- `rc`: the numeric exit status, or `nil` if the process did not exit normally
- `signal` on Unix: the fatal signal number, or `nil` when not applicable

On Windows, accessing `signal` raises [`FieldError`](../std/field-error.md).

On Unix, a signaled process reports `signal` instead of `rc`:

```
try
  run.sh -c r"kill -TERM $$"
catch Error: err
  assert_eq $err.rc nil
  assert_eq $err.signal 15
```

`str(err)` returns a stable process-failure message such as an exit status or
fatal signal description.

Spawn, lookup, and other I/O failures do not raise `Error`; they raise
[`sys.Error`](../sys/error.md) instead.

## Inherits

- [`RuntimeError`](../std/runtime-error.md)
