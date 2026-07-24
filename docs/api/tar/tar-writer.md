# Writer

Adds entries sequentially to a new TAR archive.

## Fields

### `compression`

Selected compression as `:NONE:`, `:GZIP:`, or `:ZSTD:`.

## Methods

### `entry path :size ... func`

Adds one regular-file entry and calls `func` with a scoped
[`EntryWriter`](./entry-writer.md). For directory, symlink, or hard-link
entries, use
[`create_dir`](#create_dir-path-mode-uid-gid-mtime-user_name-group_name),
[`symlink`](#symlink-target-path-mode-uid-gid-mtime-user_name-group_name), or
[`hard_link`](#hard_link-target-path-mode-uid-gid-mtime-user_name-group_name)
instead — none of them carry content, so none of them have any use for a write
handle.

**Parameters:**

| Name         | Type                                                     | Description                           |
| ------------ | -------------------------------------------------------- | ------------------------------------- |
| `path`       | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md) | Entry path                            |
| `size`       | [`int`](../std/int.md)                                   | Exact content size                    |
| `mode`       | [`int`](../std/int.md)?                                  | Permission bits; default `0o644`      |
| `uid`        | [`int`](../std/int.md)?                                  | Owner ID; default `0`                 |
| `gid`        | [`int`](../std/int.md)?                                  | Group ID; default `0`                 |
| `mtime`      | [`DateTime`](../time/datetime.md)?                       | Modification time; default Unix epoch |
| `user_name`  | [`str`](../std/str.md)?                                  | Owner name                            |
| `group_name` | [`str`](../std/str.md)?                                  | Group name                            |
| `func`       | callable                                                 | Entry writer scope                    |

**Returns:** the result of `func`.

**Errors:**

- Creating another entry while an entry scope is active raises a concurrency
    error.
- Writing fewer or more bytes than `size` fails and prevents further entries.

```
archive.entry data.bin size: 4 mode: 0o600 do |entry|
  entry.write b"\x00\x01\x02\x03"
```

### `create_dir path :mode? :uid? :gid? :mtime? :user_name? :group_name?`

Creates a directory entry.

**Parameters:**

| Name         | Type                                                     | Description                           |
| ------------ | -------------------------------------------------------- | ------------------------------------- |
| `path`       | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md) | Entry path                            |
| `mode`       | [`int`](../std/int.md)?                                  | Permission bits; default `0o755`      |
| `uid`        | [`int`](../std/int.md)?                                  | Owner ID; default `0`                 |
| `gid`        | [`int`](../std/int.md)?                                  | Group ID; default `0`                 |
| `mtime`      | [`DateTime`](../time/datetime.md)?                       | Modification time; default Unix epoch |
| `user_name`  | [`str`](../std/str.md)?                                  | Owner name                            |
| `group_name` | [`str`](../std/str.md)?                                  | Group name                            |

```
archive.create_dir "subdir" mode: 0o755
```

### `symlink target path :mode? :uid? :gid? :mtime? :user_name? :group_name?`

Creates a symbolic link entry pointing to `target`. Argument order matches
[`fs.symlink_file`](../fs/index.md#symlink_file-src-dst).

**Parameters:**

| Name         | Type                                                     | Description                           |
| ------------ | -------------------------------------------------------- | ------------------------------------- |
| `target`     | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md) | Path the symlink points to            |
| `path`       | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md) | Entry path                            |
| `mode`       | [`int`](../std/int.md)?                                  | Permission bits; default `0o777`      |
| `uid`        | [`int`](../std/int.md)?                                  | Owner ID; default `0`                 |
| `gid`        | [`int`](../std/int.md)?                                  | Group ID; default `0`                 |
| `mtime`      | [`DateTime`](../time/datetime.md)?                       | Modification time; default Unix epoch |
| `user_name`  | [`str`](../std/str.md)?                                  | Owner name                            |
| `group_name` | [`str`](../std/str.md)?                                  | Group name                            |

```
archive.symlink "target.txt" "link.txt"
```

### `hard_link target path :mode? :uid? :gid? :mtime? :user_name? :group_name?`

Creates a hard-link entry pointing to `target`. Argument order matches
[`fs.hard_link`](../fs/index.md#hard_link-src-dst).

**Parameters:**

| Name         | Type                                                     | Description                           |
| ------------ | -------------------------------------------------------- | ------------------------------------- |
| `target`     | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md) | Path the hard link points to          |
| `path`       | [`str`](../std/str.md)\|[`UnixPath`](../fs/unix-path.md) | Entry path                            |
| `mode`       | [`int`](../std/int.md)?                                  | Permission bits; default `0o644`      |
| `uid`        | [`int`](../std/int.md)?                                  | Owner ID; default `0`                 |
| `gid`        | [`int`](../std/int.md)?                                  | Group ID; default `0`                 |
| `mtime`      | [`DateTime`](../time/datetime.md)?                       | Modification time; default Unix epoch |
| `user_name`  | [`str`](../std/str.md)?                                  | Owner name                            |
| `group_name` | [`str`](../std/str.md)?                                  | Group name                            |

```
archive.hard_link "file.txt" "link.txt"
```
