# Transaction

Transaction objects are passed to the block in
[`Connection.transaction()`](./connection.md#transaction-func) and provide
methods for explicitly committing or rolling back.

## Methods

### `commit()`

Commits the transaction, making all changes permanent.

#### Example

```
open "mydb.sqlite" do |conn|
  conn.transaction do |tx|
    conn.execute "UPDATE accounts SET balance = balance - 100 WHERE id = 1"
    conn.execute "UPDATE accounts SET balance = balance + 100 WHERE id = 2"
    tx.commit()
```

### `rollback()`

Rolls back the transaction, discarding all changes.

#### Example

```
open "mydb.sqlite" do |conn|
  conn.transaction do |tx|
    conn.execute "INSERT INTO audit (action) VALUES ('attempt')"
    if should_cancel
      tx.rollback()
```

## Usage Notes

### Automatic Commit and Rollback

If the block exits without calling `commit()` or `rollback()`, the transaction
is automatically committed on success or rolled back on error.

### Automatic Retry

When a busy error occurs inside a transaction, the failing operation raises
immediately without retrying. The transaction is then rolled back and the
entire block re-invoked until it succeeds, is explicitly rolled back, or the
connection's retry limit is exhausted. See
[Connection](./connection.md#busy-retry) for retry configuration.
