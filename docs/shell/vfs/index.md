# VFS

Do can perform operations indirectly through the `dolang-vfs` companion program.
VFS[^vfs] contexts power [containers](./containers.md),
[SSH remoting](./ssh.md), and
[privilege elevation](./admin.md).

[^vfs]: Versatile Familiar Spirit

## Context Scope

Call a [`Vfs`](../../api/shell/vfs.md) with a block to enter its context:

```
import sys
  fs:
    - Path
  shell:
    - Vfs

let remote = Vfs do run ssh -T -e none server.example.com dolang-vfs --stdio
try
  remote do
    echo $sys.os_info().os
    run hostname
    echo $Path(".").canonical()
finally
  remote.stop()
```

The context is block-scoped. When the block returns or throws, the previous
context is restored. A `Vfs` handle can be entered more than once. Each entry
starts at the working directory captured when the handle was created; `cd`
and environment changes made during one entry do not change the starting state
of later entries.

The following operations use the active context:

| Operation                                  | Target behavior                                               |
| ------------------------------------------ | ------------------------------------------------------------- |
| [`fs`](../../api/fs/index.md)              | Reads and writes the target filesystem                        |
| [`proc.run`](../../api/proc-run.md)        | Resolves and launches programs on the target                  |
| `shell.env` and `shell.cd`                 | Use target environment and path conventions                   |
| [`sys`](../../api/sys/index.md)            | Reports the target OS and CPU                                 |
| [`security`](../../api/security/index.md)  | Reports and resolves the target identity                      |
| VFS-aware extensions                       | Use files or services exposed through the active VFS          |
| Other network clients and native resources | Remain in the interpreter process unless documented otherwise |

## Paths, Scopes and Handles

A [`Path`](../../api/fs/path.md) captures its Unix or Windows path style, but
not a particular host or VFS context. Each operation interprets the path
literally in the current context at the time of use. An absolute path can
therefore name a file on any compatible target, while a relative path also
depends on that context's current working directory.

Constructing `Path` from a string uses the current target's path style.
`UnixPath` and `WindowsPath` select a style explicitly. Moving a `Path` between
contexts does not copy the file it names or retain access to its original
target.

Open files and other resource handles behave differently. A handle is bound to
the VFS context that created it, so operations on it continue to address that
context even when another context is current.

Handles remain valid only while their originating VFS context remains
connected. Calling `stop()`, losing the connection, or leaving a helper that
owns a block-scoped connection invalidates its handles. `ssh.with`,
`docker.with`, `podman.with`, `sudo.with`, and `admin.with` all tear down the
context upon leaving the provided block, so any I/O to `File`s opened in the
block must also take place within it.

Manually constructed `Vfs` instances are not automatically torn down; that is
the user's responsibility.

## Returning to the Host

[`shell.host`](../../api/shell/index.md#host-func-args) temporarily returns to
the interpreter's original host context:

```
remote do
  let remote_name = sub do run hostname
  host do
    echo "remote: $remote_name"
    echo "local: $(sub do run hostname)"
```

`host` temporarily reverts the VFS context to the interpreter's startup
context, including its original working directory and environment.

## Connections

[`Vfs func`](../../api/shell/vfs.md#vfs-func) connects to a `dolang-vfs --stdio`
server over any callable-backed byte stream.
[`Vfs.unix_socket`](../../api/shell/vfs.md#unix_socket-path) connects to a
daemon on Unix, and [`Vfs.windows_admin`](../../api/shell/vfs.md#windows_admin)
uses a private named pipe for Windows UAC elevation.

Unix-socket connections are resolved through the active context. This makes it
possible to enter an SSH host and then connect to a container VFS reachable
from that host.

Call `stop()` when a manually created handle is no longer needed. Prefer
helpers such as `ssh.with`, `docker.with`, `podman.with`, and `admin.with`; they
clean up their VFS sessions when the block returns or throws.

## Limitations

Only APIs documented as VFS-aware follow the target. For example, an HTTP
client still opens network connections from the interpreter process, and its
Unix-socket option does not translate through a container filesystem.

Interactive terminal handles are also transport-dependent. In particular,
Windows cannot reliably attach an elevated child to the unelevated caller's
console. Captured output, redirection, and pipes use ordinary handles and are
the reliable choice across remote and elevated contexts.
