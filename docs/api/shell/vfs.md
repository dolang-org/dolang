# Vfs

Vfs objects are created by the `Vfs` constructor and provide methods for
container interaction.

## Creating a Vfs

Create a Vfs by connecting to a running `dolang-shell-vfs` daemon:

```
let a = Vfs unix_socket: /tmp/agent/socket
```

## Calling a Vfs

Call the Vfs with a block to execute commands inside the container:

```
a do
  # Commands here run inside the container
  # env and cd also operate in the container context
  run.ls /
  run.cat /etc/os-release
```

Entering a Vfs context affects all system interactions:

- External programs run inside the container
- [`shell.env`](../shell/index.md#env) reads and writes the container's
  environment
- [`shell.cd`](../shell/index.md#cd-path-func) changes the container's working
  directory
- [`fs`](../fs/index.md) operations (open, remove, read, write) access the
  container's filesystem

## Methods

### `stop()`

Signals the VFS daemon to stop accepting new connections and shut down.

```
a.stop()
```

See the [Container Support](../../shell/containers.md) guide for setup
instructions.

**Platform Note:** The `Vfs` class is only available on Unix platforms
(Linux, macOS, etc.).
