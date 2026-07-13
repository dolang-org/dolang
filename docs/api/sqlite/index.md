# sqlite

The `sqlite` module provides functions and types for working with SQLite
databases.

## Functions

### `open path retries? min_wait? max_wait? func?`

Opens a SQLite database connection and returns a Connection object.

#### Parameters

| Name       | Type                                            | Description                                                |
| ---------- | ----------------------------------------------- | ---------------------------------------------------------- |
| `path`     | [`str`](../std/str.md)\|[`Path`](../fs/path.md) | Path to the database file                                  |
| `retries`  | `int`                                           | Max retry attempts on busy (default: 10)                   |
| `min_wait` | `int`                                           | Initial wait in ms between retries (default: 1)            |
| `max_wait` | `int`                                           | Max wait in ms between retries (default: 1000)             |
| `func`     | func                                            | Callable to run with the connection; auto-closes when done |

#### Returns

[Connection](./connection.md) when no `func` is provided, otherwise
the result of calling `func`

#### Example

```
# Open with default retry settings
let conn = open mydb.sqlite
conn.close()

# Open with custom retry settings
let conn2 = open mydb.sqlite retries: 10 min_wait: 5 max_wait: 5000
conn2.close()

# Using block form (auto-closes)
open mydb.sqlite do |conn|
  conn.execute
    "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)"
```

## Types

- [Error](./error.md) - Base error type for SQLite errors
- [Busy](./busy.md) - Error type for database busy/locked conditions (subtype of
  Error)

## Examples

### Basic execution

```
open "mydb.sqlite" do |conn|
  conn.execute
    "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)"
  conn.execute
    "INSERT INTO users (name) VALUES (:name)" name: "Alice"
  conn.execute
    "INSERT INTO users (name) VALUES (:name)" name: "Bob"
```

### Prepared statements

```
open "mydb.sqlite" do |conn|
  conn.prepare "SELECT * FROM users WHERE name = :name" do |stmt|
    for row = stmt.query name: "Alice"
      echo "Found: $(row["name"])"
```

### Transactions

```
open "mydb.sqlite" do |conn|
  conn.transaction do |_|
    conn.execute
      "UPDATE accounts SET balance = balance - 100 WHERE id = 1"
    conn.execute
      "UPDATE accounts SET balance = balance + 100 WHERE id = 2"
```
