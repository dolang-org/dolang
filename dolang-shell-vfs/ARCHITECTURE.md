# dolang-shell-vfs Architecture

`dolang-shell-vfs` is a virtual filesystem and process-spawning layer for the
shell runtime.

It has two backends:

- Direct filesystem access and process spawning
- A Unix socket client which forwards to a server running the direct backend.

The Unix socket VFS exchanges raw file descriptors with `SCM_RIGHTS` rather
than virtualizing all I/O.
