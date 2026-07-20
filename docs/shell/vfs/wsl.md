# WSL

The `wsl` module crosses between Windows and WSL Linux using a block-scoped
stdio VFS.

## Entering Linux from Windows

`with_linux` starts `dolang-vfs` in a WSL distribution. The distribution and
user are optional:

```
import wsl sys

wsl.with_linux distro: Ubuntu user: builder do
  echo $sys.os_info().os
  run uname -a
```

The selected distribution must provide `dolang-vfs` in its command search
path. Use `command:` to supply another launcher prefix.

## Entering Windows from WSL

`with_windows` uses WSL executable interoperability:

```
import wsl sys

wsl.with_windows do
  echo $sys.os_info().os
  run cmd.exe /c ver
```

It prefers `dolang-vfs.exe` and falls back to `dolang.exe --vfs`. Both are
resolved through the active Linux environment's command search path. Use
`command:` to override discovery.

When the interpreter originally started on Windows, use
[`shell.with_host`](../../api/shell/index.md#with_host-func-args) to return
temporarily from a nested Linux VFS. `with_windows` is intended for an
interpreter that started within WSL and has no Windows startup context to
restore.

Windows UAC elevation composes with the transition:

```
import wsl admin

wsl.with_windows do
  admin.with do
    echo elevated
```

## Directory and Environment Overrides

Both functions accept `cd:` and `env:`. These describe the destination, so use
an [`UnixPath`](../../api/fs/unix-path.md) when entering Linux and a
[`WindowsPath`](../../api/fs/windows-path.md) when entering Windows if the path
must be constructed before entering that context.

Environment keys may be strings or symbols. A `nil` value unsets the variable;
`:INHERIT:` copies its current value before crossing the boundary.

The destination VFS is stopped when the block returns or throws.
