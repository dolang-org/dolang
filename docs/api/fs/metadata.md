# Metadata

Filesystem metadata object returned by
[`metadata`](./index.md),
[`Path.metadata()`](./path.md), and
[`File.metadata()`](./file.md).

## Fields

### `len`

File size in bytes.

### `type`

File type as a [`sym`](../std/sym.md): `:file:`, `:dir:`, `:symlink:`,
`:fifo:`, `:char_device:`, `:block_device:`, `:socket:`, or `:unknown:`.

### `modified`

Last modification time as [`DateTime`](../time/datetime.md).

### `accessed`

Last access time as [`DateTime`](../time/datetime.md).

### `created`

Creation or status-change time as [`DateTime`](../time/datetime.md).

## Windows-Only Fields

### `attributes`

Raw Windows file attribute bitmask.

### `readonly`

Whether the readonly attribute bit is set.

### `hidden`

Whether the hidden attribute bit is set.

### `system`

Whether the system attribute bit is set.

### `archive`

Whether the archive attribute bit is set.

### `reparse_point`

Whether the reparse-point attribute bit is set.

### `compressed`

Whether the compressed attribute bit is set.

### `encrypted`

Whether the encrypted attribute bit is set.

### `temporary`

Whether the temporary attribute bit is set.

### `offline`

Whether the offline attribute bit is set.

### `not_content_indexed`

Whether the not-content-indexed attribute bit is set.

## Unix-Only Fields

### `mode`

Stat mode bits.

### `dev`

Device ID.

### `ino`

Inode number.

### `nlink`

Hard-link count.

### `uid`

Owner user ID.

### `gid`

Owner group ID.

### `rdev`

Special-device ID.

### `blksize`

Preferred block size for I/O.

### `blocks`

Number of allocated 512-byte blocks.
