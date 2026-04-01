# Error

`Error` is raised for system and I/O failures originating from the shell
extension. It is a subtype of
[`std.RuntimeError`](../std/runtime-error.md), so it can be caught
either specifically or through the broader runtime error type.

```
try
  fs.read "/definitely/missing"
catch Error: err
  echo $str(err)
```

`str(err)` returns the underlying system error message. `dbg(err)` includes the
nominal type name together with that message.

On Unix, `Error` exposes an `errno` field containing the underlying OS
error number when one exists:

```
try
  fs.read "/definitely/missing"
catch Error: err
  assert_eq $err.errno 2
```

If the failure did not originate from an OS errno, `errno` is `nil`.
