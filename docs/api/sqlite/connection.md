# Connection

Connection objects are returned by
[`open()`](./index.md#open-path-retries-min_wait-max_wait-func)
and provide methods for interacting with SQLite databases.

## Methods

### `execute sql args...`

Prepares a SQL statement, executes it, and returns the number of rows affected.
This is a convenience shorthand for [`prepare`](#prepare-sql-func) +
[`Statement.execute`](./statement.md#execute-args) when a statement doesn't
need to be reused.

**Parameters:**

| Name  | Type                   | Description                             |
| ----- | ---------------------- | --------------------------------------- |
| `sql` | [`str`](../std/str.md) | SQL statement to execute                |
| `...` | any                    | Keyword arguments for parameter binding |

**Returns:** `int` — number of rows affected

**Example:**

```
open "mydb.sqlite" do |conn|
  conn.execute
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"
  conn.execute
    "INSERT INTO users (name) VALUES (:name)" name: "Alice"
```

### `prepare sql func?`

Prepares a SQL statement for repeated execution.

**Parameters:**

| Name   | Type                   | Description                                               |
| ------ | ---------------------- | --------------------------------------------------------- |
| `sql`  | [`str`](../std/str.md) | SQL statement to prepare                                  |
| `func` | func                   | Callable to run with the statement; auto-closes when done |

**Returns:** [Statement](./statement.md) when no `func` is provided, otherwise
the result of calling `func`

**Example:**

```
open "mydb.sqlite" do |conn|
  # Using block form (auto-closes)
  conn.prepare "SELECT * FROM users WHERE id = :id" do |stmt|
    for row = stmt.query id: 1
      echo "User: $(row["name"])"

  # Manual management
  let stmt = conn.prepare "INSERT INTO users (name) VALUES (:name)"
  stmt.execute name: "Charlie"
  stmt.close()
```

### `transaction func`

Begins a database transaction and passes a [Transaction](./transaction.md)
object to the provided block.

**Parameters:**

| Name   | Type | Description                            |
| ------ | ---- | -------------------------------------- |
| `func` | func | Callable to run within the transaction |

**Returns:** The result of calling `func`

The transaction is automatically committed when `func` returns successfully and
automatically rolled back if it raises an error. Call
[`commit()`](./transaction.md#commit) or
[`rollback()`](./transaction.md#rollback) explicitly to finalize the transaction
early.

When a busy error occurs inside a transaction, the operation raises immediately
without retrying. The transaction block is then rolled back and re-invoked
until it succeeds, is explicitly rolled back, or retries are exhausted.

**Example:**

```
open "mydb.sqlite" do |conn|
  conn.transaction do |_|
    conn.execute "UPDATE accounts SET balance = balance - 100 WHERE id = 1"
    conn.execute "UPDATE accounts SET balance = balance + 100 WHERE id = 2"

  # Explicit rollback example
  conn.transaction do |tx|
    conn.execute "INSERT INTO audit (action) VALUES ('attempt')"
    if should_cancel
      tx.rollback()
```

### `close()`

Closes the database connection and releases resources. Connections not
explicitly closed are closed when garbage collected.

## Usage Notes

### Busy Retry

When an operation encounters a busy error outside of a transaction, it is
automatically retried with exponential backoff. The retry parameters are
configured when opening the connection:

| Parameter  | Default | Description                             |
| ---------- | ------- | --------------------------------------- |
| `retries`  | 10      | Maximum number of retry attempts        |
| `min_wait` | 1       | Initial wait in milliseconds            |
| `max_wait` | 1000    | Maximum wait in milliseconds (cap)      |

The wait time doubles after each attempt (plus a small random jitter) until it
reaches `max_wait`. Set `retries: 0` to disable automatic retry.

Operations within a transaction are not retried individually; instead the
entire transaction is retried. See [Transaction](./transaction.md) for details.

```
# High-contention scenario: retry up to 20 times, wait up to 5s
open "mydb.sqlite" retries: 20 max_wait: 5000 do |conn|
  conn.execute "UPDATE counters SET value = value + 1"

# Disable automatic retry
open "mydb.sqlite" retries: 0 do |conn|
  do
    conn.execute "UPDATE counters SET value = value + 1"
  catch Busy
    echo "Database is busy"
```

### Concurrency

A connection may only be used by one strand at a time. Concurrent access from
multiple strands raises a concurrency error.
