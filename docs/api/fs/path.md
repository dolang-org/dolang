# Path

Path objects represent filesystem paths and provide methods for path
manipulation and file operations.

## Fields

### `name`

Returns the final component of the path, or `nil` if the path is empty.

```
let path = Path /home/user/file.txt
echo $path.name  # file.txt
```

### `stem`

Returns the final component without its last extension, or `nil` if the path is
empty.

```
let path = Path "/home/user/archive.tar.gz"
echo $path.stem  # archive.tar

let no_ext = Path "/home/user/Makefile"
echo $no_ext.stem  # Makefile
```

### `parent`

Returns the parent directory as a new Path, or `nil` if the path is empty or
contains only one component.

```
let path = Path /home/user/file.txt
let parent = path.parent
echo parent  # /home/user
```

### `ext`

Returns the file extension (without the leading dot), or `nil` if the final
component has no extension.

```
let path = Path "/home/user/file.txt"
echo $path.ext  # txt

let no_ext = Path "/home/user/noextension"
echo $no_ext.ext  # nil
```

### `is_absolute`

Returns whether the path is absolute (starts from the filesystem root).

```
let abs = Path "/home/user/file.txt"
echo $abs.is_absolute  # true

let rel = Path "./file.txt"
echo $rel.is_absolute  # false
```

## Platform-Specific Fields

### Windows

#### `disk`

Drive letter for `C:`-style and `\\?\C:`-style prefixes, or `nil` otherwise.

```
let path = Path "C:/work/file.txt"
echo $path.disk  # C
```

#### `server`

UNC server name, or `nil` if the path does not use a UNC prefix.

```
let path = Path "//server/share/file.txt"
echo $path.server  # server
```

#### `share`

UNC share name, or `nil` if the path does not use a UNC prefix.

```
let path = Path "//server/share/file.txt"
echo $path.share  # share
```

#### `device`

Device namespace name for `\\.\name` paths, or `nil` otherwise.

```
let path = Path r"\\.\COM42"
echo $path.device  # COM42
```

#### `verbatim`

Returns whether the path uses a verbatim `\\?\...` prefix.

```
let path = Path r"\\?\C:\work\file.txt"
echo $path.verbatim  # true
```

#### `stream_name`

Alternate data stream name from the final path component, or
`nil` if no stream is specified.

```
let path = Path "file.txt:zone"
echo $path.name         # file.txt
echo $path.stream_name  # zone
```

Use `stream_name != nil` to test whether a path targets an alternate stream.

#### `stream_type`

Explicit alternate data stream type without the leading `$`, or
`nil` if no type is specified.

```
let path = Path "file.txt:zone:$DATA"
echo $path.stream_type  # DATA
```

`Path` parses stream syntax only from the final component. Forms with more than
two `:` separators in that component are rejected, and an explicit stream type
must start with `$`.

## Class Methods

### `(init) path`

Constructs a new path.

| Name   | Type          | Description |
| ------ | ------------- | ----------- |
| `path` | `str`\|`Path` | The path    |

### `join ...components`

Joins multiple path components into a single path. Components may be Path
objects or strings.

If any component is an absolute path, it replaces everything before it.

**Parameters:**

| Name         | Type          | Description             |
| ------------ | ------------- | ----------------------- |
| `components` | `str`\|`Path` | Path components to join |

**Returns:** Path

**Example:**

```
let path = Path.join home user docs file.txt
echo $path.name  # file.txt

# Absolute path replaces everything before it
let abs = Path.join home /etc config.txt
echo $abs  # /etc/config.txt
```

## Methods

### `open :mode? :block?`

Opens the file at this path. Equivalent to `open` but with a `Path`
object.

**Parameters:**

| Name    | Type  | Description                                          |
| ------- | ----- | ---------------------------------------------------- |
| `mode`  | `str` | File access mode (default: `"r"`)                    |
| `block` | func  | Callable to run with the file; auto-closes when done |

**Returns:** File

**Example:**

```
let path = Path data.txt
path.open r do |file|
  let content = file.read()
  echo $content
```

### `metadata :follow = false`

Gets metadata for the file at this path.

**Parameters:**

| Name     | Type                     | Description                                                       |
| -------- | ------------------------ | ----------------------------------------------------------------- |
| `follow` | [`bool`](../std/bool.md) | If `false`, returns metadata for a symlink rather than its target |

**Returns:** [`Metadata`](metadata.md)

**Example:**

```
let path = Path config.json
let meta = path.metadata()
echo "Size: $(meta.len) bytes"

# Get symlink metadata without following
let link = Path "link.txt"
let link_meta = link.metadata follow: false
echo "Link points to: $(link_meta.type)"
```

### `attrs :follow = true`

Gets filesystem attributes for this path.

Equivalent to [`attrs`](index.md).

**Parameters:**

| Name     | Type                     | Description                                                  |
| -------- | ------------------------ | ------------------------------------------------------------ |
| `follow` | [`bool`](../std/bool.md) | If `false`, queries attributes for symlink instead of target |

**Returns:** [`Attrs`](attrs.md)

```
let path = Path "data.txt"
let a = path.attrs()
if a.hidden
  echo hidden
```

### `exists()`

Checks if the path exists.

Equivalent to the free function [`exists`](index.md#exists-path).

**Returns:** [`bool`](../std/bool.md)

**Example:**

```
let path = Path important.txt
if path.exists()
  echo "File exists!"
else
  echo "File not found"
```

### `read mode?`

Reads the entire contents of the file at this path.

Equivalent to [`read`](index.md#read-path-mode).

**Parameters:**

| Name   | Type  | Description                                 |
| ------ | ----- | ------------------------------------------- |
| `mode` | `str` | Optional mode string; only `"b"` is allowed |

**Returns:** [`str`](../std/str.md)\|[`bin`](../std/bin.md)

**Example:**

```
let path = Path "config.txt"
let text = path.read()
let bytes = (Path "archive.bin").read "b"
```

### `write content`

Writes the entire contents of the file at this path, creating or truncating
the file.

Equivalent to [`write`](index.md#write-path-content).

**Parameters:**

| Name      | Type | Description    |
| --------- | ---- | -------------- |
| `content` | any  | Value to write |

**Returns:** [`int`](../std/int.md) - Number of bytes written

**Example:**

```
let path = Path "output.txt"
path.write "hello"
```

### `set_len size`

Truncates the file at this path to the given byte length, creating it if
needed.

Equivalent to [`set_len`](index.md#set_len-path-size).

**Parameters:**

| Name   | Type                     | Description              |
| ------ | ------------------------ | ------------------------ |
| `size` | [`int`](../std/index.md) | New file length in bytes |

**Example:**

```
let path = Path "output.txt"
path.set_len 0
```

### `set_attrs :readonly? :hidden? ...`

Updates filesystem attributes for this path.

Equivalent to [`set_attrs`](index.md).

Unspecified attributes are left unchanged.

```
let path = Path "data.txt"
path.set_attrs hidden: true
path.set_attrs readonly: false
path.set_attrs no_dump: true
path.set_attrs opaque: false
```

### `xattrs :namespace? :follow = true`

Lists extended attributes for this path.

Equivalent to [`xattrs`](index.md).

**Parameters:**

| Name        | Type                                            | Description                                                      |
| ----------- | ----------------------------------------------- | ---------------------------------------------------------------- |
| `namespace` | [`str`](../std/str.md)\|[`sym`](../std/sym.md)? | Namespace to query; Linux accepts `:any:` to list all namespaces |
| `follow`    | [`bool`](../std/bool.md)                        | If `false`, does not follow a symlink                            |

**Returns:** iterator of [`XattrEntry`](xattr-entry.md)

```
let path = Path "data.txt"
for attr = path.xattrs()
  echo $attr.name
```

### `streams :follow = true`

Lists alternate data streams for this path.

Equivalent to [`streams`](index.md).

**Parameters:**

| Name     | Type                     | Description                           |
| -------- | ------------------------ | ------------------------------------- |
| `follow` | [`bool`](../std/bool.md) | If `false`, does not follow a symlink |

**Returns:** iterator of [`StreamEntry`](stream-entry.md)

```
let path = Path "data.txt"
for stream = path.streams()
  echo "$(stream.name) $(stream.type)"
  echo (path / stream)
```

### `xattr name :namespace? :follow = true`

Gets an extended attribute value.

Equivalent to [`xattr`](index.md).

**Parameters:**

| Name        | Type                                                   | Description                           |
| ----------- | ------------------------------------------------------ | ------------------------------------- |
| `name`      | [`str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs` |
| `namespace` | [`str`](../std/str.md)?                                | Namespace to query                    |
| `follow`    | [`bool`](../std/bool.md)                               | If `false`, does not follow a symlink |

**Returns:** [`bin`](../std/bin.md)

```
let path = Path "data.txt"
let value = path.xattr "comment"
```

### `set_xattr name value :namespace? :follow = true`

Sets an extended attribute value.

Equivalent to
[`set_xattr`](index.md).

**Parameters:**

| Name        | Type                                                   | Description                           |
| ----------- | ------------------------------------------------------ | ------------------------------------- |
| `name`      | [`str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs` |
| `value`     | [`str`](../std/str.md)\|[`bin`](../std/bin.md)         | Attribute bytes; strings use UTF-8    |
| `namespace` | [`str`](../std/str.md)?                                | Namespace to update                   |
| `follow`    | [`bool`](../std/bool.md)                               | If `false`, does not follow a symlink |

```
let path = Path "data.txt"
path.set_xattr "comment" "ready"
```

### `remove_xattr name :namespace? :follow = true`

Removes an extended attribute.

Equivalent to
[`remove_xattr`](index.md).

**Parameters:**

| Name        | Type                                                   | Description                           |
| ----------- | ------------------------------------------------------ | ------------------------------------- |
| `name`      | [`str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs` |
| `namespace` | [`str`](../std/str.md)?                                | Namespace to update                   |
| `follow`    | [`bool`](../std/bool.md)                               | If `false`, does not follow a symlink |

```
let path = Path "data.txt"
path.remove_xattr "comment"
```

### `copy to :all?`

Copies this filesystem entry to `to`.

Equivalent to [`copy`](index.md#copy-from-to-all).

By default this copies a single file or symlink. With `all: true`, it also
copies directories recursively.

**Parameters:**

| Name  | Type                                      | Description                                |
| ----- | ----------------------------------------- | ------------------------------------------ |
| `to`  | [`str`](../std/str.md)\|[Path](path.md)   | Destination path                           |
| `all` | [`bool`](../std/bool.md)                  | If `true`, allows recursive directory copy |

**Example:**

```
let src = Path "source.txt"
src.copy "backup.txt"

let dir = Path "project"
dir.copy "project-backup" all: true
```

### `rename to`

Renames this path to `to`.

Equivalent to [`rename`](index.md#rename-from-to).

**Parameters:**

| Name | Type                                    | Description      |
| ---- | --------------------------------------- | ---------------- |
| `to` | [`str`](../std/str.md)\|[Path](path.md) | Destination path |

**Example:**

```
let src = Path "old.txt"
src.rename "new.txt"
```

### `move to :all?`

Moves this filesystem entry to `to`.

Equivalent to [`move`](index.md#move-from-to-all).

By default this moves a single file or symlink. With `all: true`, it also
moves directories recursively.

**Parameters:**

| Name  | Type                                      | Description                                |
| ----- | ----------------------------------------- | ------------------------------------------ |
| `to`  | [`str`](../std/str.md)\|[Path](path.md)   | Destination path                           |
| `all` | [`bool`](../std/bool.md)                  | If `true`, allows recursive directory move |

**Example:**

```
let src = Path "source.txt"
src.move "dest.txt"

let dir = Path "project"
dir.move "archive/project" all: true
```

### `hard_link to`

Creates a hard link at `to` pointing to this existing file.

Equivalent to [`hard_link`](index.md#hard_link-src-dst).

This uses the platform-native hard-link operation. The source must already
exist, and the link must be created on the same filesystem or volume if the
platform requires it.

**Parameters:**

| Name | Type                                    | Description                         |
| ---- | --------------------------------------- | ----------------------------------- |
| `to` | [`str`](../std/str.md)\|[Path](path.md) | Path where the hard link is created |

**Example:**

```
let src = Path "data.txt"
src.hard_link "data-copy.txt"
```

### `entries()`

Reads the entries in the directory at this path.

**Returns:** Iterable of [DirEntry](direntry.md) objects

**Example:**

```
let dir = Path /home/user/docs

# Iterate over directory entries
for entry = dir.entries()
  echo "$(entry.name) - $(entry.type)"
  echo (dir / entry)

# Collect into an array
let files = [...dir.entries()]
echo "Found $(files.len) entries"
```

### `add_ext ext`

Returns a new path with `ext` appended as an additional extension.

**Parameters:**

| Name  | Type                   | Description                  |
| ----- | ---------------------- | ---------------------------- |
| `ext` | [`str`](../std/str.md) | Extension to append          |

**Returns:** [Path](path.md)

**Example:**

```
let path = Path "archive.tar"
echo path.add_ext "gz"  # archive.tar.gz

let file = Path "report"
echo file.add_ext "txt"  # report.txt
```

### `components()`

Returns an iterator over the lexical components of the path.

**Returns:** Iterator of [`str`](../std/str.md)

```
let path = Path "alpha/beta/gamma"
assert_eq [...path.components()] ["alpha", "beta", "gamma"]

let first ...rest = path.components()
assert_eq $first "alpha"
assert_eq [...rest] ["beta", "gamma"]
```

### `canonical()`

Returns the canonical, absolute form of the path with all intermediate
components normalized and symbolic links resolved.

**Returns:** [Path](path.md)

**Example:**

```
let rel = Path "./foo/../bar"
let abs = rel.canonical()
echo $abs  # Absolute normalized path
```

### `read_link()`

Reads the target of a symbolic link.

**Returns:** [Path](path.md) - The path that the symlink points to

**Errors:** Raises a runtime error if the path is not a symbolic link or cannot
be read.

**Example:**

```
let link = Path "my_link"
let target = link.read_link()
echo "Link points to: $target"
```

### `without_ext()`

Returns a new path with the final extension removed.

**Returns:** [Path](path.md)

```
let path = Path "archive.tar.gz"
echo path.without_ext()  # archive.tar

let plain = Path "Makefile"
echo plain.without_ext()  # Makefile
```

### `with_ext ext`

Returns a new path with the final extension replaced.

**Parameters:**

| Name  | Type                   | Description             |
| ----- | ---------------------- | ----------------------- |
| `ext` | [`str`](../std/str.md) | Replacement extension   |

**Returns:** [Path](path.md)

```
let path = Path "archive.tar.gz"
echo path.with_ext "zip"  # archive.tar.zip

let plain = Path "Makefile"
echo plain.with_ext "txt"  # Makefile.txt
```

### `with_name name`

Returns a new path with the final component replaced.

**Parameters:**

| Name   | Type                   | Description                 |
| ------ | ---------------------- | --------------------------- |
| `name` | [`str`](../std/str.md) | Replacement final component |

**Returns:** [Path](path.md)

```
let path = Path "src/main.rs"
echo path.with_name "lib.rs"  # src/lib.rs
```

### `with_stem stem`

Returns a new path with the final stem replaced, preserving the final
extension when present.

**Parameters:**

| Name   | Type                   | Description         |
| ------ | ---------------------- | ------------------- |
| `stem` | [`str`](../std/str.md) | Replacement stem    |

**Returns:** [Path](path.md)

```
let path = Path "archive.tar.gz"
echo path.with_stem "bundle"  # bundle.gz

let plain = Path "Makefile"
echo plain.with_stem "Dockerfile"  # Dockerfile
```

### `remove :all? :ignore?`

Removes this path.

Equivalent to [`remove`](index.md#remove-path-all-ignore).

By default this removes a single file or symlink. With `all: true`, it also
removes directories recursively, similar to `rm -r`. With `ignore: true`,
missing paths are treated as success.

**Parameters:**

| Name     | Type                     | Description                                |
| -------- | ------------------------ | ------------------------------------------ |
| `all`    | [`bool`](../std/bool.md) | If `true`, removes directories recursively |
| `ignore` | [`bool`](../std/bool.md) | If `true`, ignores a missing path          |

**Example:**

```
let file = Path "temp.txt"
file.remove()

let dir = Path "build"
dir.remove all: true

dir.remove ignore: true
```

### `create_dir :all?`

Creates a directory at this path.

**Parameters:**

| Name  | Type                     | Description                               |
| ----- | ------------------------ | ----------------------------------------- |
| `all` | [`bool`](../std/bool.md) | If `true`, creates parent directories too |

**Example:**

```
let dir = Path "new_subdir"
dir.create_dir()

# Create with parents
let nested = Path "a/b/c"
nested.create_dir all: true
```

### `remove_dir :all? :ignore?`

Removes the directory at this path.

By default this removes only an empty directory. With `all: true`, it removes
directories recursively, but only through subtrees that contain directories and
no files or other non-directory entries. Use
[`remove`](index.md#remove-path-all-ignore) to delete directories that
contain files.

**Parameters:**

| Name     | Type                     | Description                                                      |
| -------- | ------------------------ | ---------------------------------------------------------------- |
| `all`    | [`bool`](../std/bool.md) | If `true`, recursively prunes only empty directory subtrees      |
| `ignore` | [`bool`](../std/bool.md) | If `true`, ignores missing directories and file-blocked subtrees |

**Example:**

```
let dir = Path "empty_dir"
dir.remove_dir()

# Remove an empty directory tree
let to_remove = Path "old_project"
to_remove.remove_dir all: true

to_remove.remove_dir all: true ignore: true
```

### `chmod mode`

Changes the permissions of the file or directory at this path.

**Platform Notes:**

- **Unix:** Changes file permissions using standard Unix mode bits
- **Windows:** Raises a runtime error (not supported)

**Parameters:**

| Name   | Type                   | Description                          |
| ------ | ---------------------- | ------------------------------------ |
| `mode` | [`int`](../std/int.md) | Permission mode bits (e.g., `0o755`) |

**Errors:** Raises a runtime error on non-Unix platforms or if the operation
fails.

**Example:**

```
let script = Path "script.sh"
script.chmod 0o755

let shared = Path "/tmp/shared"
shared.chmod 0o777
```

### `set_timestamps :modified? :accessed? :created?`

Updates the timestamps of the file or directory at this path.

**Platform Notes:**

- **Unix:** `modified` and `accessed` are available; `created` is not supported
- **Windows:** `modified`, `accessed`, and `created` are available

Unspecified timestamps are left unchanged.

**Parameters:**

| Name       | Type                              | Description                               |
| ---------- | --------------------------------- | ----------------------------------------- |
| `modified` | [`DateTime`](../time/datetime.md) | Optional new modification time            |
| `accessed` | [`DateTime`](../time/datetime.md) | Optional new access time                  |
| `created`  | [`DateTime`](../time/datetime.md) | Optional new creation time (Windows only) |

```
import time:
  - DateTime

let artifact = Path "artifact.tar"
artifact.set_timestamps modified: DateTime.from_unix(1700000000)
artifact.set_timestamps accessed: DateTime.now()
artifact.set_timestamps created: DateTime.from_unix(1690000000)
```

### `chown user? :group? :follow = true`

Changes the owner and/or group of the file, directory, or symlink target at
this path.

**Platform Notes:**

- **Unix:** Available
- **Windows:** Not available

**Parameters:**

| Name     | Type                                           | Description                               |
| -------- | ---------------------------------------------- | ----------------------------------------- |
| `user`   | [`int`](../std/int.md)\|[`str`](../std/str.md) | Optional owner UID or user name           |
| `group`  | [`int`](../std/int.md)\|[`str`](../std/str.md) | Optional group GID or group name          |
| `follow` | [`bool`](../std/bool.md)                       | If `false`, operate on the symlink itself |

At least one of `user` or `group` must be provided.

**Example:**

```
let script = Path "script.sh"
script.chown "deploy" group: "deploy"

let shared = Path "/tmp/shared"
shared.chown group: "build"

let link = Path "current"
link.chown group: 33 follow: false
```

### `normalize()`

Returns a normalized path with `.` and `..` components resolved without
accessing the filesystem.

**Returns:** [Path](path.md)

**Example:**

```
let messy = Path "./foo/../bar/./baz"
let clean = messy.normalize()
echo $clean  # bar/baz
```

### `absolute()`

Returns the absolute form of this path based on the current working directory.

If the path is already absolute, it is returned unchanged.

**Returns:** [Path](path.md)

**Example:**

```
let rel = Path "./config.txt"
let abs = rel.absolute()
echo $abs  # /current/working/dir/config.txt

# Already absolute paths are unchanged
let already_abs = Path "/etc/passwd"
echo $already_abs.absolute()  # /etc/passwd
```

### `relative(base?)`

Returns this path relative to a base directory.

**Parameters:**

| Name   | Type                                      | Description                   |
| ------ | ----------------------------------------- | ----------------------------- |
| `base` | [`str`](../std/str.md)\|[`Path`](path.md) | Base directory (default: cwd) |

**Returns:** [Path](path.md) - Relative path, or the original path if it cannot
be made relative

**Example:**

```
# Relative to current directory
let path = Path "/home/user/docs/file.txt"
echo path.relative()  # docs/file.txt (if cwd is /home/user)

# Relative to specific base
let path2 = Path "/a/b/c/d"
echo path2.relative "/a/b"  # c/d

# Returns original if no common prefix
let path3 = Path "/etc/passwd"
echo path3.relative "/home/user"  # /etc/passwd
```

### `glob pattern :max_depth? :follow?`

Returns an iterator over paths matching a glob pattern relative to this path.

**Parameters:**

| Name        | Type                     | Description                                                       |
| ----------- | ------------------------ | ----------------------------------------------------------------- |
| `pattern`   | `str`                    | Glob pattern (e.g., `"*.txt"`, `"**/*.rs"`)                       |
| `max_depth` | [`int`](../std/int.md)   | Maximum directory depth to traverse (default: unlimited)          |
| `follow`    | [`bool`](../std/bool.md) | Whether to follow symbolic links when traversing (default: false) |

**Returns:** Iterable of [Path](path.md) objects

**Example:**

```
let src = Path "src"

# Find all Rust files in src directory
for file = src.glob "*.rs"
  echo "Source: $file"

# Recursive search
for file = src.glob "**/*.rs" max_depth: 2
  echo "Source: $file"

# Follow symlinks
for entry = src.glob "**/*" follow: true
  echo "Entry: $entry"
```
