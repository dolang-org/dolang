# Archive

Archive objects are returned by [`open()`](./index.md#open-path-mode-func)
and provide methods for working with ZIP archives.

## Fields

### `entries`

Immutable array-like view of [`Entry`](./entry.md) objects for every entry in
the archive.

**Availability:** Read mode only

#### Example

```
open "archive.zip" do |archive|
  for entry = archive.entries
    echo "Found: $entry.name"

  # Or collect into an array
  let entries = [...archive.entries]
  echo "Total files: $(entries.len)"
```

**Error:** Raises a runtime error if accessed on an archive opened in write
mode. A view obtained before the archive is closed reports an empty length
afterward rather than erroring, since `len` cannot itself raise an error.

## Methods

### `open name :mode? func?`

Opens a file within the archive. Always creates a regular file entry in
write mode — use [`create_dir`](#create_dir-name-mode) or
[`symlink`](#symlink-target-name-mode) for directory or symlink entries,
which carry no content and so have no use for a write handle.

#### Parameters

| Name   | Type                    | Description                                          |
| ------ | ----------------------- | ---------------------------------------------------- |
| `name` | [`str`](../std/str.md)  | Name/path of the file within the archive             |
| `mode` | [`int`](../std/int.md)? | Unix permission bits in write mode (default: `0`)    |
| `func` | func                    | Callable to run with the file; auto-closes when done |

**Mode-specific behavior:**

- **Read mode:** Opens an existing file for reading
- **Write mode:** Creates a new file entry for writing

#### Returns

[File](./file.md) when no func is provided, otherwise the result of
calling `func`

#### Example

```
open "archive.zip" do |archive|
  # Read mode - open existing file
  archive.open "document.txt" do |file|
    let content = file.read 1024
    echo content

# Write mode - create new file
open "output.zip" "w" do |archive|
  archive.open "data.txt" mode: 0o644 do |file|
    file.write "Hello, World!"
```

### `create_dir name :mode?`

Creates a directory entry. Write mode only.

#### Parameters

| Name   | Type                    | Description                                                                        |
| ------ | ----------------------- | ---------------------------------------------------------------------------------- |
| `name` | [`str`](../std/str.md)  | Name/path of the directory within the archive (a trailing `/` is added if missing) |
| `mode` | [`int`](../std/int.md)? | Unix permission bits (default: `0`)                                                |

#### Example

```
open "output.zip" "w" do |archive|
  archive.create_dir "subdir" mode: 0o755
```

### `symlink target name :mode?`

Creates a symbolic link entry pointing to `target`. Write mode only.
Argument order matches [`fs.symlink_file`](../fs/index.md#symlink_file-src-dst).

#### Parameters

| Name     | Type                    | Description                                 |
| -------- | ----------------------- | ------------------------------------------- |
| `target` | [`str`](../std/str.md)  | Path the symlink points to                  |
| `name`   | [`str`](../std/str.md)  | Name/path of the symlink within the archive |
| `mode`   | [`int`](../std/int.md)? | Unix permission bits (default: `0`)         |

#### Example

```
open "output.zip" "w" do |archive|
  archive.symlink "target.txt" "link.txt" mode: 0o777
```

### `close()`

Closes the archive and releases resources.

**Mode-specific behavior:**

- **Read mode:** Simply closes the archive
- **Write mode:** Finalizes the archive (writes central directory) before
  closing

#### Example

```
let archive = open "data.zip"
# ... use archive ...
archive.close()
```

## Usage Notes

### Concurrent Access

Only one file can be open at a time within an archive. Attempting to open a
second file while another is open will raise a concurrency error.
