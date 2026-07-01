# Attrs

Filesystem attributes returned by [`attrs`](./index.md),
[`Path.attrs()`](./path.md), and [`Metadata.attrs`](./metadata.md).

Fields are platform-specific. Accessing a field that is not known for the
current platform raises a field error.

## Fields

### `win_attrs`

Raw Windows file attribute bitmask.

### `readonly`

Whether the Windows readonly attribute bit is set.

### `hidden`

Whether the Windows hidden attribute bit or macOS hidden flag is set.

### `system`

Whether the Windows system attribute bit is set.

### `archive`

Whether the Windows archive attribute bit is set.

### `reparse_point`

Whether the Windows reparse-point attribute bit is set.

### `compressed`

Whether the Windows compressed attribute bit, Linux compressed flag, or macOS
compressed flag is set.

### `encrypted`

Whether the Windows encrypted attribute bit is set.

### `temporary`

Whether the Windows temporary attribute bit is set.

### `offline`

Whether the Windows offline attribute bit is set.

### `not_content_indexed`

Whether the Windows not-content-indexed attribute bit is set.

### `unix_flags`

Raw macOS/Linux filesystem flags.

### `immutable`

Whether the Linux or macOS immutable flag is set.

### `append_only`

Whether the Linux or macOS append-only flag is set.

### `no_dump`

Whether the Linux or macOS no-dump flag is set.

### `no_atime`

Whether the Linux no-atime flag is set.

### `no_copy_on_write`

Whether the Linux no-copy-on-write flag is set.

### `dir_sync`

Whether the Linux synchronous-directory-updates flag is set.

### `casefold`

Whether the Linux case-insensitive-directory-lookups flag is set.

### `data_journaling`

Whether the Linux data-journaling flag is set.

### `no_compress`

Whether the Linux don't-compress flag is set.

### `project_inherit`

Whether the Linux project-hierarchy flag is set.

### `secure_delete`

Whether the Linux secure-deletion flag is set.

### `sync`

Whether the Linux synchronous-updates flag is set.

### `no_tail_merge`

Whether the Linux no-tail-merging flag is set.

### `top_dir`

Whether the Linux top-of-directory-hierarchy flag is set.

### `undelete`

Whether the Linux undeletable flag is set.

### `direct_access`

Whether the Linux direct-access flag is set.

### `extent_format`

Whether the Linux extent-format flag is set.

### `opaque`

Whether the macOS opaque flag is set.
