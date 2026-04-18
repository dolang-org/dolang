# File

File objects are returned by [`open`](./index.md#open-path-mode-func) and
provide methods for file operations. All operations on closed files raise a
runtime error.

## Methods

### `write data`

Writes data to the file.

**Parameters:**

| Name   | Type         | Description                                       |
| ------ | ------------ | ------------------------------------------------- |
| `data` | `str`\|`bin` | Data to write. Strings are written as UTF-8 text. |

**Returns:** [`int`](../std/index.md) (number of bytes written)

**Example:**

```
open output.txt w do |file|
  let bytes_written = file.write "Hello, World!"
  echo "Wrote $bytes_written bytes"

  # Write binary data
  let binary = b"Hello"
  file.write binary
```

### `read :size?`

Reads data from the file.

**Parameters:**

| Name   | Type                     | Description                                                              |
| ------ | ------------------------ | ------------------------------------------------------------------------ |
| `size` | [`int`](../std/index.md) | Number of bytes to read. If [`nil`](../std/index.md), reads entire file. |

**Returns:** [`str`](../std/str.md) in text mode, binary blob in binary
mode

**Example:**

```
# Read entire file
open input.txt r do |file|
  let content = file.read()
  echo "File contents: $content"

# Read specific number of bytes
open data.bin rb do |file|
  let header = file.read 4
  let rest = file.read()
```

### `metadata()`

Gets file metadata as a record.

**Returns:** Record with the following fields:

**Always present:**

| Field  | Type                     | Description                                               |
| ------ | ------------------------ | --------------------------------------------------------- |
| `len`  | [`int`](../std/index.md) | File size in bytes                                        |
| `type` | [`sym`](../std/sym.md)   | File type: `:file:`, `:dir:`, `:symlink:`, or `:unknown:` |

**Optional timestamps** (platform-dependent):

| Field      | Type                              | Description            |
| ---------- | --------------------------------- | ---------------------- |
| `modified` | [`DateTime`](../time/datetime.md) | Last modification time |
| `accessed` | [`DateTime`](../time/datetime.md) | Last access time       |
| `created`  | [`DateTime`](../time/datetime.md) | Creation/change time   |

**Unix-only** (not available on Windows):

| Field     | Type                     | Description                           |
| --------- | ------------------------ | ------------------------------------- |
| `mode`    | [`int`](../std/index.md) | File permissions and type (stat mode) |
| `dev`     | [`int`](../std/index.md) | Device ID                             |
| `ino`     | [`int`](../std/index.md) | Inode number                          |
| `nlink`   | [`int`](../std/index.md) | Number of hard links                  |
| `uid`     | [`int`](../std/index.md) | User ID of owner                      |
| `gid`     | [`int`](../std/index.md) | Group ID of owner                     |
| `rdev`    | [`int`](../std/index.md) | Device ID (if special file)           |
| `blksize` | [`int`](../std/index.md) | Preferred block size for I/O          |
| `blocks`  | [`int`](../std/index.md) | Number of 512-byte blocks allocated   |

**Example:**

```
open data.txt r do |file|
  let meta = file.metadata()
  echo "Size: $(meta.len)"
  echo "Type: $(meta.type)"
  echo "Modified: $(meta.modified)"
  echo "Modified seconds: $(meta.modified.seconds)"

  # Unix-specific metadata
  if (meta.mode != nil)
    echo "Mode: $(meta.mode)"
    echo "Owner: UID=$(meta.uid), GID=$(meta.gid)"
```

### `close()`

Explicitly closes the file. Required if you didn't use the `func` parameter to
`open()`.

**Example:**

```
let file = open data.txt r
let data = file.read()
file.close()
```

## Input/Output Iteration

Files implement the input/output iterator protocols, allowing them to be used
with `for` loops, `.next()`, `.put()`, and `strand.redirect`.

### `input`/`output`

Returns the file as its own input/output iterator.

**Returns:** The file object itself

### `next`

Fetches the next item from the file.

**Text mode:** Reads the next line (delimited by `\n`), stripping the line
ending. Handles both `\n` and `\r\n` line endings.

**Binary mode:** Reads a chunk of data of arbitrary length.

### `put`

Writes a value to the file.

**Text mode:**

- If the value is binary data (`bin`), writes it unmodified
- Otherwise, converts to string and appends `\n`

**Binary mode:** Writes bytes directly.
