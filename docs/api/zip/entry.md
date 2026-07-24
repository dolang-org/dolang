# Entry

A single archive entry's metadata, with a method to open it for reading.

Entries are only available for archives opened in read mode, via
[`Archive.entries`](./archive.md#entries).

## Fields

### `name`

Entry name as a [`UnixPath`](../fs/unix-path.md).

### `type`

Entry type as `:FILE:`, `:DIR:`, `:SYMLINK:`, `:FIFO:`, `:CHAR_DEVICE:`,
`:BLOCK_DEVICE:`, or `:UNKNOWN:`.

### `size`

Uncompressed size in bytes.

### `compressed_size`

Compressed size in bytes.

### `crc32`

CRC-32 checksum of the uncompressed data.

### `compression`

Compression method as `:STORED:`, `:DEFLATE:`, `:ZSTD:`, or `:UNKNOWN:`.

### `mode`

Unix permission bits, or `nil` when the archive does not provide Unix
metadata.

### `comment`

Entry comment.

### `last_modified`

Last modification time as [`DateTime`](../time/datetime.md), or `nil` when
the stored timestamp cannot be represented.

## Methods

### `open block?`

Opens the entry for reading.

**Parameters:**

| Name    | Type | Description                                          |
| ------- | ---- | ---------------------------------------------------- |
| `block` | func | Callable to run with the file; auto-closes when done |

**Returns:** [`File`](./file.md) when no block is provided, otherwise the
result of calling `block`.

**Errors:**

- Raises a concurrency error if another file is already open in the archive.

```
for entry = archive.entries
  entry.open do |file|
    echo (str (file.read entry.size))
```
