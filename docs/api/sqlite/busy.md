# Busy

`Busy` is raised when a SQLite operation fails because the database is locked
by another connection or process.

By default, busy errors outside of transactions are retried automatically — see
[Connection](./connection.md#busy-retry) for configuration. Within
transactions, the entire transaction block is retried — see
[Transaction](./transaction.md#automatic-retry) for details.

## Inherits

- [`Error`](./error.md)
