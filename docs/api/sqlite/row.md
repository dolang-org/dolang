# Row

`Row` objects are returned by iterating [`Rows`](./rows.md) and provide access
to the column values of a single query result row.

## Access

Rows support index access by position (`row[0]`) or column name (`row["name"]`).

Rows can be unpacked by position or by symbol name (matching the column name):

```
let id :name :age ...rest = row
```

A `...rest` term yields a [`Rows`](./rows.md) iterator over the remaining
columns.

## Column Types

Column values are automatically converted from SQLite types to Do types:

| SQLite Type                | Do Type | Notes                                                        |
| -------------------------- | ------- | ------------------------------------------------------------ |
| NULL                       | `nil`   |                                                              |
| INTEGER (declared BOOLEAN) | `bool`  | Declared type must be `BOOLEAN` or `BOOL` (case-insensitive) |
| INTEGER                    | `int`   |                                                              |
| REAL                       | `float` |                                                              |
| TEXT                       | `str`   |                                                              |
| BLOB                       | `bin`   |                                                              |
