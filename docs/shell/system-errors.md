# System Errors

System and I/O failures raise [`sys.Error`](sys.Error) or one of its
categorized subclasses.

Every stable portable I/O category has a corresponding subclass. This makes
common network, filesystem, resource, data, and control-flow failures catchable
without inspecting platform-specific codes. [`sys.Error`](sys.Error)
remains their catch-all superclass and represents failures without a portable
classification.

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

[`sys.Error.code`](sys.Error.code) is `nil` when no native code is
available. Otherwise it is one of:

- [`sys.linux.Errno`](sys.linux.Errno)
- [`sys.freebsd.Errno`](sys.freebsd.Errno)
- [`sys.macos.Errno`](sys.macos.Errno)
- [`sys.windows.WinError`](sys.windows.WinError)

All three extend [`sys.ErrorCode`](sys.ErrorCode). The `.value`
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

Libraries can construct or subclass these errors while retaining a native
code:

```
class ServiceError: sys.Error
  def (init) self message code
    sys.Error.(init) $self $message code: $code

throw ServiceError "service failed" $sys.linux.Errno.EIO
```

Errors reflect the current VFS target, not the shell host. A Linux host
operating through a Windows VFS receives `sys.windows.WinError` codes; a Windows
interpreter operating on Linux receives `Errno`.

Internal transport or protocol failures can occur before a target operation
returns. They still raise `sys.Error`, but may not contain a native code, or may
contain a code for the host system rather than the target.
