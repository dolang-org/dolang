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
Creating the named pipe and launching or elevating that process are the
caller's responsibility.

Path-based operations execute on the RPC server. Operations whose names begin
with `file_` act locally on a file handle that the server already transferred
to the client.
