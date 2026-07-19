# Metadata

Filesystem metadata object returned by [`metadata`](./index.md),
[`Path.metadata()`](./path.md), and [`File.metadata()`](./file.md).

For filesystem-level capacity and mount metadata, use
[`FsMetadata`](./fs-metadata.md).

Accessing a field that does not apply to the target platform raises a field
error. `linux_attrs`, `macos_attrs`, and related attribute fields are `nil`
when the filesystem or file type does not support querying file attributes.

## Fields

### `len`

File size in bytes.

### `type`

File type as a [`sym`](../std/sym.md): `:FILE:`, `:DIR:`, `:SYMLINK:`,
`:FIFO:`, `:CHAR_DEVICE:`, `:BLOCK_DEVICE:`, `:SOCKET:`, or `:UNKNOWN:`.

### `modified`

Last modification time as [`DateTime`](../time/datetime.md).

### `accessed`

Last access time as [`DateTime`](../time/datetime.md).

### `created`

Creation or status-change time as [`DateTime`](../time/datetime.md).

## Windows-Only Fields

### `user`

Owner [`Sid`](../security/windows/sid.md).

### `group`

Primary group [`Sid`](../security/windows/sid.md).

### `win_attrs`

Raw Windows file attribute bitmask.

### `readonly`

Whether the readonly attribute is set.

### `system`

Whether the system attribute is set.

### `archive`

Whether the archive attribute is set.

### `reparse_point`

Whether the reparse-point attribute is set.

### `encrypted`

Whether the encrypted attribute is set.

### `temporary`

Whether the temporary attribute is set.

### `offline`

Whether the offline attribute is set.

### `not_content_indexed`

Whether the not-content-indexed attribute is set.

## Linux-Only Fields

### `linux_attrs`

Raw Linux attribute flags.

### `no_atime`

Whether the no-atime flag is set.

### `no_copy_on_write`

Whether the no-copy-on-write flag is set.

### `dir_sync`

Whether the synchronous-directory-updates flag is set.

### `casefold`

Whether the case-insensitive-directory-lookups flag is set.

### `data_journaling`

Whether the data-journaling flag is set.

### `no_compress`

Whether the don't-compress flag is set.

### `project_inherit`

Whether the project-hierarchy flag is set.

### `secure_delete`

Whether the secure-deletion flag is set.

### `sync`

Whether the synchronous-updates flag is set.

### `no_tail_merge`

Whether the no-tail-merging flag is set.

### `top_dir`

Whether the top-of-directory-hierarchy flag is set.

### `undelete`

Whether the undeletable flag is set.

### `direct_access`

Whether the direct-access flag is set.

### `extent_format`

Whether the extent-format flag is set.

## macOS-Only Fields

### `macos_attrs`

Raw macOS file flags.

### `opaque`

Whether the opaque flag is set.

## Platform Attribute Fields

### `hidden`

Whether the Windows or macOS hidden flag is set.

### `compressed`

Whether the platform compressed flag is set.

### `immutable`

Whether the Linux or macOS immutable flag is set.

### `append_only`

Whether the Linux or macOS append-only flag is set.

### `no_dump`

Whether the Linux or macOS no-dump flag is set.

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
