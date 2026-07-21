# Privilege Elevation

The `admin` module queries and acquires administrator privileges in a
cross-platform manner. The elevated block runs in a modified [VFS
context](./index.md), so filesystem operations and external programs both use
the elevated identity.

Use `admin.with` for code that requires administrator privileges, such as
installing files to system paths:

```
import admin

admin.with do
  echo "administrator: $(admin.query())"
```

On Unix, `admin.with` uses `sudo`. On Windows, it requests elevation through
UAC. If [`admin.query()`](../../api/admin.md) reports that the current context
is already elevated, the block runs directly without creating another context.

## Unix `sudo`

The `sudo` module provides more granular use of `sudo` on Unix targets.
[`sudo.run`](../../api/sudo.md) elevates one verbatim command:

```
import sudo

sudo.run install -m 0644 example.conf /etc/example.conf
```

[`sudo.with`](../../api/sudo.md) runs a complete block through a scoped VFS
context:

```
import sudo fs

let shadow = sudo.with do fs.read /etc/shadow
```

Both accept `user:` to run as an account other than root. They also accept
`cd:` and `env:` overrides. For `sudo.run`, these use sudo's native `-D` and
`--preserve-env` facilities and remain subject to sudoers policy. `sudo.with`
starts a `dolang-vfs` daemon through `sudo`, connects through a private Unix
socket, and cleans up the session when the block returns or throws. It also
works remotely via an existing SSH context.

With no `env:` entries, `sudo.with` starts from the environment permitted by
`sudo -E` and the active sudoers policy. An explicit value sets a variable,
`nil` unsets it, and `:INHERIT:` captures the current strand value before
elevation.

## Windows UAC

On a Windows VFS target, `admin.with` launches an elevated copy of the target's
current executable and communicates with it via a private named pipe. It also
also works when the Windows context was entered from WSL. Cancelling the UAC
prompt raises a permission error.

[`Vfs.windows_admin()`](../../api/shell/vfs.md#windows_admin-cd-env) exposes the
lower-level context when its lifetime must be controlled directly:

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

Windows does not reliably allow an elevated process to use console handles from
its unelevated parent. Interactive console programs may hang, fail, or produce
no output. Captured or redirected output and pipelines should work correctly.

Since UAC generally requires a user to be physically present, it is not usable
over SSH. However, administrator accounts generally receive a full elevated
token when connecting over SSH, in which case `admin.with` will execute the
supplied block directly.
