# Writer

Adds entries sequentially to a new TAR archive.

## Fields

### `compression`

Selected compression as `:NONE:`, `:GZIP:`, or `:ZSTD:`.

## Methods

### `entry path :size ... func`

Adds one entry and calls `func` with a scoped
[`EntryWriter`](./entry-writer.md).

**Parameters:**

| Name           | Type                                                      | Description                           |
| -------------- | --------------------------------------------------------- | ------------------------------------- |
| `path`         | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md)  | Entry path                            |
| `size`         | [`int`](../std/int.md)                                    | Exact content size                    |
| `type`         | [`sym`](../std/sym.md)?                                   | Entry type; default `:FILE:`          |
| `mode`         | [`int`](../std/int.md)?                                   | Permission bits; default `0o644`      |
| `uid`          | [`int`](../std/int.md)?                                   | Owner ID; default `0`                 |
| `gid`          | [`int`](../std/int.md)?                                   | Group ID; default `0`                 |
| `mtime`        | [`DateTime`](../time/datetime.md)?                        | Modification time; default Unix epoch |
| `user_name`    | [`str`](../std/str.md)?                                   | Owner name                            |
| `group_name`   | [`str`](../std/str.md)?                                   | Group name                            |
| `link_name`    | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md)? | Required for hard and symbolic links  |
| `device_major` | [`int`](../std/int.md)?                                   | Required for device entries           |
| `device_minor` | [`int`](../std/int.md)?                                   | Required for device entries           |
| `func`         | callable                                                  | Entry writer scope                    |

**Returns:** the result of `func`.

**Errors:**

- Creating another entry while an entry scope is active raises a concurrency
    error.
- Writing fewer or more bytes than `size` fails and prevents further entries.

```
archive.entry data.bin size: 4 mode: 0o600 do |entry|
  entry.write b"\x00\x01\x02\x03"
```
