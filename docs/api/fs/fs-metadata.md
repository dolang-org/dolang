# FsMetadata

Filesystem metadata returned by [`fs_metadata`](./index.md),
[`Path.fs_metadata()`](./path.md), and [`File.fs_metadata()`](./file.md).

This is metadata about the containing filesystem, not a single file entry. For
file metadata such as `len`, `type`, and timestamps, use
[`Metadata`](./metadata.md).

## Fields

### `capacity`

Total filesystem capacity in bytes.

### `free`

Total free space in bytes, including space reserved from unprivileged users.

### `available`

Free space available to the current process in bytes.

### `block_size`

Filesystem block size in bytes when available.

### `read_only`

Whether the filesystem is mounted read-only.

## Unix-Only Fields

### `blocks`

Total data blocks.

### `blocks_free`

Free data blocks, including reserved blocks.

### `blocks_available`

Free data blocks available to unprivileged users.

### `files`

Total file nodes.

### `files_free`

Free file nodes.

### `files_available`

Free file nodes available to unprivileged users.

### `fragment_size`

Filesystem fragment size in bytes.

### `linux_attrs`

Raw Linux filesystem mount attribute mask.

### `macos_attrs`

Raw macOS filesystem mount attribute mask.

### `fsid`

Raw filesystem identifier when the platform reports one.

### `name_max`

Maximum filename length.

### `no_suid`

Whether setuid execution is disabled.

### `no_exec`

Whether execution is disabled.

### `synchronous`

Whether writes are synchronous.

### `no_dev`

Whether device files are disabled.

### `no_atime`

Whether access time updates are disabled.

### `no_dir_atime`

Whether directory access time updates are disabled.

### `relatime`

Whether relative-atime updates are enabled.

## Windows-Only Fields

### `win_flags`

Raw Windows volume flags.

### `volume_serial_number`

Raw Windows volume serial number.

### `component_length_max`

Maximum path component length for the volume.
