# Error

`Error` is raised for system and I/O failures.

```
try
  fs.read "/definitely/missing"
catch Error: err
  echo $str(err)
```

`str(err)` returns the underlying system error message.

`Error` exposes an `errno` field containing the underlying Unix error number
when one exists:

```
try
  fs.read "/definitely/missing"
catch Error: err
  assert_eq $err.errno 2
```

On Windows, accessing `errno` raises [`FieldError`](../std/field-error.md). For
Unix failures without an OS error number, `errno` is `nil`.

On Windows, `winerror` contains the underlying Win32 error code when one
exists. On Unix, accessing `winerror` raises `FieldError`.

## Inherits

- [`RuntimeError`](../std/runtime-error.md)
