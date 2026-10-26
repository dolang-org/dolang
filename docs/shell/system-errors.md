# System Errors

System and I/O failures raise [`sys.Error`](sys.Error) or one of its
categorized subclasses.

The subclasses cover common network, filesystem, resource, data, and
control-flow failures, so a script can catch them without inspecting
platform-specific codes. A failure that fits none of them raises
[`sys.Error`](sys.Error) itself.

## Catching Categorized Failures

```
import fs sys

try
  fs.read_str config.ini
catch sys.NotFoundError: error
  echo "configuration is missing: $error"
catch sys.PermissionDeniedError: error
  echo "configuration is not readable: $error"
```

## Native Error Codes

[`sys.Error.code`](sys.Error.code) is `nil` when no native code is
available. Otherwise it is one of:

- [`sys.linux.Errno`](sys.linux.Errno)
- [`sys.freebsd.Errno`](sys.freebsd.Errno)
- [`sys.macos.Errno`](sys.macos.Errno)
- [`sys.windows.WinError`](sys.windows.WinError)

Each extends [`sys.ErrorCode`](sys.ErrorCode). The `.value`
field is the raw integer value; string conversion returns the native symbolic
name when known:

```
import fs sys

try
  fs.read_str /definitely/missing
catch sys.Error: error
  if error.code == nil
    echo $error
  else
    echo "$(error.code): raw value $(error.code.value)"
```

Use native codes only when recovery genuinely depends on a platform-specific
condition. Prefer the categorized subclasses otherwise.

Libraries can construct or subclass these errors while retaining a native
code:

```
class ServiceError: sys.Error
  def (init) self message code
    sys.Error.(init) $self $message code: $code

throw ServiceError "service failed" $sys.linux.Errno.EIO
```

Errors reflect the current [VFS](./vfs.md) target, not the shell host. A Linux
host operating through a Windows VFS receives `sys.windows.WinError` codes; a
Windows interpreter operating on Linux receives `Errno`.

A failure in the connection to a remote target, rather than in the operation
itself, still raises `sys.Error`, but its code may be `nil` or may come from
the host system rather than the target.
