# dolang-shell-vfs Architecture

IPC client/server for spawning processes and accessing the filesystem with FD
pass-through on Unix. A long-lived daemon performs operations on behalf of
connected clients, primarily to support operating inside containers from a host
dolang-shell instance.
