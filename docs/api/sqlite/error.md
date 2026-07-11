# Error

`Error` is raised when a SQLite operation fails for any reason other
than a busy/locked condition. [`Busy`](./busy.md) is a subtype of
`Error`, so catching `Error` handles all SQLite errors.

```
open "mydb.sqlite" do |conn|
  do
    conn.execute "INSERT INTO users (name) VALUES (:name)" name: "Alice"
  catch Busy
    echo "Database is busy"
  catch Error: e
    echo "SQLite error: $e"
```

## Inherits

- [`RuntimeError`](../std/runtime-error.md)
