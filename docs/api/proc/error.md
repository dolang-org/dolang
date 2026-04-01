# Error

`Error` is raised when an external program exits unsuccessfully. It is a
subtype of [`std.RuntimeError`](../std/runtime-error.md).

```
try
  run.sh -c "exit 42"
catch Error: err
  assert_eq $err.rc 42
```

`Error` exposes:

- `rc`: the numeric exit status, or `nil` if the process did not exit normally
- `signal` on Unix: the fatal signal number, or `nil` when not applicable

On Unix, a signaled process reports `signal` instead of `rc`:

```
try
  run.sh -c "kill -TERM \$\$"
catch Error: err
  assert_eq $err.rc nil
  assert_eq $err.signal 15
```

`str(err)` returns a stable process-failure message such as an exit status or
fatal signal description. `dbg(err)` includes the nominal type name together
with that message.

Spawn, lookup, and other I/O failures do not raise `Error`; they raise
[`sys.Error`](../sys/error.md) instead.
