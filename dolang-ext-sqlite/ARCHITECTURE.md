# dolang-ext-sqlite Architecture

The `dolang-ext-sqlite` crate provides SQLite database access for Do programs.
The main challenges are bridging SQLite's blocking C API with the async strand
runtime, and supporting file I/O through the shell VFS on Unix.

## Object Model

Four object types are exposed to Do programs:

- **Connection** - Database connection. Methods: `prepare`, `query`, `execute`,
  `close`, `transaction`. Has a strand-level `in_use` flag that prevents
  concurrent access from multiple strands.
- **Statement** - Prepared SQL statement. Methods: `query`, `execute`, `close`.
  Supports named parameters (`:name` syntax) bound from a Do keyword-argument
  dict.
- **Rows** - Lazy iterator over result rows. Invalidated if its statement is
  reused.
- **Row** - Single result row. Fields are accessible by name or integer index.
  Row data is either a cheap reference into statement memory or a heap copy,
  depending on how the row is being consumed (see Epoch Counters below).
- **Transaction** - Handle returned by `connection.transaction()`. Commits or
  rolls back when the enclosing block returns.

## Async Bridging

SQLite operations block the thread. To avoid stalling the tokio executor, all
SQLite calls are dispatched via `tokio::task::spawn_blocking()`. `Connection`
methods are `async fn`s that await these blocking tasks, so they compose
naturally with Do's strand concurrency.

## Busy Retries

`SQLITE_BUSY` and `SQLITE_LOCKED` errors trigger automatic retries with
exponential backoff and jitter. The retry parameters (`retries`, `min_wait`,
`max_wait`) are configurable per connection at `sqlite.open()` time. Retries
are skipped inside transactions — once SQLITE_BUSY is returned mid-transaction,
the whole transaction must be restarted.

`connection.transaction()` handles this by wrapping the entire transaction block
in its own retry loop: if SQLITE_BUSY escapes from a commit, the block runs
again from the beginning.

## Epoch Counters

Stale handle detection uses integer epochs counters:

- **Connection epoch** — incremented at each transaction boundary. `Transaction`
  objects capture the epoch when created and fail if the epoch has changed (the
  connection was recycled under them).
- **Statement epoch** — incremented on each `query`/`execute`. `Rows` objects
  validate against this to detect statement reuse.
- **Row epoch** — incremented on each `Rows::next()`. `RowIter` (produced by
  partial unpack) validates against this to detect iterator advancement.

`Row` data defaults to a zero-copy reference into statement memory. If the row
will outlive the current `Rows::next()` call (e.g. for a `...rest` unpack that
hands data to a closure), the data is copied to an owned heap buffer instead.

## Deferred Cleanup

`sqlite3_close()` and `sqlite3_finalize()` block. Rather than call them during
GC, `Drop` impls spawn background `spawn_blocking` tasks to finalize resources.
A `pending_close` flag on `Connection` allows marking a connection for closing
while it is in use.

## Custom VFS (Unix)

On Unix, the extension registers a custom SQLite VFS named `"dolang-shell"`.
It is used when a shell VFS connection is available, such as when Do is running
on the host and needs to operate on a database inside a container.

The VFS forwards all file I/O through the shell VFS helper. SQLite's POSIX
locking protocol is replicated faithfully:

- **InodeState** — per-inode lock tracking keyed by device+inode. Necessary
  because closing any file descriptor to an inode releases all `fcntl` locks
  for that process (POSIX semantics), so lock acquisition and release must be
  coordinated across all open handles to the same file.
- **ShmNode / ShmConn** — WAL shared-memory coordination. `ShmNode` holds the
  mmap'd `-shm` file regions and the dead-man-switch (DMS) lock for WAL
  initialization. `ShmConn` is a per-connection view with per-slot lock masks.

`with_shell()` sets a thread-local client pointer before entering a VFS
operation so that async shell VFS calls can be driven synchronously from the
VFS callbacks via `block_on_shell()`.
