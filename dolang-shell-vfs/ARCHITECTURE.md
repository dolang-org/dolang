# dolang-shell-vfs Architecture

`dolang-shell-vfs` is a virtual filesystem and process-spawning layer for the
shell runtime.

It has two backends:

- Direct filesystem access and process spawning
- An RPC client which forwards to a server running the direct backend.

The Unix socket VFS exchanges raw file descriptors with `SCM_RIGHTS` rather
than virtualizing all I/O.

On Windows, the RPC client uses the server end of a connected named pipe and
the RPC server uses the client end. The client retains a trusted handle for the
server process so `dolang-rpc` can transfer native handles in both directions.
`shell.Vfs.windows_admin()` creates the pipe and launches the current
`dolang.exe` through UAC. The child handles the private `--vfs` mode and serves
the direct backend until shutdown or disconnect.

For automated tests, `shell.Vfs.windows_admin(elevate: false)` uses a normal
same-user process launch instead. Windows release validation must also accept a
real UAC prompt, perform an operation requiring administrator access, call
`stop()`, and confirm that the child exits. Cancelling the prompt must return
an error without leaving a child process.

An elevated Windows VFS process cannot reliably use console handles inherited
from its non-elevated parent. Programs which require console input or output
may therefore hang, fail, or produce no output when run through an elevated
VFS session. The VFS still duplicates standard handles because doing so is
harmless and remains useful for non-console handles. In particular, redirected
and captured output works because it uses ordinary handles rather than the
parent console.

Path-based operations execute on the RPC server. Operations whose names begin
with `file_` act locally on a file handle that the server already transferred
to the client.

VFS operations return the crate's `Error` type. Errors without a raw system
code retain their original `io::Error` locally. System errors carry the raw
code, originating operating system, `ErrorKind`, and formatted message across
RPC. A client must not interpret a foreign raw code using the host platform's
error tables.
