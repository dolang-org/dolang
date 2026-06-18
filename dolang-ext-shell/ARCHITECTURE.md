# dolang-ext-shell Architecture

The `dolang-ext-shell` crate provides shell-like functionality for Do: process
execution, I/O redirection, environment management, and filesystem operations.

## Process Execution

`Program` objects are instantiated by a `ProgramFactory` singleton registered
as the `shell.program` module when fields are accessed. When called, they
resolve an executable in `PATH` and spawn it with environment variables and
working directory taken from the strand. Objects from the strand-local input
iterator are converted to strings and pumped into the process stdin. Output
from the process is parsed at line boundaries and fed into the strand-local
output iterator as individual strings with line endings removed. Non-zero exit
codes are treated as errors.

### Container Transparency

On Unix, process spawning has two paths selected at runtime by
`dispatch_run()` in `program.rs`:

- **Local path** (`run`): No container context. Creates a
  `tokio::process::Command` directly and spawns it on the host.
- **Vfs path** (`run_container`): A container context is present in
  strand-local state. Creates a `dolang_shell_vfs::CommandBuilder` via the
  VFS client and spawns the process inside the container.

Both paths implement the same abstract `CommandBuilder` trait (program name,
env, cwd, FD redirection), so the rest of the execution pipeline (stdin pump,
stdout line splitting, exit code handling) is shared.

The container context (`Context` in `shell.rs`) holds a shell VFS `Client`,
container-local cwd, and a scoped `Env`. It is stored in `Local` and accessed
via `local.container()`. `container::host()` temporarily clears the context to
force local execution for a closure. The `shell.Vfs()` constructor connects to
the shell VFS Unix socket and queries its environment/cwd.

## Pipe Channels (Unix)

Pipe channels connect Do strand I/O iterators to Unix pipes for process
stdin/stdout, allowing lazy negotiation between value-mode (buffered Do objects)
and pipe-mode (raw bytes over a file descriptor).

### State Machine (`PipeState`)

The shared state (`PipeChannelShared`) tracks the current mode:

| State      | Send side             | Recv side             |
| ---------- | --------------------- | --------------------- |
| `Value`    | Buffered Do values    | Buffered Do values    |
| `RecvPipe` | Buffered Do values    | Reading from pipe FD  |
| `SendPipe` | Writing to pipe FD    | Buffered Do values    |
| `Direct`   | Writing to pipe FD    | Reading from pipe FD  |
| `Draining` | Buffered Do values    | Buffered Do values    |

Transitions are triggered by negotiation (a side requesting a pipe FD) or
completion (a side finishing with its pipe FD):

- `negotiate_recv()`: polls until a transition to `RecvPipe` (or `Direct` if
  send already has a pipe) is possible. Returns an RAII `RecvGuard`.
- `negotiate_send()`: symmetric; transitions to `SendPipe` or `Direct`.
  Returns a `SendGuard`.
- `recv_done()`: recv releases its pipe. `RecvPipe → Draining` (if send absent)
  or `Direct → SendPipe`.
- `send_done()`: send releases its pipe. `SendPipe → Draining` or
  `Direct → RecvPipe`.

When transitioning from value mode to pipe mode, any buffered value is encoded
to bytes and written into the pipe (`take_buffered_bytes()`), preserving
ordering.

### FD Lifecycle (`EndState<T>`)

Each pipe end (sender/receiver) has a three-state lifecycle:

- `Absent` — no FD allocated yet
- `Present(T)` — FD available for use
- `Taken` — FD exists but is checked out for I/O without holding a borrow

RAII guards (`SendEndGuard`, `RecvEndGuard`) move the end to `Taken` while
performing I/O and restore it to `Present` on drop (unless the channel was
closed in the meantime, in which case the FD is dropped and waiters are woken).

### Async Coordination

Waker queues (`send_wakers`, `recv_wakers`, `negotiate_wakers`) allow futures
to sleep until a state transition makes progress possible. Explicit
`send_closed`/`recv_closed` flags allow early termination.

`PipeReceiver` and `PipeSender` are GC object types (`Object<'v>`) that
implement the Do iterator/sink protocols. They hold `Rc<RefCell<...>>` to the
shared state.

Windows: pipe channels are stubbed out (not supported).

## Environment Management

The `Env` object provides dictionary-like access to strand-local environment
variables with hierarchical fallback to the process environment. It also allows
introducing scoped overrides by calling it with a closure.

## Directory Operations

`cd` changes the strand-local current working directory for the duration
of the passed closure.
