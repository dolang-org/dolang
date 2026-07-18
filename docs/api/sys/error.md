# Error

`Error` is raised for system and I/O failures.

```
try
  fs.read "/definitely/missing"
catch Error: err
  echo $str(err)
```

`str(err)` returns the underlying system error message and appends the native
symbolic code in parentheses when it is known.

## Fields

### `code`

`code` contains the underlying native system error code when one exists:

```
try
  fs.read "/definitely/missing"
catch Error: err
  echo $err.code
```

The value is [`sys.linux.LinuxErrno`](./linux/linux-errno.md),
[`sys.macos.MacosErrno`](./macos/macos-errno.md), or
[`sys.windows.WinError`](./windows/win-error.md), according to the system where
the error originated. Errors without a native code expose `nil`.

## Inherits

- [`RuntimeError`](../std/runtime-error.md)
