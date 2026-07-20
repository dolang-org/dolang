# Privilege Escalation

The `admin` module queries and acquires administrator privileges without making
the caller choose between Unix `sudo` and Windows User Account Control (UAC).
The elevated block is a [VFS context](./index.md), so filesystem
operations and external programs both use the elevated identity.

## Portable Administration

Use `admin.with` for code that requires the platform's administrator identity:

```
import admin

admin.with do
  echo "administrator: $(admin.query())"
```

On Unix, `admin.with` uses `sudo`. On Windows, it requests elevation through
UAC. If [`admin.query()`](../../api/admin.md) reports that the current context
is already elevated, the block runs directly without creating another context.

## Unix `sudo`

[`sudo.run`](../../api/sudo.md) elevates one verbatim command:

```
import sudo

sudo.run install -m 0644 example.conf /etc/example.conf
```

[`sudo.with`](../../api/sudo.md) runs a complete block through a temporary VFS:

```
import sudo
import fs:
  - Path

let shadow = sudo.with do (Path /etc/shadow).read()
```

Both accept `user:` to run as an account other than root. They also accept `cd:`
and `env:` overrides. For `sudo.run`, these use sudo's native `-D` and
`--preserve-env` facilities and remain subject to sudoers policy. `sudo.with`
starts a `dolang-vfs` daemon through `sudo`, connects through a private Unix
socket, and cleans up the session when the block returns or throws. Inside an
SSH context, the same process runs on the remote Unix host.

## Windows UAC

On a Windows VFS target, `admin.with` launches an elevated copy of the target's
current executable and connects to its VFS server through a private named pipe.
This also works when the Windows context was entered from WSL. The target
chooses its own executable; the caller cannot supply one. The current working
directory when the context is created becomes its initial directory. Cancelling
the UAC prompt raises a permission error.

[`Vfs.windows_admin()`](../../api/shell/vfs.md#windows_admin-cd-env) exposes the
lower-level handle when its lifetime must be controlled directly:

```
import shell:
  - Vfs

let elevated = Vfs.windows_admin()
try
  elevated.with do
    run sc.exe query example
finally
  elevated.stop()
```

Windows does not reliably allow an elevated process to use console handles
from its unelevated caller. Interactive console programs may hang, fail, or
produce no output. Captured output, redirected files, and pipes use ordinary
handles and work across the elevated boundary.

UAC support elevates the local Windows interpreter. It does not elevate a
remote Windows host selected through SSH.
