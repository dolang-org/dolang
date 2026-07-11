# Rows

`Rows` objects are returned by [`Statement.query()`](./statement.md#query-args)
and act as an iterator of [`Row`](./row.md) objects. A `Rows` iterator is also
yielded by a `...rest` term when unpacking a `Row`, in which case it iterates
the remaining unconsumed columns of that row as values.

## Inherits

- [`Iter`](../std/iter.md)

## Usage Notes

### Invalidation

Only the most recently returned `Row` is valid; attempting to use previously
returned rows after advancing the iterator will raise concurrency errors.
Reusing or closing the statement or closing the connection invalidates `Rows`
and any returned `Row`; further accesses will result in concurrency errors. As
an exception, spreading (e.g. `[...rows]`) will return `Row` objects which own
copies of all column values, avoiding invalidation on iterator advancement.
However, invalidation of the statement will still result in concurrency errors.

### Automatic Retry

When iterating over rows outside of a transaction, busy errors encountered
while fetching the next row are automatically retried according to the
connection's retry configuration.
