# Error

`Error` is raised for system and I/O failures.

```
try
  fs.read "/definitely/missing"
catch Error: err
  echo $str(err)
```

`str(err)` returns the underlying system error message.

On Unix, `Error` exposes an `errno` field containing the underlying OS
error number when one exists:

```
try
  fs.read "/definitely/missing"
catch Error: err
  assert_eq $err.errno 2
```

If the failure did not originate from an OS errno, `errno` is `nil`.

## Inherits

- [`RuntimeError`](../std/runtime-error.md)
