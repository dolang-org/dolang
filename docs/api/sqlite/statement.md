# Statement

Statement objects are returned by
[`Connection.prepare()`](./connection.md#prepare-sql-func) and represent
compiled SQL statements that can be executed multiple times with different
parameters.

## Methods

### `query args...`

Executes the statement and returns a Rows iterator for reading results.

**Parameters:**

| Name  | Type | Description                                           |
| ----- | ---- | ----------------------------------------------------- |
| `...` | any  | Keyword arguments for parameter binding               |

**Returns:** [Rows](./rows.md)

**Example:**

```
open "mydb.sqlite" do |conn|
  conn.prepare "SELECT * FROM users WHERE age > :min_age" do |stmt|
    for row = stmt.query min_age: 18
      echo "$(row["name"]) is $(row["age"]) years old"

    let count = 0
    for row = stmt.query min_age: 21
      count += 1
    echo "Found $(count) adults"
```

### `execute args...`

Executes the statement and returns the number of rows affected.

**Parameters:**

| Name  | Type | Description                                           |
| ----- | ---- | ----------------------------------------------------- |
| `...` | any  | Keyword arguments for parameter binding               |

**Returns:** `int` - Number of rows affected

**Example:**

```
open "mydb.sqlite" do |conn|
  conn.prepare "UPDATE users SET status = :status WHERE created < :date" do |stmt|
    let affected = stmt.execute status: "archived" date: "2023-01-01"
    echo "Archived $(affected) users"
```

### `close()`

Closes the statement and releases resources.

## Usage Notes

### Parameter Binding

Statements support named parameters using the `:name` syntax in SQL. Parameters
are bound by passing keyword arguments to `query()` or `execute()`.

**Supported parameter types:**

| Type    | SQLite Type  | Example                                  |
| ------- | ------------ | ---------------------------------------- |
| `nil`   | NULL         | `stmt.execute value: nil`                |
| `bool`  | INTEGER      | `stmt.execute active: true`              |
| `int`   | INTEGER      | `stmt.execute id: 42`                    |
| `float` | REAL         | `stmt.execute price: 19.99`              |
| `str`   | TEXT         | `stmt.execute name: "Alice"`             |
| `bin`   | BLOB         | `stmt.execute data: b"\x01\x02\x03"`     |

`bool` values are stored as `0` or `1`.

**Example:**

```
open "mydb.sqlite" do |conn|
  conn.prepare
    "INSERT INTO users (name, age, active) VALUES (:name, :age, :active)"
    do |stmt|
      stmt.execute name: "Alice" age: 30 active: true
      stmt.execute name: "Bob" age: 25 active: false
```

### Automatic Retry

When a statement is executed outside of a transaction, busy errors are
automatically retried according to the connection's retry configuration.

### Concurrent Use

Only one query can be active on a statement at a time. Starting a new query or
executing another statement invalidates the previous query iterator, which will
subsequently raise a concurrency error on use.

```
open "mydb.sqlite" do |conn|
  conn.prepare "SELECT * FROM users" do |stmt|
    let rows = stmt.query()
    let rows2 = stmt.query()
    # rows has been invalidated at this point
```
