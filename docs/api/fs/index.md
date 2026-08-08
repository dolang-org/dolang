# fs

The `fs` module provides functions and types for filesystem operations.

Ordinary metadata such as size, timestamps, ownership, permissions, and file
attributes are available through [`Metadata`](./metadata.md) and
[`set_metadata`](#set_metadata-resolve-paths). Extended attributes use
[`xattrs`](#xattrs-path-namespace-resolve) and related functions. POSIX ACLs
use [`acl`](#acl-path-default-resolve) and
[`set_acl`](#set_acl-path-acl-default-resolve). Windows security descriptors
can also be fetched and manipulated with full fidelity; see
[`sec_desc`](#sec_desc-path-owner-group-dacl-sacl-resolve) and the
[Security Guide](../../shell/security.md). Windows alternate data streams are
listed with [`streams`](#streams-path-resolve).

## Types

| Type                         | Description                    |
| ---------------------------- | ------------------------------ |
| [Path](path.md)              | Supertype for filesystem paths |
| [Metadata](metadata.md)      | Immutable filesystem metadata  |
| [DirEntry](direntry.md)      | Directory entry object         |
| [XattrEntry](xattr-entry.md) | Extended attribute entry       |

## Modules

| Module                             | Description              |
| ---------------------------------- | ------------------------ |
| [`fs.unix`](unix/index.md)         | Unix filesystem types    |
| [`fs.windows`](windows/index.md)   | Windows filesystem types |

## Resolution modes

Many functions accept a `resolve:` parameter that controls how symbolic links
and other recursive path resolution is handled. Two values are accepted:

- **`:TARGET:`** — Resolve all links to their final target. This is the default
  for most functions.
- **`:LINK:`** — Resolve all links except the final
  component. For example, given a symlink `link -> target`, `metadata link
  resolve: :LINK:` returns the link's own metadata rather than the target's.
  This is the default for `glob`.

On Unix, `:LINK:` corresponds to `lstat`-style behavior. On Windows, it applies
to both symbolic links and other reparse points such as directory junctions.

## Functions

### `open path mode? func?`

Opens a file and returns a File object.

#### Parameters

| Name   | Type                   | Description                                          |
| ------ | ---------------------- | ---------------------------------------------------- |
| `path` | [`Str`](../std/str.md) | Path to the file to open                             |
| `mode` | `Str`                  | File access mode (default: `"r"`)                    |
| `func` | func                   | Callable to run with the file; auto-closes when done |

**File modes:**

| Mode   | Description                              |
| ------ | ---------------------------------------- |
| `"r"`  | Read-only                                |
| `"w"`  | Write-only (truncates existing file)     |
| `"a"`  | Append to existing file                  |
| `"r+"` | Read and write                           |
| `"w+"` | Read and write (truncates existing file) |
| `"a+"` | Read and append                          |

Add `"b"` suffix for binary mode (e.g., `"rb"`, `"wb"`, `"r+b"`).

#### Returns

File

#### Example

``` 
# Read a file (auto-closed when block finishes)
open config.txt r do |file|
  let content = file.read()
  echo "Content: $content"

# Write with automatic cleanup
open output.txt w do |file|
  file.write "Hello, World!"

# Manual file management
let file = open data.txt w
file.write "some data"
file.close()
```

### `remove path... :all? :ignore?`

Removes one or more paths from the filesystem.

By default this removes a single file or symlink. With `all: true`, it also
removes directories recursively, similar to `rm -r`. With `ignore: true`,
missing paths are treated as success.

#### Parameters

| Name     | Type                                      | Description                                |
| -------- | ----------------------------------------- | ------------------------------------------ |
| `path`   | [`Str`](../std/str.md)\|[`Path`](path.md) | One or more paths to remove                |
| `all`    | [`Bool`](../std/bool.md)                  | If `true`, removes directories recursively |
| `ignore` | [`Bool`](../std/bool.md)                  | If `true`, ignores a missing path          |

#### Example

```
write "temp.txt" "temporary data"
remove "temp.txt"

remove "missing.txt" ignore: true
remove "build" all: true
remove "a.txt" "b.txt"
```

### `exists path`

Checks whether a file or directory exists at the given path.

#### Parameters

| Name   | Type                                      | Description                 |
| ------ | ----------------------------------------- | --------------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to check for existence |

#### Returns

[`Bool`](../std/bool.md) - `true` if the path
exists, `false` otherwise

#### Example

```
# Check before removing
if exists "temp.txt"
  remove "temp.txt"
  echo "Removed temp.txt"
else
  echo "temp.txt does not exist"

# Conditional file operations
if exists "config.yaml"
  echo "Found config file"
```

### `read path mode?`

Reads the entire contents of a file in one call.

By default, returns text as a [`Str`](../std/str.md). If `mode` is
`"b"`, returns raw bytes as [`Bin`](../std/bin.md).

#### Parameters

| Name   | Type                                      | Description                                 |
| ------ | ----------------------------------------- | ------------------------------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the file to read                    |
| `mode` | `Str`                                     | Optional mode string; only `"b"` is allowed |

#### Returns

[`Str`](../std/str.md)\|[`Bin`](../std/bin.md)

#### Example

```
let text = read "config.txt"
let data = read "archive.bin" "b"
```

### `write path content`

Writes the entire contents of a file in one call, creating or truncating the
file.

Binary values are written as raw bytes and strings as UTF-8 text.

#### Parameters

| Name      | Type                                      | Description               |
| --------- | ----------------------------------------- | ------------------------- |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the file to write |
| `content` | `Str`\|`Bin`                              | Value to write            |

#### Returns

[`Int`](../std/int.md) - Number of bytes written

#### Example

```
write "message.txt" "hello"
write "data.bin" b"\x01\x02\x03"
```

### `append path content`

Appends content to a file, creating it if needed.

#### Parameters

| Name      | Type                                      | Description                 |
| --------- | ----------------------------------------- | --------------------------- |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the file to append  |
| `content` | `Str`\|`Bin`                              | Content to append           |

#### Returns

[`Int`](../std/int.md) - Number of bytes written

#### Example

```
append "messages.txt" "another message\n"
append "data.bin" b"\x04\x05"
```

### `set_size path size`

Truncates the file at the given path to the specified byte length, creating it
if needed.

#### Parameters

| Name   | Type                                      | Description              |
| ------ | ----------------------------------------- | ------------------------ |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the file         |
| `size` | [`Int`](../std/index.md)                  | New file length in bytes |

#### Example

```
set_size "output.txt" 0
set_size (Path "archive.bin") 1024
```

### `is_absolute path`

Checks whether a path is absolute.

#### Parameters

| Name   | Type                                      | Description   |
| ------ | ----------------------------------------- | ------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to check |

#### Returns

[`Bool`](../std/bool.md) - `true` if the path is absolute,
`false` if relative

#### Example

```
# Check different paths
if is_absolute "/etc/passwd"
  echo "Absolute path"

if !is_absolute "./config.txt"
  echo "Relative path"
```

### `home_dir()`

Returns the current user's home directory as a [`Path`](path.md).

**Platform behavior:**

| Platform | Result                                                         |
| -------- | -------------------------------------------------------------- |
| Unix     | `env["HOME"]`, or home directory from passwd database if unset |
| Windows  | `FOLDERID_Profile`, typically `C:\Users\<user>`                |

#### Returns

[`Path`](path.md)

### `cache_dir :app?`

Returns the platform-native user cache directory as a [`Path`](path.md).

When `app` is given, the result is scoped to that application.

#### Parameters

| Name  | Type                    | Description      |
| ----- | ----------------------- | ---------------- |
| `app` | [`Str`](../std/str.md)? | Application name |

#### Platform behavior

Without `app`, the base directories are:

| Platform       | Result                                                                  |
| -------------- | ----------------------------------------------------------------------- |
| Non-macOS Unix | `$XDG_CACHE_HOME`, otherwise `~/.cache`                                 |
| macOS          | `(home_dir() / "Library" / "Caches")`                                   |
| Windows        | `FOLDERID_LocalAppData`, typically `(home_dir() / "AppData" / "Local")` |

With `app: myapp`:

| Platform       | Result                              |
| -------------- | ----------------------------------- |
| Non-macOS Unix | `(cache_dir() / "myapp)"`           |
| macOS          | `(cache_dir() / "myapp)"`           |
| Windows        | `(cache_dir() / "myapp" / "Cache")` |

#### Returns

[`Path`](path.md)

#### Example

```
let cache = cache_dir app: blastinator8000
echo "Cache: $cache"
```

### `metadata path :resolve?`

Gets file metadata for the given path.

#### Parameters

| Name      | Type                                      | Description                                      |
| --------- | ----------------------------------------- | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the file or directory                    |
| `resolve` | `:TARGET:`\|`:LINK:`                      | Resolution mode (see [above](#resolution-modes)) |

#### Returns

[`Metadata`](metadata.md)

**Fields:**

| Field  | Type                   | Description                                                                                                        |
| ------ | ---------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `len`  | [`Int`](../std/int.md) | File size in bytes                                                                                                 |
| `type` | [`Sym`](../std/sym.md) | File type: `:FILE:`, `:DIR:`, `:SYMLINK:`, `:FIFO:`, `:CHAR_DEVICE:`, `:BLOCK_DEVICE:`, `:SOCKET:`, or `:UNKNOWN:` |

**Optional timestamps** (platform-dependent):

| Field      | Type                              | Description            |
| ---------- | --------------------------------- | ---------------------- |
| `modified` | [`DateTime`](../time/datetime.md) | Last modification time |
| `accessed` | [`DateTime`](../time/datetime.md) | Last access time       |
| `created`  | [`DateTime`](../time/datetime.md) | Creation/change time   |

**Unix-only** (these fields do not exist on Windows):

| Field     | Type                   | Description                           |
| --------- | ---------------------- | ------------------------------------- |
| `mode`    | [`Int`](../std/int.md) | File permissions and type (stat mode) |
| `dev`     | [`Int`](../std/int.md) | Device ID                             |
| `ino`     | [`Int`](../std/int.md) | Inode number                          |
| `nlink`   | [`Int`](../std/int.md) | Number of hard links                  |
| `uid`     | [`Int`](../std/int.md) | User ID of owner                      |
| `gid`     | [`Int`](../std/int.md) | Group ID of owner                     |
| `rdev`    | [`Int`](../std/int.md) | Device ID (if special file)           |
| `blksize` | [`Int`](../std/int.md) | Preferred block size for I/O          |
| `blocks`  | [`Int`](../std/int.md) | Number of 512-byte blocks allocated   |

**Windows-only** (these fields do not exist on Unix):

| Field       | Type                   | Description                             |
| ----------- | ---------------------- | --------------------------------------- |
| `win_attrs` | [`Int`](../std/int.md) | Raw Windows file attribute bitmask      |

#### Example

```
let meta = metadata "data.txt"
echo "Size: $(meta.size)"
echo "Type: $(meta.type)"

if (sys.os_info().family != :WINDOWS:)
  echo "Mode: $(meta.mode)"
else
  echo "Attributes: $(meta.win_attrs)"

# Get symlink metadata without following
let link_meta = metadata "link.txt" resolve: :LINK:
echo "Link type: $(link_meta.type)"
```

### `fs_metadata path :resolve?`

Gets filesystem metadata for the filesystem containing the given path.

#### Parameters

| Name      | Type                                      | Description                                      |
| --------- | ----------------------------------------- | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to resolve                                  |
| `resolve` | `:TARGET:`\|`:LINK:`                      | Resolution mode (see [above](#resolution-modes)) |

#### Returns

[`FsMetadata`](fs-metadata.md)

#### Errors

| Exception              | Condition                           |
| ---------------------- | ----------------------------------- |
| `sys.UnsupportedError` | On Linux, `resolve: :LINK:` is used |

```
let meta = fs_metadata "data.txt"
echo "Capacity: $(meta.capacity)"
echo "Available: $(meta.available)"
echo "Readonly: $(meta.read_only)"
```

### `sec_desc path :owner? :group? :dacl? :sacl? :resolve?`

Gets selected parts of a Windows security descriptor.

#### Parameters

| Name      | Type                                      | Description                                      |
| --------- | ----------------------------------------- | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to query                                    |
| `owner`   | [`Bool`](../std/bool.md)                  | Load the owner SID                               |
| `group`   | [`Bool`](../std/bool.md)                  | Load the primary group SID                       |
| `dacl`    | [`Bool`](../std/bool.md)                  | Load the discretionary ACL                       |
| `sacl`    | [`Bool`](../std/bool.md)                  | Load the system ACL                              |
| `resolve` | `:TARGET:`\|`:LINK:`                      | Resolution mode (see [above](#resolution-modes)) |

#### Returns

[`security.windows.SecDesc`](../security/windows/secdesc.md)

### `set_sec_desc path desc :resolve?`

Applies the components selected by a Windows security descriptor's `mask`.

#### Parameters

| Name      | Type                                                         | Description                                      |
| --------- | ------------------------------------------------------------ | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md)                    | Path to update                                   |
| `desc`    | [`security.windows.SecDesc`](../security/windows/secdesc.md) | Security descriptor to apply                     |
| `resolve` | `:TARGET:`\|`:LINK:`                                         | Resolution mode (see [above](#resolution-modes)) |

### `acl path :default? :resolve?`

Gets the POSIX.1e ACL stored on a path.

#### Parameters

| Name      | Type                                      | Description                                      |
| --------- | ----------------------------------------- | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to query                                    |
| `default` | [`Bool`](../std/bool.md)                  | Query the directory's inheritable default ACL    |
| `resolve` | `:TARGET:`\|`:LINK:`                      | Resolution mode (see [above](#resolution-modes)) |

#### Returns

[`security.unix.Acl`](../security/unix/acl.md), or `nil` when no ACL metadata
is stored.

Linux and FreeBSD support this operation. Other targets raise
`sys.UnsupportedError`.

### `set_acl path acl :default? :resolve?`

Sets or removes a POSIX.1e ACL.

#### Parameters

| Name      | Type                                                                   | Description                                      |
| --------- | ---------------------------------------------------------------------- | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md)                              | Path to update                                   |
| `acl`     | [`security.unix.Acl`](../security/unix/acl.md)\|[`nil`](../std/nil.md) | ACL to set, or `nil` to remove it                |
| `default` | [`Bool`](../std/bool.md)                                               | Update the directory's inheritable default ACL   |
| `resolve` | `:TARGET:`\|`:LINK:`                                                   | Resolution mode (see [above](#resolution-modes)) |

Linux and FreeBSD support this operation. Other targets raise
`sys.UnsupportedError`.

### `xattrs path :namespace? :resolve?`

Lists extended attributes for the given path.

On Windows, this uses NTFS extended attributes. Returned names may differ in
case from the requested name.

#### Parameters

| Name        | Type                                            | Description                                                                                              |
| ----------- | ----------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `path`      | [`Str`](../std/str.md)\|[`Path`](path.md)       | Path to query                                                                                            |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)? | Namespace to query; `:USER:` and `:SYSTEM:` name well-known namespaces, and `:ANY:` lists all namespaces |
| `resolve`   | `:TARGET:`\|`:LINK:`                            | Resolution mode (see [above](#resolution-modes))                                                         |

#### Returns

iterator of [`XattrEntry`](xattr-entry.md)

```
for attr = xattrs "data.txt"
  echo $attr.name
```

### `streams path :resolve?`

Lists alternate data streams for the given path.

This is only supported on Windows.

#### Parameters

| Name      | Type                                      | Description                                      |
| --------- | ----------------------------------------- | ------------------------------------------------ |
| `path`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to query                                    |
| `resolve` | `:TARGET:`\|`:LINK:`                      | Resolution mode (see [above](#resolution-modes)) |

#### Returns

`Iter` of [`fs.windows.StreamEntry`](windows/stream-entry.md)

```
let path = Path data.txt
open $path r do |file|
  for stream = file.streams()
    echo "$(stream.name) $(stream.type)"
    echo (path / stream)
```

### `xattr path name :namespace? :resolve?`

Gets an extended attribute value.

#### Parameters

| Name        | Type                                                   | Description                                      |
| ----------- | ------------------------------------------------------ | ------------------------------------------------ |
| `path`      | [`Str`](../std/str.md)\|[`Path`](path.md)              | Path to query                                    |
| `name`      | [`Str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs`            |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)?        | Namespace to query                               |
| `resolve`   | `:TARGET:`\|`:LINK:`                                   | Resolution mode (see [above](#resolution-modes)) |

#### Returns

[`Bin`](../std/bin.md)

```
let value = xattr "data.txt" "comment"
```

### `set_xattr path name value :namespace? :resolve?`

Sets an extended attribute value.

On Windows, empty values are rejected. NTFS deletes the attribute instead of
storing an empty value.

#### Parameters

| Name        | Type                                                   | Description                                      |
| ----------- | ------------------------------------------------------ | ------------------------------------------------ |
| `path`      | [`Str`](../std/str.md)\|[`Path`](path.md)              | Path to update                                   |
| `name`      | [`Str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs`            |
| `value`     | [`Str`](../std/str.md)\|[`Bin`](../std/bin.md)         | Attribute bytes; strings use UTF-8               |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)?        | Namespace to update                              |
| `resolve`   | `:TARGET:`\|`:LINK:`                                   | Resolution mode (see [above](#resolution-modes)) |

```
set_xattr "data.txt" "comment" "ready"
set_xattr "data.txt" "raw" b"\x00\x01"
```

### `remove_xattr path name :namespace? :resolve?`

Removes an extended attribute.

#### Parameters

| Name        | Type                                                   | Description                                      |
| ----------- | ------------------------------------------------------ | ------------------------------------------------ |
| `path`      | [`Str`](../std/str.md)\|[`Path`](path.md)              | Path to update                                   |
| `name`      | [`Str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs`            |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)?        | Namespace to update                              |
| `resolve`   | `:TARGET:`\|`:LINK:`                                   | Resolution mode (see [above](#resolution-modes)) |

```
remove_xattr "data.txt" "comment"
```

### `copy from to :all?`

Copies a filesystem entry from one location to another.

By default this copies a single file or symlink. With `all: true`, it also
copies directories recursively.

#### Parameters

| Name   | Type                                      | Description                                |
| ------ | ----------------------------------------- | ------------------------------------------ |
| `from` | [`Str`](../std/str.md)\|[`Path`](path.md) | Source path                                |
| `to`   | [`Str`](../std/str.md)\|[`Path`](path.md) | Destination path                           |
| `all`  | [`Bool`](../std/bool.md)                  | If `true`, allows recursive directory copy |

#### Example

```
copy "source.txt" "backup.txt"
copy "project" "project-backup" all: true
```

### `rename from to :replace?`

Renames (moves) a file or directory.

By default, this replaces an existing destination. Set `replace` to `false`
to fail atomically instead.

!!! note
    `replace: false` is not supported on FreeBSD.

#### Parameters

| Name      | Type                                      | Description                                  |
| --------- | ----------------------------------------- | -------------------------------------------- |
| `from`    | [`Str`](../std/str.md)\|[`Path`](path.md) | Source path                                  |
| `to`      | [`Str`](../std/str.md)\|[`Path`](path.md) | Destination path                             |
| `replace` | [`Bool`](../std/bool.md)?                 | Whether to replace an existing destination   |

#### Example

```
rename "old_name.txt" "new_name.txt"

# Move to different directory
rename "file.txt" "subdir/file.txt"

# Fail if the destination exists
rename "draft.txt" "published.txt" replace: false
```

### `move from to :all?`

Moves a filesystem entry from one location to another.

This first tries a plain rename. If that fails because the source and
destination are on different filesystems, it falls back to copy-and-delete.
By default this moves a single file or symlink. With `all: true`, it also
moves directories recursively.

#### Parameters

| Name   | Type                                      | Description                                |
| ------ | ----------------------------------------- | ------------------------------------------ |
| `from` | [`Str`](../std/str.md)\|[`Path`](path.md) | Source path                                |
| `to`   | [`Str`](../std/str.md)\|[`Path`](path.md) | Destination path                           |
| `all`  | [`Bool`](../std/bool.md)                  | If `true`, allows recursive directory move |

#### Example

```
move "source.txt" "dest.txt"
move "project" "archive/project" all: true
```

### `symlink src dst`

Creates a symbolic link at `dst` pointing to `src`.

**Platform Notes:**

- **Unix:** Creates a standard symbolic link
- **Windows:** Attempts to determine if the target is a file or directory by
  reading its metadata. If the target cannot be accessed, the operation fails.
  For explicit control, use `symlink_file` or `symlink_dir`.

#### Parameters

| Name  | Type                                      | Description                       |
| ----- | ----------------------------------------- | --------------------------------- |
| `src` | [`Str`](../std/str.md)\|[`Path`](path.md) | Target path the symlink points to |
| `dst` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path where the symlink is created |

#### Errors

| Exception           | Condition                                |
| ------------------- | ---------------------------------------- |
| `sys.NotFoundError` | The target cannot be accessed on Windows |

#### Example

```
symlink "/path/to/target" "link_name"
```

### `symlink_dir src dst`

Creates a directory symbolic link at `dst` pointing to `src`.

**Platform Notes:**

- **Unix:** Equivalent to `symlink`
- **Windows:** Creates a directory symlink (requires appropriate permissions on
  some Windows versions)

#### Parameters

| Name  | Type                                      | Description                       |
| ----- | ----------------------------------------- | --------------------------------- |
| `src` | [`Str`](../std/str.md)\|[`Path`](path.md) | Target directory path             |
| `dst` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path where the symlink is created |

#### Example

```
symlink_dir "/path/to/dir" "dir_link"
```

### `symlink_file src dst`

Creates a file symbolic link at `dst` pointing to `src`.

**Platform Notes:**

- **Unix:** Equivalent to `symlink`
- **Windows:** Creates a file symlink (may require appropriate permissions on
  some Windows versions)

#### Parameters

| Name  | Type                                      | Description                       |
| ----- | ----------------------------------------- | --------------------------------- |
| `src` | [`Str`](../std/str.md)\|[`Path`](path.md) | Target file path                  |
| `dst` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path where the symlink is created |

#### Example

```
symlink_file "/path/to/file" "file_link"
```

### `hard_link src dst`

Creates a hard link at `dst` pointing to the existing file at `src`.

This uses the platform-native hard-link operation. The source must already
exist, and the link must be created on the same filesystem or volume if the
platform requires it.

#### Parameters

| Name  | Type                                      | Description                         |
| ----- | ----------------------------------------- | ----------------------------------- |
| `src` | [`Str`](../std/str.md)\|[`Path`](path.md) | Existing file to link to            |
| `dst` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path where the hard link is created |

#### Example

```
hard_link "data.txt" "data-copy.txt"
```

### `entries path`

Reads the entries in a directory.

#### Parameters

| Name   | Type                                      | Description           |
| ------ | ----------------------------------------- | --------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the directory |

#### Returns

Iterable of [`DirEntry`](direntry.md) objects

#### Example

```
# Iterate over directory entries
for entry = entries /home/user/docs
  echo "$(entry.name) - $(entry.type)"

# Collect into an array
let files = [...entries "."]
echo "Found $(files.len) entries"
```

### `create_dir path :all?`

Creates a directory at the given path.

#### Parameters

| Name   | Type                                      | Description                               |
| ------ | ----------------------------------------- | ----------------------------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the directory to create           |
| `all`  | [`Bool`](../std/bool.md)                  | If `true`, creates parent directories too |

#### Example

```
# Create a single directory
create_dir new_dir

# Create directory and all parents
create_dir a/b/c all: true
```

### `remove_dir path... :all? :ignore?`

Removes one or more directories.

By default this removes only empty directories. With `all: true`, it removes
directories recursively, but only through subtrees that contain directories and
no files or other non-directory entries. Use
[`remove`](index.md#remove-path-all-ignore) to delete directories that contain
files.

#### Parameters

| Name     | Type                                      | Description                                                      |
| -------- | ----------------------------------------- | ---------------------------------------------------------------- |
| `path`   | [`Str`](../std/str.md)\|[`Path`](path.md) | One or more directories to remove                                |
| `all`    | [`Bool`](../std/bool.md)                  | If `true`, recursively prunes only empty directory subtrees      |
| `ignore` | [`Bool`](../std/bool.md)                  | If `true`, ignores missing directories and file-blocked subtrees |

#### Example

```
# Remove an empty directory
remove_dir empty_dir

# Remove an empty directory tree
remove_dir dir_to_remove all: true

# Prune only the empty branches and ignore file-blocked subtrees
remove_dir cache tmp all: true ignore: true
```

### `glob pattern :max_depth? :resolve?`

Returns an iterator over paths matching a glob pattern.

#### Parameters

| Name        | Type                   | Description                                                          |
| ----------- | ---------------------- | -------------------------------------------------------------------- |
| `pattern`   | `Str`                  | Glob pattern (e.g., `"*.txt"`, `"**/*.rs"`)                          |
| `max_depth` | [`Int`](../std/int.md) | Maximum directory depth to traverse (default: unlimited)             |
| `resolve`   | `:TARGET:`\|`:LINK:`   | Resolution mode (see [above](#resolution-modes)) (default: `:LINK:`) |

#### Returns

`Iter` of [`Path`](path.md) objects

**Glob pattern syntax:**

- `*` - Match any sequence of characters except path separator
- `?` - Match a single character
- `**` - Match any sequence of characters including path separators (recursive)
- `[abc]` - Match any character in the set
- `{a,b,c}` - Match any of the comma-separated patterns

#### Example

```
# Find all text files
for path = glob "*.txt"
  echo "Found: $path"

# Recursive search with depth limit
for path = glob "**/*.rs" max_depth: 3
  echo "Source: $path"

# Follow symlinks
for path = glob "**/*" resolve: :TARGET:
  echo "Entry: $path"
```

### `normalize path`

Returns a normalized path with `.` and `..` components resolved without
accessing the filesystem.

Unresolvable `..` components in relative paths are preserved.

#### Parameters

| Name   | Type                                      | Description       |
| ------ | ----------------------------------------- | ----------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to normalize |

#### Returns

[`Path`](path.md) - Normalized path

#### Example

```
# Remove redundant components
let clean = normalize "./foo/../bar/./baz"
echo $clean  # bar/baz

# Works with Path objects too
let path = Path "a/b/../c"
let norm = normalize $path
echo $norm  # a/c
```

### `absolute path`

Returns the absolute form of a path based on the current working directory.

#### Parameters

| Name   | Type                                      | Description           |
| ------ | ----------------------------------------- | --------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to make absolute |

#### Returns

[`Path`](path.md) - Absolute path

#### Example

```
let abs = absolute "./config.txt"
echo $abs  # /current/working/dir/config.txt

# Already absolute paths are unchanged
let unchanged = absolute "/etc/passwd"
echo $unchanged  # /etc/passwd
```

### `relative path base?`

Returns the path relative to a base directory.

#### Parameters

| Name   | Type                                      | Description                   |
| ------ | ----------------------------------------- | ----------------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to make relative         |
| `base` | [`Str`](../std/str.md)\|[`Path`](path.md) | Base directory (default: cwd) |

#### Returns

[`Path`](path.md) - Relative path, or the original path if it
cannot be made relative

#### Example

```
# Relative to current directory
let rel = relative "/home/user/docs/file.txt"
echo $rel  # docs/file.txt (if cwd is /home/user)

# Relative to specific base
let rel2 = relative "/a/b/c/d" "/a/b"
echo $rel2  # c/d

# Returns original if no common prefix
let unchanged = relative "/etc/passwd" "/home/user"
echo $unchanged  # /etc/passwd
```

### `set_metadata :resolve? ...paths ...`

Updates timestamps, permissions, ownership, and filesystem attributes.

Unspecified metadata is left unchanged. Unix targets support `mode`, numeric or
named `user` and `group` values, and applicable filesystem attributes. Windows
targets accept an account name or [`Sid`](../security/windows/sid.md) for `user`
and `group` and support applicable filesystem attributes.
Unix supports `modified` and `accessed` timestamps; Windows also supports
`created`.
Paths are submitted from left to right and processing stops at the first error.
Within each path, ownership, mode, attributes, and timestamps are applied in
that order.
Backends may use multiple system operations; atomicity and rollback behavior
are unspecified.

#### Parameters

| Name                  | Type                                                                                | Description                                      |
| --------------------- | ----------------------------------------------------------------------------------- | ------------------------------------------------ |
| `paths`               | ([`Str`](../std/str.md)\|[`Path`](path.md))*                                        | Paths to update in order                         |
| `mode`                | [`Int`](../std/int.md)                                                              | Optional Unix permission mode                    |
| `user`                | [`Int`](../std/int.md)\|[`Str`](../std/str.md)\|[`Sid`](../security/windows/sid.md) | Optional owner ID, name, or SID                  |
| `group`               | [`Int`](../std/int.md)\|[`Str`](../std/str.md)\|[`Sid`](../security/windows/sid.md) | Optional group ID, name, or SID                  |
| `modified`            | [`DateTime`](../time/datetime.md)                                                   | Optional new modification time                   |
| `accessed`            | [`DateTime`](../time/datetime.md)                                                   | Optional new access time                         |
| `created`             | [`DateTime`](../time/datetime.md)                                                   | Optional new creation time (Windows only)        |
| `resolve`             | `:TARGET:`\|`:LINK:`                                                                | Resolution mode (see [above](#resolution-modes)) |
| `readonly`            | [`Bool`](../std/bool.md)                                                            | Optional readonly attribute value                |
| `hidden`              | [`Bool`](../std/bool.md)                                                            | Optional hidden attribute/flag                   |
| `system`              | [`Bool`](../std/bool.md)                                                            | Optional system attribute value                  |
| `archive`             | [`Bool`](../std/bool.md)                                                            | Optional archive attribute value                 |
| `compressed`          | [`Bool`](../std/bool.md)                                                            | Optional compressed flag                         |
| `temporary`           | [`Bool`](../std/bool.md)                                                            | Optional temporary value                         |
| `offline`             | [`Bool`](../std/bool.md)                                                            | Optional offline value                           |
| `not_content_indexed` | [`Bool`](../std/bool.md)                                                            | Optional indexing attribute value                |
| `immutable`           | [`Bool`](../std/bool.md)                                                            | Optional immutable flag                          |
| `append_only`         | [`Bool`](../std/bool.md)                                                            | Optional append-only flag                        |
| `no_dump`             | [`Bool`](../std/bool.md)                                                            | Optional no-dump flag                            |
| `no_atime`            | [`Bool`](../std/bool.md)                                                            | Optional Linux no-atime flag                     |
| `no_copy_on_write`    | [`Bool`](../std/bool.md)                                                            | Optional Linux no-COW flag                       |
| `dir_sync`            | [`Bool`](../std/bool.md)                                                            | Optional Linux dir-sync flag                     |
| `casefold`            | [`Bool`](../std/bool.md)                                                            | Optional Linux casefold flag                     |
| `data_journaling`     | [`Bool`](../std/bool.md)                                                            | Optional Linux journaling flag                   |
| `no_compress`         | [`Bool`](../std/bool.md)                                                            | Optional Linux no-compress flag                  |
| `project_inherit`     | [`Bool`](../std/bool.md)                                                            | Optional Linux project flag                      |
| `secure_delete`       | [`Bool`](../std/bool.md)                                                            | Optional Linux secure-delete flag                |
| `sync`                | [`Bool`](../std/bool.md)                                                            | Optional Linux sync flag                         |
| `no_tail_merge`       | [`Bool`](../std/bool.md)                                                            | Optional Linux no-tail flag                      |
| `top_dir`             | [`Bool`](../std/bool.md)                                                            | Optional Linux top-dir flag                      |
| `undelete`            | [`Bool`](../std/bool.md)                                                            | Optional Linux undelete flag                     |
| `direct_access`       | [`Bool`](../std/bool.md)                                                            | Optional Linux direct-access flag                |
| `extent_format`       | [`Bool`](../std/bool.md)                                                            | Optional Linux extent flag                       |
| `opaque`              | [`Bool`](../std/bool.md)                                                            | Optional macOS opaque flag                       |

#### Errors

| Exception              | Condition                                        |
| ---------------------- | ------------------------------------------------ |
| `sys.UnsupportedError` | The operation is used on an unsupported platform |

```
set_metadata "script.sh" mode: 0o755 user: "deploy" group: "deploy"
set_metadata "one.txt" "two.txt" mode: 0o640
set_metadata "data.txt" hidden: true
set_metadata "data.txt" no_dump: true
set_metadata "link" group: "www-data" resolve: :LINK:
set_metadata "artifact.tar" modified: $DateTime.from_unix(1700000000)
set_metadata "cache.db" accessed: $DateTime.now()
```

### `canonical path`

Returns the canonical, absolute form of a path with all intermediate components
normalized and symbolic links resolved.

#### Parameters

| Name   | Type                                      | Description          |
| ------ | ----------------------------------------- | -------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to canonicalize |

#### Returns

[`Path`](path.md) - Canonical path

#### Example

```
let abs = canonical "./foo/../bar"
echo $abs # /current/working/dir/bar (with symlinks resolved)

```

### `read_link path`

Reads the target of a symbolic link.

#### Parameters

| Name   | Type                                      | Description         |
| ------ | ----------------------------------------- | ------------------- |
| `path` | [`Str`](../std/str.md)\|[`Path`](path.md) | Path to the symlink |

#### Returns

[`Path`](path.md) - The path that the symlink points to

#### Errors

| Exception                   | Condition                                          |
| --------------------------- | -------------------------------------------------- |
| `sys.NotFoundError`         | The path does not exist                            |
| `sys.PermissionDeniedError` | Permission denied to read the symlink              |
| `sys.UnsupportedError`      | Reading symlinks is not supported on this platform |
| `sys.Error`                 | Other I/O errors                                   |

#### Example

```
let link = read_link "./my_link"
echo "Link points to: $link"
```

### `with_temp_dir func`

Creates a temporary directory, invokes a callable with the directory path, then
removes the directory recursively upon return or error.

#### Parameters

| Name     | Type | Description                                     |
| -------- | ---- | ----------------------------------------------- |
| `func`   | func | Called with a [`Path`](path.md) to the temp dir |
| `parent` | path | Parent directory for the temporary directory    |

**Platform Notes:**

The active VFS target chooses the default parent as follows:

- **Windows:** Uses the directory returned by
  [`GetTempPath2`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-gettemppath2a)
- **Unix:** Uses `TMPDIR` from the strand-local environment if set, otherwise
  `/tmp`

#### Example

```
# Use the temporary directory in the default location
with_temp_dir do |dir|
  let file = (dir / "test.txt")
  file.open w do |f|
    f.write "Hello, World!"
  echo "Wrote to: $file"

# Directory is automatically cleaned up

# Use a custom parent directory
with_temp_dir parent: my_temp do |dir|
  # ...
```
