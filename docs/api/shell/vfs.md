# Vfs

`Vfs` connects to a shell VFS daemon and can run code in that VFS context.

## Creating a Vfs

Create a `Vfs` by connecting to a running `dolang-shell-vfs` daemon:

```
let a = Vfs unix_socket: /tmp/agent/socket
```

## Calling a Vfs

Call the `Vfs` with a block to execute code in that VFS context:

```
a do
  # Commands here use the VFS context
  # env and cd also use that context
  run.ls /
  run.cat /etc/os-release
```

Entering a `Vfs` context affects the operations that are routed through it:

- External programs run through the VFS daemon
- [`shell.env`](../shell/index.md#env) reads and writes that context's
  environment
- [`shell.cd`](../shell/index.md#cd-path-func) changes that context's working
  directory
- [`fs`](../fs/index.md) operations use that context's filesystem view

This is commonly used for containers, but it is not limited to them.

## Methods

### `stop()`

Signals the connected VFS daemon to stop accepting new connections and shut
down.

```
a.stop()
```

See the [Container Support](../../shell/containers.md) guide for setup
instructions.

**Platform Note:** The `Vfs` class is only available on Unix platforms
(Linux, macOS, etc.).
