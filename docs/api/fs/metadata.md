# Metadata

Filesystem metadata object returned by
[`metadata`](./index.md),
[`Path.metadata()`](./path.md), and
[`File.metadata()`](./file.md).

For filesystem-level capacity and mount metadata, use
[`FsMetadata`](./fs-metadata.md).

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

### `win_attrs`

Raw Windows file attribute bitmask.

## Windows/macOS Fields

### `attrs`

[`Attrs`](./attrs.md) object built from this metadata snapshot.

## macOS-Only Fields

### `unix_flags`

Raw macOS file flags.

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
