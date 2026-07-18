# dolang-shell-vfs Architecture

`dolang-shell-vfs` is a virtual filesystem and process-spawning layer for the
shell runtime.

It has two backends:

- Direct filesystem access and process spawning
- An RPC client which forwards to a server running the direct backend.

An RPC client may carry an opaque VFS selector. The selector names an `AnyVfs`
retained by the outer session, allowing the same request protocol and handlers
to operate through another remote backend. Every request carries the optional
selector; retained files, stdio ends, and children are associated with that
VFS domain.

Opening a Unix-socket VFS through a native direct session transfers the
connected socket back to the caller, avoiding request forwarding. When the
outer transport cannot transfer handles, the server retains the connected
client and returns an opaque VFS selector instead. Stopping a selected VFS
stops and releases only that backend. Outer-session teardown drops retained
clients without stopping their independent daemons.

The Unix socket VFS normally exchanges raw file descriptors with `SCM_RIGHTS`.
An opaque-only client instead asks the server to retain regular files and
performs byte I/O, seeking, flushing, and truncation through typed opaque RPC
identities. Generic byte-stream sessions use the same retained-file path.

Open requests prefer native handles on attachment-capable sessions unless the
client explicitly requests an opaque file. A server which cannot transfer
handles falls back to an opaque file even for a native-preferred request.
Cursor-affecting operations on each retained file are serialized by the server.
Explicit close removes the resource and consumes the file when no operation is
racing; otherwise close reports that the resource is busy and the outstanding
guard performs final drop cleanup. Connection teardown drops all resources
which remain in the RPC object table.

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

The initial VFS query returns a snapshot of the target environment, working
directory, operating system, architecture, logical CPU count, and Wine status.
Operating systems and architectures are closed enums covering the project's
supported ports. The shell stores the target snapshot in strand-local context,
so system information follows nested VFS contexts rather than the interpreter
host.
