# Entry

Exposes metadata and streaming content for the current TAR entry.

The handle remains valid only until its parent [`Reader`](./tar-reader.md)
advances or leaves scope.

## Fields

### `path`

Entry path as a [`UnixPath`](../fs/unix-path.md).

### `type`

Entry type as `:FILE:`, `:DIR:`, `:HARDLINK:`, `:SYMLINK:`, `:FIFO:`,
`:CHAR_DEVICE:`, `:BLOCK_DEVICE:`, `:CONTIGUOUS:`, or `:UNKNOWN:`.

### `size`

Declared content size in bytes.

### `mode`

Unix permission bits.

### `uid`

Numeric owner ID.

### `gid`

Numeric group ID.

### `mtime`

Modification time as [`DateTime`](../time/datetime.md).

### `user_name`

Owner name, or `nil` when absent.

### `group_name`

Group name, or `nil` when absent.

### `link_name`

Link target as a [`UnixPath`](../fs/unix-path.md), or `nil` when absent.

### `device_major`

Device major number, or `nil` when absent.

### `device_minor`

Device minor number, or `nil` when absent.

## Methods

### `read size`

Reads up to `size` bytes from the current content position.

**Parameters:**

| Name   | Type                    | Description             |
| ------ | ----------------------- | ----------------------- |
| `size` | [`int`](../std/int.md)  | Maximum bytes to read   |

**Returns:** [`bin`](../std/bin.md).

```
let prefix = entry.read 512
```

## Operators

Iteration yields arbitrary-sized binary chunks until the entry is exhausted.

```
for chunk = entry
  output.put $chunk
```
