# System Errors

System and I/O failures raise [`sys.Error`](../api/sys/error.md) or one of its
categorized subclasses.

## Catching Categorized Failures

```
import fs sys

try
  fs.read config.ini
catch sys.NotFoundError: error
  echo "configuration is missing: $error"
catch sys.PermissionDeniedError: error
  echo "configuration is not readable: $error"
```

## Native Error Codes

[`sys.Error.code`](../api/sys/error.md#code) is `nil` when no native code is
available. Otherwise it is one of:

- [`sys.linux.Errno`](../api/sys/linux/errno.md)
- [`sys.macos.Errno`](../api/sys/macos/errno.md)
- [`sys.windows.WinError`](../api/sys/windows/win-error.md)

All three extend [`sys.ErrorCode`](../api/sys/error-code.md). The `.value`
field is the raw integer value; string conversion returns the native symbolic
name when known:

```
import fs sys

try
  fs.read /definitely/missing
catch sys.Error: error
  if error.code == nil
    echo $error
  else
    echo "$(error.code): raw value $(error.code.value)"
```

Use native codes only when recovery genuinely depends on a platform-specific
condition. Prefer the categorized subclasses otherwise.

Errors reflect the current VFS target, not the shell host. A Linux host
operating through a Windows VFS receives `sys.windows.WinError` codes; a Windows
interpreter operating on Linux receives `Errno`.

Internal transport or protocol failures can occur before a target operation
returns. They still raise `sys.Error`, but may not contain a native code, or may
contain a code for the host system rather than the target.
