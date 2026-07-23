# FileLock

Reports the state of a scoped [`File`](./file.md) lock.

## Fields

### `held`

Whether the lock is currently held.

The value is false when `try_lock` does not acquire the lock and after
releasing it.

## Methods

### `release()`

Releases the lock before the surrounding scope exits.

Repeated calls have no effect. Release runs with interruption masked.
