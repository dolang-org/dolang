# Container Support

Do can operate inside containers through the `dolang-shell-vfs`
daemon. This enables running external programs and accessing the filesystem
in a container while running on the host.

## Architecture

The shell VFS is a daemon that runs inside the container. It accepts
connections on a Unix socket and spawns processes on behalf of connected
clients. File descriptors are passed through using `SCM_RIGHTS`, allowing
stdin/stdout/stderr to be piped between host and container.

## Setting Up the VFS

1. **Copy the VFS binary into the container.** The VFS binary is
   `dolang-shell-vfs`.

2. **Run the VFS daemon with a shared socket.** Mount a host directory into the
   container for the Unix socket. For example:

    ```
    let rundir = env.get XDG_RUNTIME_DIR else: do "/run/usr/$(sub run.id "-u")"
    let vfsdir = "$rundir/vfs"
    run.create_dir -p $vfsdir
    run.docker run -v $vfsdir:/tmp/vfs:rw my-container --
      /tmp/dolang-shell-vfs /tmp/vfs/socket
    ```

3. **Connect from Do code:**

    ```
    import shell: Vfs

    let a = Vfs unix_socket: $vfsdir/socket
    a do
      run.cat /etc/os-release
    a.stop
    ```

## VFS Context

Within the VFS context (the block passed to it):

- [`shell.env`](../api/shell/index.md#env) reads and writes the container's
  environment (modifications do not persist after leaving context)
- [`shell.cd`](../api/shell/index.md#cd-path-func) changes the container's
  working directory (modifications also not persistent)
- Launched external programs run inside the container
- Filesystem operations are redirected to the container

## Returning to Host Context

The [`shell.host`](../api/shell/index.md#host-func-args) function provides a way
to temporarily return to the host environment from within a VFS context. This is
useful when you need to access host resources or execute commands that should
not run inside the container.

### When to Use `host()`

- Access host files or services not available in the container
- Execute commands that require host permissions
- Temporarily reset environment variables to host defaults
- Perform operations that need to be isolated from container state

### Context Reset Behavior

The `host()` function always creates a "fresh" host context:

- Working directory is reset to the original startup working directory of
  `dolang-shell`
- Environment variables are reset to their startup values
- Any active VFS context is temporarily suspended
- After completion, the previous context is restored

### Example

```
import shell:
  - Vfs
  - host

let a = Vfs unix_socket: /tmp/container/socket

a do
  # Inside container context
  cd /tmp
  env["CONTAINER_VAR"] = "value"

  # Temporarily return to host
  host do
    # Back to host with original environment
    run.echo "Host directory: $(cd())"

  # Back in container context
  run.echo "Container directory: $(cd())"
```

Note: The `host` function is available on all platforms, but `Vfs` is only
available on Unix platforms.
