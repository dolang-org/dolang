# fs

The `fs` module provides functions and types for filesystem operations.

## Types

- [Path](path.md)

## Functions

### `open path mode? func?`

Opens a file and returns a File object.

**Parameters:**

| Name   | Type                   | Description                                          |
| ------ | ---------------------- | ---------------------------------------------------- |
| `path` | [`str`](../std/str.md) | Path to the file to open                             |
| `mode` | `str`                  | File access mode (default: `"r"`)                    |
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

**Returns:** File

**Example:**

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

**Parameters:**

| Name     | Type                                      | Description                                |
| -------- | ----------------------------------------- | ------------------------------------------ |
| `path`   | [`str`](../std/str.md)\|[`Path`](path.md) | One or more paths to remove                |
| `all`    | [`bool`](../std/bool.md)                  | If `true`, removes directories recursively |
| `ignore` | [`bool`](../std/bool.md)                  | If `true`, ignores a missing path          |

**Example:**

```
write "temp.txt" "temporary data"
remove "temp.txt"

remove "missing.txt" ignore: true
remove "build" all: true
remove "a.txt" "b.txt"
```

### `exists path`

Checks whether a file or directory exists at the given path.

**Parameters:**

| Name   | Type                                      | Description                 |
| ------ | ----------------------------------------- | --------------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to check for existence |

**Returns:** [`bool`](../std/bool.md) - `true` if the path
exists, `false` otherwise

**Example:**

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

By default, returns text as a [`str`](../std/str.md). If `mode` is
`"b"`, returns raw bytes as [`bin`](../std/bin.md).

**Parameters:**

| Name   | Type                                      | Description                                 |
| ------ | ----------------------------------------- | ------------------------------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the file to read                    |
| `mode` | `str`                                     | Optional mode string; only `"b"` is allowed |

**Returns:** [`str`](../std/str.md)\|[`bin`](../std/bin.md)

**Example:**

```
let text = read "config.txt"
let data = read "archive.bin" "b"
```

### `write path content`

Writes the entire contents of a file in one call, creating or truncating the
file.

Binary values are written as raw bytes. All other values are converted to a
string first.

**Parameters:**

| Name      | Type                                      | Description               |
| --------- | ----------------------------------------- | ------------------------- |
| `path`    | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the file to write |
| `content` | `str`\|`bin`                              | Value to write            |

**Returns:** [`int`](../std/int.md) - Number of bytes written

**Example:**

```
write "message.txt" "hello"
write "data.bin" b"\x01\x02\x03"
```

### `set_len path size`

Truncates the file at the given path to the specified byte length, creating it
if needed.

**Parameters:**

| Name   | Type                                      | Description              |
| ------ | ----------------------------------------- | ------------------------ |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the file         |
| `size` | [`int`](../std/index.md)                  | New file length in bytes |

**Example:**

```
set_len "output.txt" 0
set_len (Path "archive.bin") 1024
```

### `is_absolute path`

Checks whether a path is absolute.

**Parameters:**

| Name   | Type                                      | Description   |
| ------ | ----------------------------------------- | ------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to check |

**Returns:** [`bool`](../std/bool.md) - `true` if the path is absolute,
`false` if relative

**Example:**

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

**Returns:** [`Path`](path.md)

### `cache_dir()`

Returns the platform-native user cache directory as a [`Path`](path.md).

**Platform behavior:**

| Platform       | Result                                                                |
| -------------- | --------------------------------------------------------------------- |
| Non-macOS Unix | `env["XDG_CACHE_HOME"]`, or `home_dir() / ".cache"` if unset          |
| macOS          | `home_dir() / "Library" / "Caches"`                                   |
| Windows        | `FOLDERID_LocalAppData`, typically `home_dir() / "AppData" / "Local"` |

**Returns:** [`Path`](path.md)

### `metadata path :follow = true`

Gets file metadata for the given path.

**Parameters:**

| Name     | Type                                      | Description                                                 |
| -------- | ----------------------------------------- | ----------------------------------------------------------- |
| `path`   | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the file or directory                               |
| `follow` | [`bool`](../std/bool.md)                  | If `false`, returns metadata for symlink instead of target. |

**Returns:** Record with the following fields:

**Always present:**

| Field  | Type                   | Description                                               |
| ------ | ---------------------- | --------------------------------------------------------- |
| `len`  | [`int`](../std/int.md) | File size in bytes                                        |
| `type` | [`sym`](../std/sym.md) | File type: `:file:`, `:dir:`, `:symlink:`, or `:unknown:` |

**Optional timestamps** (platform-dependent):

| Field      | Type                              | Description            |
| ---------- | --------------------------------- | ---------------------- |
| `modified` | [`DateTime`](../time/datetime.md) | Last modification time |
| `accessed` | [`DateTime`](../time/datetime.md) | Last access time       |
| `created`  | [`DateTime`](../time/datetime.md) | Creation/change time   |

**Unix-only** (not available on Windows):

| Field     | Type                   | Description                           |
| --------- | ---------------------- | ------------------------------------- |
| `mode`    | [`int`](../std/int.md) | File permissions and type (stat mode) |
| `dev`     | [`int`](../std/int.md) | Device ID                             |
| `ino`     | [`int`](../std/int.md) | Inode number                          |
| `nlink`   | [`int`](../std/int.md) | Number of hard links                  |
| `uid`     | [`int`](../std/int.md) | User ID of owner                      |
| `gid`     | [`int`](../std/int.md) | Group ID of owner                     |
| `rdev`    | [`int`](../std/int.md) | Device ID (if special file)           |
| `blksize` | [`int`](../std/int.md) | Preferred block size for I/O          |
| `blocks`  | [`int`](../std/int.md) | Number of 512-byte blocks allocated   |

**Example:**

```
let meta = metadata "data.txt"
echo "Size: $(meta.len)"
echo "Type: $(meta.type)"

if (meta.mode != nil)
  echo "Mode: $(meta.mode)"

# Get symlink metadata without following
let link_meta = metadata "link.txt" follow: false
echo "Link type: $(link_meta.type)"
```

### `copy from to :all?`

Copies a filesystem entry from one location to another.

By default this copies a single file or symlink. With `all: true`, it also
copies directories recursively.

**Parameters:**

| Name   | Type                                      | Description                                |
| ------ | ----------------------------------------- | ------------------------------------------ |
| `from` | [`str`](../std/str.md)\|[`Path`](path.md) | Source path                                |
| `to`   | [`str`](../std/str.md)\|[`Path`](path.md) | Destination path                           |
| `all`  | [`bool`](../std/bool.md)                  | If `true`, allows recursive directory copy |

**Example:**

```
copy "source.txt" "backup.txt"
copy "project" "project-backup" all: true
```

### `rename from to`

Renames (moves) a file or directory.

**Parameters:**

| Name   | Type                                      | Description      |
| ------ | ----------------------------------------- | ---------------- |
| `from` | [`str`](../std/str.md)\|[`Path`](path.md) | Source path      |
| `to`   | [`str`](../std/str.md)\|[`Path`](path.md) | Destination path |

**Example:**

```
rename "old_name.txt" "new_name.txt"

# Move to different directory
rename "file.txt" "subdir/file.txt"
```

### `move from to :all?`

Moves a filesystem entry from one location to another.

This first tries a plain rename. If that fails because the source and
destination are on different filesystems, it falls back to copy-and-delete.
By default this moves a single file or symlink. With `all: true`, it also
moves directories recursively.

**Parameters:**

| Name   | Type                                      | Description                                |
| ------ | ----------------------------------------- | ------------------------------------------ |
| `from` | [`str`](../std/str.md)\|[`Path`](path.md) | Source path                                |
| `to`   | [`str`](../std/str.md)\|[`Path`](path.md) | Destination path                           |
| `all`  | [`bool`](../std/bool.md)                  | If `true`, allows recursive directory move |

**Example:**

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

**Parameters:**

| Name  | Type                                      | Description                       |
| ----- | ----------------------------------------- | --------------------------------- |
| `src` | [`str`](../std/str.md)\|[`Path`](path.md) | Target path the symlink points to |
| `dst` | [`str`](../std/str.md)\|[`Path`](path.md) | Path where the symlink is created |

**Errors:** Raises a runtime error if the target cannot be accessed on Windows.

**Example:**

```
symlink "/path/to/target" "link_name"
```

### `symlink_dir src dst`

Creates a directory symbolic link at `dst` pointing to `src`.

**Platform Notes:**

- **Unix:** Equivalent to `symlink`
- **Windows:** Creates a directory symlink (requires appropriate permissions on
  some Windows versions)

**Parameters:**

| Name  | Type                                      | Description                       |
| ----- | ----------------------------------------- | --------------------------------- |
| `src` | [`str`](../std/str.md)\|[`Path`](path.md) | Target directory path             |
| `dst` | [`str`](../std/str.md)\|[`Path`](path.md) | Path where the symlink is created |

**Example:**

```
symlink_dir "/path/to/dir" "dir_link"
```

### `symlink_file src dst`

Creates a file symbolic link at `dst` pointing to `src`.

**Platform Notes:**

- **Unix:** Equivalent to `symlink`
- **Windows:** Creates a file symlink (may require appropriate permissions on
  some Windows versions)

**Parameters:**

| Name  | Type                                      | Description                       |
| ----- | ----------------------------------------- | --------------------------------- |
| `src` | [`str`](../std/str.md)\|[`Path`](path.md) | Target file path                  |
| `dst` | [`str`](../std/str.md)\|[`Path`](path.md) | Path where the symlink is created |

**Example:**

```
symlink_file "/path/to/file" "file_link"
```

### `entries path`

Reads the entries in a directory.

**Parameters:**

| Name   | Type                                      | Description           |
| ------ | ----------------------------------------- | --------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the directory |

**Returns:** Iterable of [`DirEntry`](direntry.md) objects

**Example:**

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

**Parameters:**

| Name   | Type                                      | Description                               |
| ------ | ----------------------------------------- | ----------------------------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the directory to create           |
| `all`  | [`bool`](../std/bool.md)                  | If `true`, creates parent directories too |

**Example:**

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

**Parameters:**

| Name     | Type                                      | Description                                                      |
| -------- | ----------------------------------------- | ---------------------------------------------------------------- |
| `path`   | [`str`](../std/str.md)\|[`Path`](path.md) | One or more directories to remove                                |
| `all`    | [`bool`](../std/bool.md)                  | If `true`, recursively prunes only empty directory subtrees      |
| `ignore` | [`bool`](../std/bool.md)                  | If `true`, ignores missing directories and file-blocked subtrees |

**Example:**

```
# Remove an empty directory
remove_dir empty_dir

# Remove an empty directory tree
remove_dir dir_to_remove all: true

# Prune only the empty branches and ignore file-blocked subtrees
remove_dir cache tmp all: true ignore: true
```

### `glob pattern :max_depth? :follow?`

Returns an iterator over paths matching a glob pattern.

**Parameters:**

| Name        | Type                     | Description                                                       |
| ----------- | ------------------------ | ----------------------------------------------------------------- |
| `pattern`   | `str`                    | Glob pattern (e.g., `"*.txt"`, `"**/*.rs"`)                       |
| `max_depth` | [`int`](../std/int.md)   | Maximum directory depth to traverse (default: unlimited)          |
| `follow`    | [`bool`](../std/bool.md) | Whether to follow symbolic links when traversing (default: false) |

**Returns:** Iterable of [`Path`](path.md) objects

**Glob pattern syntax:**

- `*` - Match any sequence of characters except path separator
- `?` - Match a single character
- `**` - Match any sequence of characters including path separators (recursive)
- `[abc]` - Match any character in the set
- `{a,b,c}` - Match any of the comma-separated patterns

**Example:**

```
# Find all text files
for path = glob "*.txt"
  echo "Found: $path"

# Recursive search with depth limit
for path = glob "**/*.rs" max_depth: 3
  echo "Source: $path"

# Follow symlinks
for path = glob "**/*" follow: true
  echo "Entry: $path"
```

### `normal path`

Returns a normalized path with `.` and `..` components resolved without
accessing the filesystem.

**Parameters:**

| Name   | Type                                      | Description       |
| ------ | ----------------------------------------- | ----------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to normalize |

**Returns:** [`Path`](path.md) - Normalized path

**Example:**

```
# Remove redundant components
let clean = normal "./foo/../bar/./baz"
echo $clean  # bar/baz

# Works with Path objects too
let path = Path "a/b/../c"
let norm = normal $path
echo $norm  # a/c
```

### `absolute path`

Returns the absolute form of a path based on the current working directory.

**Parameters:**

| Name   | Type                                      | Description           |
| ------ | ----------------------------------------- | --------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to make absolute |

**Returns:** [`Path`](path.md) - Absolute path

**Example:**

```
let abs = absolute "./config.txt"
echo $abs  # /current/working/dir/config.txt

# Already absolute paths are unchanged
let unchanged = absolute "/etc/passwd"
echo $unchanged  # /etc/passwd
```

### `relative path base?`

Returns the path relative to a base directory.

**Parameters:**

| Name   | Type                                      | Description                   |
| ------ | ----------------------------------------- | ----------------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to make relative         |
| `base` | [`str`](../std/str.md)\|[`Path`](path.md) | Base directory (default: cwd) |

**Returns:** [`Path`](path.md) - Relative path, or the original path if it
cannot be made relative

**Example:**

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

### `chmod path mode`

Changes the permissions of a file or directory.

**Platform Notes:**

- **Unix:** Changes file permissions using standard Unix mode bits
- **Windows:** Raises a runtime error (not supported)

**Parameters:**

| Name   | Type                                      | Description                          |
| ------ | ----------------------------------------- | ------------------------------------ |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to the file or directory        |
| `mode` | [`int`](../std/int.md)                    | Permission mode bits (e.g., `0o755`) |

**Errors:** Raises a runtime error on non-Unix platforms or if the operation
fails.

**Example:**

```
# Set file permissions to rwxr-xr-x (755)
chmod "script.sh" 0o755

# Set directory permissions to rwxrwxrwx (777)
chmod "/tmp/shared" 0o777

# Remove write permissions for group and others
chmod "readonly.txt" 0o444
```

### `set_timestamps path :modified? :accessed? :created?`

Updates the timestamps of a file or directory.

**Platform Notes:**

- **Unix:** `modified` and `accessed` are available; `created` is not supported
- **Windows:** `modified`, `accessed`, and `created` are available

Unspecified timestamps are left unchanged.

**Parameters:**

| Name       | Type                                      | Description                               |
| ---------- | ----------------------------------------- | ----------------------------------------- |
| `path`     | [`str`](../std/str.md)\|[`Path`](path.md) | Path to update                            |
| `modified` | [`DateTime`](../time/datetime.md)         | Optional new modification time            |
| `accessed` | [`DateTime`](../time/datetime.md)         | Optional new access time                  |
| `created`  | [`DateTime`](../time/datetime.md)         | Optional new creation time (Windows only) |

**Errors:** Raises a runtime error if `created` is used on unsupported
platforms or if the operation fails.

```
import time:
  - DateTime

set_timestamps "artifact.tar" modified: DateTime.from_unix(1700000000)
set_timestamps "cache.db" accessed: DateTime.now()
set_timestamps "cache.db" created: DateTime.from_unix(1690000000)
```

### `chown path user? :group? :follow = true`

Changes the owner and/or group of a file, directory, or symlink target.

**Platform Notes:**

- **Unix:** Available
- **Windows:** Not available

**Parameters:**

| Name     | Type                                           | Description                               |
| -------- | ---------------------------------------------- | ----------------------------------------- |
| `path`   | [`str`](../std/str.md)\|[`Path`](path.md)      | Path to update                            |
| `user`   | [`int`](../std/int.md)\|[`str`](../std/str.md) | Optional owner UID or user name           |
| `group`  | [`int`](../std/int.md)\|[`str`](../std/str.md) | Optional group GID or group name          |
| `follow` | [`bool`](../std/bool.md)                       | If `false`, operate on the symlink itself |

At least one of `user` or `group` must be provided.

**Example:**

```
chown "script.sh" "deploy"
chown "artifact" group: "build"
chown "cache" 1000 group: 1000
chown "link" group: "www-data" follow: false
```

### `canonical path`

Returns the canonical, absolute form of a path with all intermediate components
normalized and symbolic links resolved.

**Parameters:**

| Name   | Type                                      | Description          |
| ------ | ----------------------------------------- | -------------------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path to canonicalize |

**Returns:** [`Path`](path.md) - Canonical path

**Example:**

```
let abs = canonical "./foo/../bar"
echo $abs # /current/working/dir/bar (with symlinks resolved)

```

### `with_temp_dir func`

Creates a temporary directory, invokes a callable with the directory path, then
removes the directory recursively upon return or error.

**Parameters:**

| Name     | Type | Description                                     |
| -------- | ---- | ----------------------------------------------- |
| `func`   | func | Called with a [`Path`](path.md) to the temp dir |
| `parent` | path | Parent directory for the temporary directory    |

**Platform Notes:**

The default parent of the temporary directory is chosen as follows:

- **Windows:** Uses the directory returned by
  [`GetTempPath2`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-gettemppath2a)
- **Unix:** Uses `TMPDIR` from the strand-local environment if set, otherwise
  `/tmp`

**Example:**

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
