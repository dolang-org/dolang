# Reader

Destructively iterates the entries in an open TAR archive.

Advancing the reader drains unread data from the current entry and invalidates
its [`Entry`](./tar-entry.md) handle.

## Fields

### `compression`

Detected compression as `:NONE:`, `:GZIP:`, or `:ZSTD:`.

## Operators

Iteration yields [`Entry`](./tar-entry.md) objects in archive order.

```
read "archive.tar" do |archive|
  for entry = archive
    echo $entry.path
```
