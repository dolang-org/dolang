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

### `set_len size`

Truncates the file to the given byte length.

If the file has buffered unread data, the logical cursor position is preserved
after truncation.

**Parameters:**

| Name   | Type                     | Description                 |
| ------ | ------------------------ | --------------------------- |
| `size` | [`int`](../std/index.md) | New file length in bytes    |

**Example:**

```
open data.bin r+ do |file|
  file.set_len 8
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

Gets file metadata.

**Returns:** [`Metadata`](metadata.md)

**Fields:**

| Field  | Type                     | Description                                                                                                        |
| ------ | ------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `len`  | [`int`](../std/index.md) | File size in bytes                                                                                                 |
| `type` | [`sym`](../std/sym.md)   | File type: `:file:`, `:dir:`, `:symlink:`, `:fifo:`, `:char_device:`, `:block_device:`, `:socket:`, or `:unknown:` |

**Optional timestamps** (platform-dependent):

| Field      | Type                              | Description            |
| ---------- | --------------------------------- | ---------------------- |
| `modified` | [`DateTime`](../time/datetime.md) | Last modification time |
| `accessed` | [`DateTime`](../time/datetime.md) | Last access time       |
| `created`  | [`DateTime`](../time/datetime.md) | Creation/change time   |

**Unix-only** (these fields do not exist on Windows):

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

**Windows-only** (these fields do not exist on Unix):

| Field       | Type                     | Description                           |
| ----------- | ------------------------ | ------------------------------------- |
| `win_attrs` | [`int`](../std/index.md) | Raw Windows file attribute bitmask    |
| `attrs`     | [`Attrs`](attrs.md)      | Windows attributes from this metadata |

**Example:**

```
open data.txt r do |file|
  let meta = file.metadata()
  echo "Size: $(meta.len)"
  echo "Type: $(meta.type)"
  echo "Modified: $(meta.modified)"
  echo "Modified seconds: $(meta.modified.unix_secs)"

  if (sys.os_info().family != :windows:)
    echo "Mode: $(meta.mode)"
    echo "Owner: UID=$(meta.uid), GID=$(meta.gid)"
  else
    echo "Attributes: $(meta.attrs.win_attrs)"
```

### `seek offset`

Moves the file cursor by a relative byte offset.

Buffered unread data is discarded before the seek so subsequent reads use the
new cursor position.

**Parameters:**

| Name     | Type                     | Description                       |
| -------- | ------------------------ | --------------------------------- |
| `offset` | [`int`](../std/index.md) | Relative byte offset from current |

**Returns:** [`int`](../std/index.md) - New absolute byte position

**Example:**

```
open data.bin rb do |file|
  file.seek 10
  file.seek (0 - 4)
```

### `seek start: ofs`

Moves the file cursor to an absolute byte offset from the start of the file.

Buffered unread data is discarded before the seek so subsequent reads use the
new cursor position.

**Parameters:**

| Name  | Type                     | Description                          |
| ----- | ------------------------ | ------------------------------------ |
| `ofs` | [`int`](../std/index.md) | Absolute byte offset from file start |

**Returns:** [`int`](../std/index.md) - New absolute byte position

**Example:**

```
open data.bin rb do |file|
  file.seek start: 10
  let pos = file.tell()
```

### `seek end: ofs`

Moves the file cursor to a byte offset relative to the end of the file.

Buffered unread data is discarded before the seek so subsequent reads use the
new cursor position.

**Parameters:**

| Name  | Type                     | Description                      |
| ----- | ------------------------ | -------------------------------- |
| `ofs` | [`int`](../std/index.md) | Byte offset relative to file end |

**Returns:** [`int`](../std/index.md) - New absolute byte position

**Example:**

```
open data.bin rb do |file|
  file.seek end: (0 - 1)
```

### `tell()`

Returns the current file cursor position in bytes.

**Returns:** [`int`](../std/index.md)

**Example:**

```
open data.txt r do |file|
  assert_eq (file.tell()) 0
  file.read 5
  assert_eq (file.tell()) 5
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
