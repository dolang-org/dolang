# container

The `container` module provides container support on Linux. It allows running
commands inside containers from Do scripts on the host.

## Functions

### `host func ...args`

Executes a callable in a fresh host context, regardless of the current context.
This provides a way to escape from agent contexts back to the host environment
for specific operations.

| Name   | Type | Description                            |
| ------ | ---- | -------------------------------------- |
| `func` | func | Block to execute in fresh host context |
| `args` |      | Additional arguments to pass to `func` |

**Returns:** Return value of the executed callable

Note that environment variables and working directory are reset to the current
process-level values. For `dolang-shell` these will be the original values at
process start time.

**Example:**

```
# Execute in fresh host context
host do
  program.echo "Running with original host environment: $(cd())"
```

## Types

### `Vfs`

The `Vfs` class connects to a running `dolang-shell-vfs` daemon via its Unix
socket. Use the `unix_socket` key argument to specify the socket path.

**Constructor:** `Vfs unix_socket: path`

| Name          | Type                   | Description                     |
| ------------- | ---------------------- | ------------------------------- |
| `unix_socket` | [`str`](../std/str.md) | Path to the agent's Unix socket |

**Returns:** Vfs object

```
let a = Vfs unix_socket: /tmp/agent/socket
```

See the [Vfs documentation](./vfs.md) for details on using Vfs objects.

**Platform Note:** The `Vfs` class is only available on Unix platforms
(Linux, macOS, etc.). On Windows, the `Vfs` class is not available.
