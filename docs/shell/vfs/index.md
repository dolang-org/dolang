# VFS

Do can perform operations indirectly through the `dolang-vfs` companion program.
VFS[^vfs] contexts power [containers](./containers.md),
[SSH remoting](./ssh.md), [WSL transitions](./wsl.md), and
[privilege elevation](./admin.md).

[^vfs]: Versatile Familiar Spirit

## Architecture

The Do interpreter and program state remain in the original process.
`dolang-vfs` runs on the selected target and performs target-side operations
on the interpreter's behalf:

| Value or operation                                           | Context behavior                                 |
| ------------------------------------------------------------ | ------------------------------------------------ |
| `fs`, `proc.run`, `shell.env`, `shell.cd`, `sys`, `security` | Follow the active target                         |
| VFS-aware extensions                                         | Follow the target as documented by the extension |
| HTTP and ordinary network clients                            | Stay in the interpreter process                  |
| `Path` values                                                | Carry a path style, but no target identity       |
| Open files and other handles                                 | Remain bound to the target that created them     |

Contexts may be nested. For example, a local interpreter can enter an SSH host
and then connect to a container VFS on that host. Leaving a context restores
the complete previous target.

## Context Scope

Call [`Vfs.with`](../../api/shell/vfs.md#with-func-args) with a block to enter
its context:

```
import sys
  fs:
    - Path
  shell:
    - Vfs

let remote = Vfs do run ssh -T -e none server.example.com dolang-vfs --stdio
try
  remote.with do
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

| Operation                                 | Target behavior                                               |
| ----------------------------------------- | ------------------------------------------------------------- |
| [`fs`](../../api/fs/index.md)             | Reads and writes the target filesystem                        |
| [`proc.run`](../../api/proc-run.md)       | Resolves and launches programs on the target                  |
| `shell.env` and `shell.cd`                | Use target environment and path conventions                   |
| [`sys`](../../api/sys/index.md)           | Reports the target OS and CPU                                 |
| [`security`](../../api/security/index.md) | Reports and resolves the target identity                      |
| VFS-aware extensions                      | Use files or services exposed through the active VFS          |
| Other APIs                                | Remain in the interpreter process unless documented otherwise |

## Paths, Scopes and Handles

A [`Path`](../../api/fs/path.md) captures its Unix or Windows path style, but
not a particular host or VFS context. Each operation interprets the path
literally in the current context at the time of use. An absolute path can
therefore name a file on any compatible target, while a relative path also
depends on that context's current working directory.

Constructing `Path` from a string uses the current target's path style.
`UnixPath` and `WindowsPath` select a style explicitly. Moving a `Path` between
contexts does not retain access to its original target.

Open files and other handles behave differently. A handle is bound to the VFS
context that created it, so operations on it continue to address that context
even when another context is current.

Handles remain valid only while their originating VFS context remains
connected. Calling `stop()`, losing the connection, or leaving a helper that
owns a block-scoped connection invalidates its handles. `ssh.with`,
`docker.with`, `podman.with`, `wsl.with_linux`, `wsl.with_windows`, `sudo.with`,
and `admin.with` all tear down the context upon leaving the provided block, so
any I/O to `File`s opened in the block must also take place within it.

Manually constructed `Vfs` instances are not automatically torn down; that is
the user's responsibility.

## Returning to the Host

[`shell.with_host`](../../api/shell/index.md#with_host-func-args) temporarily
returns to the interpreter's original host context:

```
remote.with do
  let remote_name = sub do run hostname
  with_host do
    echo "remote: $remote_name"
    echo "local: $(sub do run hostname)"
```

`with_host` temporarily reverts the VFS context to the interpreter's startup
context, including its original working directory and environment.

## Connections

[`Vfs func`](../../api/shell/vfs.md#vfs-func) connects to any strand which
speaks the VFS protocol on its input and output pipe, typically by immediately
running `dolang-vfs --stdio`.
[`Vfs.unix_socket`](../../api/shell/vfs.md#unix_socket-path) connects to a
`dolang-vfs --listen <socket_path>` instance on Unix, and
[`Vfs.windows_admin`](../../api/shell/vfs.md#windows_admin-cd-env)
performs Windows UAC elevation.

Unix-socket and Windows administrator connections are resolved through the
active context. This makes it possible to enter an SSH host and then connect to
a container VFS reachable from that host, or to UAC elevate from
WSL.

Call `stop()` when a manually created handle is no longer needed to shut down
the VFS process and clean up related local resources. Prefer helpers such as
`ssh.with`, `docker.with`, `podman.with`, `wsl.with_linux`, and `admin.with`;
they clean up their VFS sessions when the block returns or throws.

### Errors

Target system errors cross the transport without being converted to the
interpreter host's error model. A Windows target returns Windows native error
codes even when the interpreter runs on Linux; see
[System Errors](../system-errors.md).

### Version Compatibility

The VFS protocol does not currently negotiate versions. Use `dolang` and
`dolang-vfs` from the same build or release. Other combinations are unsupported
and may fail with a protocol error. Protocol versioning is planned for a
stable release.

### Trust Boundary

`dolang-vfs` executes arbitrary operations with its target identity. Always
restrict access to its client endpoint. Do not run the client endpoint on a
system you do not trust, as a local administrator could possibly take control
of it. The protocol is not encrypted, so always tunnel it over a secure
channel such as SSH if traversing a network.

## Limitations

Only APIs documented as VFS-aware follow the target. For example, an HTTP
client still opens network connections from the interpreter process.
