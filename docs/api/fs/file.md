# File

File objects are returned by [`open`](./index.md#open-path-mode-func) and
provide methods for file operations. All operations on closed files raise a
runtime error.

## Inherits

- [`Iter`](../std/iter.md)
- [`Sink`](../std/sink.md)

## Methods

### `write data`

Writes data to the file.

#### Parameters

| Name   | Type         | Description                                       |
| ------ | ------------ | ------------------------------------------------- |
| `data` | `Str`\|`Bin` | Data to write. Strings are written as UTF-8 text. |

#### Returns

[`Int`](../std/index.md) (number of bytes written)

#### Example

```
open output.txt w do |file|
  let bytes_written = file.write "Hello, World!"
  echo "Wrote $bytes_written bytes"

  # Write binary data
  let binary = b"Hello"
  file.write binary
```

### `set_size size`

Truncates the file to the given byte length.

If the file has buffered unread data, the logical cursor position is preserved
after truncation.

#### Parameters

| Name   | Type                     | Description                 |
| ------ | ------------------------ | --------------------------- |
| `size` | [`Int`](../std/index.md) | New file length in bytes    |

#### Example

```
open data.bin r+ do |file|
  file.set_size 8
```

### `lock range :shared? func`

Acquires a byte-range lock while `func` runs.

**Parameters:**

| Name     | Type                       | Description                                    |
| -------- | -------------------------- | ---------------------------------------------- |
| `range`  | [`Range`](../std/range.md) | Half-open byte range; `..` is total            |
| `shared` | [`Bool`](../std/bool.md)?  | Acquire a shared lock                          |
| `func`   | [`Func`](../std/func.md)   | Block receiving a [`FileLock`](./file-lock.md) |

**Returns:** the block's result

The lock is exclusive unless `shared` is true. It is released before the
method returns, including when the block raises an error or is canceled.
Release runs with interruption masked and may wait indefinitely.

Locks are mandatory on Windows, where they prevent conflicting file access.
On Unix they are advisory and affect only programs that cooperate through file
locking.

Native blocking acquisition cannot be canceled. Canceling the strand may
leave a blocking worker waiting for a conflicting lock and delay shutdown.
Use `try_lock` with async retry for bounded or cancellable waiting.

Overlapping active lock ranges on the same `File` are unsupported, including
identical shared ranges.

Finite zero-length ranges are invalid on Unix. On Windows they conflict only
with positive-length ranges that start before and end after their offset. They
do not conflict with another zero-length range or a range starting at the same
offset. The zero-length range `0..0` is invalid.

```
file.lock (0..128) do |lock|
  update_header()
```

### `try_lock range :shared? func`

Attempts to acquire a byte-range lock without waiting.

**Parameters:**

| Name     | Type                       | Description                                    |
| -------- | -------------------------- | ---------------------------------------------- |
| `range`  | [`Range`](../std/range.md) | Half-open byte range; `..` is total            |
| `shared` | [`Bool`](../std/bool.md)?  | Acquire a shared lock                          |
| `func`   | [`Func`](../std/func.md)   | Block receiving a [`FileLock`](./file-lock.md) |

**Returns:** the block's result

The block always runs. `lock.held` is false when another handle holds a
conflicting lock. Other acquisition failures raise errors.

```
file.try_lock (..) do |lock|
  if lock.held
    update_index()
```

### `read :size?`

Reads data from the file.

#### Parameters

| Name   | Type                     | Description                                                              |
| ------ | ------------------------ | ------------------------------------------------------------------------ |
| `size` | [`Int`](../std/index.md) | Number of bytes to read. If [`nil`](../std/index.md), reads entire file. |

#### Returns

[`Str`](../std/str.md) in text mode, binary blob in binary
mode

#### Example

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

#### Returns

[`Metadata`](metadata.md)

**Fields:**

| Field  | Type                     | Description                                                                                                        |
| ------ | ------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `len`  | [`Int`](../std/index.md) | File size in bytes                                                                                                 |
| `type` | [`Sym`](../std/sym.md)   | File type: `:FILE:`, `:DIR:`, `:SYMLINK:`, `:FIFO:`, `:CHAR_DEVICE:`, `:BLOCK_DEVICE:`, `:SOCKET:`, or `:UNKNOWN:` |

**Optional timestamps** (platform-dependent):

| Field      | Type                              | Description            |
| ---------- | --------------------------------- | ---------------------- |
| `modified` | [`DateTime`](../time/datetime.md) | Last modification time |
| `accessed` | [`DateTime`](../time/datetime.md) | Last access time       |
| `created`  | [`DateTime`](../time/datetime.md) | Creation/change time   |

**Unix-only** (these fields do not exist on Windows):

| Field     | Type                     | Description                           |
| --------- | ------------------------ | ------------------------------------- |
| `mode`    | [`Int`](../std/index.md) | File permissions and type (stat mode) |
| `dev`     | [`Int`](../std/index.md) | Device ID                             |
| `ino`     | [`Int`](../std/index.md) | Inode number                          |
| `nlink`   | [`Int`](../std/index.md) | Number of hard links                  |
| `uid`     | [`Int`](../std/index.md) | User ID of owner                      |
| `gid`     | [`Int`](../std/index.md) | Group ID of owner                     |
| `rdev`    | [`Int`](../std/index.md) | Device ID (if special file)           |
| `blksize` | [`Int`](../std/index.md) | Preferred block size for I/O          |
| `blocks`  | [`Int`](../std/index.md) | Number of 512-byte blocks allocated   |

**Windows-only** (these fields do not exist on Unix):

| Field       | Type                     | Description                           |
| ----------- | ------------------------ | ------------------------------------- |
| `win_attrs` | [`Int`](../std/index.md) | Raw Windows file attribute bitmask    |

#### Example

```
open data.txt r do |file|
  let meta = file.metadata()
  echo "Size: $(meta.size)"
  echo "Type: $(meta.type)"
  echo "Modified: $(meta.modified)"
  echo "Modified seconds: $(meta.modified.unix_secs)"

  if (sys.os_info().family != :WINDOWS:)
    echo "Mode: $(meta.mode)"
    echo "Owner: UID=$(meta.uid), GID=$(meta.gid)"
  else
    echo "Attributes: $(meta.win_attrs)"
```

### `fs_metadata()`

Gets filesystem metadata for the filesystem backing this open file.

#### Returns

[`FsMetadata`](fs-metadata.md)

#### Example

```
open data.txt r do |file|
  let meta = file.fs_metadata()
  echo "Capacity: $(meta.capacity)"
  echo "Available: $(meta.available)"
```

### `sec_desc :owner = true :group = true :dacl = true :sacl = false`

Gets selected parts of the Windows security descriptor through this file's
existing handle.

**Parameters:**

| Name    | Type                     | Description                |
| ------- | ------------------------ | -------------------------- |
| `owner` | [`Bool`](../std/bool.md) | Load the owner SID         |
| `group` | [`Bool`](../std/bool.md) | Load the primary group SID |
| `dacl`  | [`Bool`](../std/bool.md) | Load the discretionary ACL |
| `sacl`  | [`Bool`](../std/bool.md) | Load the system ACL        |

**Returns:** [`security.windows.SecDesc`](../security/windows/secdesc.md)

The operation raises a permission error if the file was opened without the
necessary Windows access rights. Other platforms raise `UnsupportedError`.

### `set_sec_desc desc`

Applies the components selected by a security descriptor's `mask` through
this file's existing handle.

**Parameters:**

| Name   | Type                                                         | Description                  |
| ------ | ------------------------------------------------------------ | ---------------------------- |
| `desc` | [`security.windows.SecDesc`](../security/windows/secdesc.md) | Security descriptor to apply |

The operation raises a permission error if the file was opened without the
necessary Windows access rights. Windows may normalize the resulting
descriptor. Other platforms raise `UnsupportedError`.

### `acl :default?`

Gets the POSIX.1e ACL stored on the open file.

**Parameters:**

| Name      | Type                      | Description                                   |
| --------- | ------------------------- | --------------------------------------------- |
| `default` | [`Bool`](../std/bool.md)? | Query the directory's inheritable default ACL |

**Returns:** [`security.unix.Acl`](../security/unix/acl.md), or `nil`

### `set_acl acl :default?`

Sets or removes a POSIX.1e ACL on the open file.

**Parameters:**

| Name      | Type                                                                   | Description                                    |
| --------- | ---------------------------------------------------------------------- | ---------------------------------------------- |
| `acl`     | [`security.unix.Acl`](../security/unix/acl.md)\|[`nil`](../std/nil.md) | ACL to set, or `nil` to remove it              |
| `default` | [`Bool`](../std/bool.md)?                                              | Update the directory's inheritable default ACL |

### `xattrs :namespace?`

Lists extended attributes for this file.

On Windows, this uses NTFS extended attributes. Returned names may differ in
case from the requested name.

#### Parameters

| Name        | Type                                            | Description                                                                                              |
| ----------- | ----------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)? | Namespace to query; `:USER:` and `:SYSTEM:` name well-known namespaces, and `:ANY:` lists all namespaces |

#### Returns

iterator of [`XattrEntry`](xattr-entry.md)

```
open data.txt r do |file|
  for attr = file.xattrs()
    echo $attr.name
```

### `streams`

Lists alternate data streams for this file.

This is only supported on Windows.

#### Returns

iterator of [`fs.windows.StreamEntry`](windows/stream-entry.md)

```
let path = Path data.txt
open $path r do |file|
  for stream = file.streams()
    echo "$(stream.name) $(stream.type)"
    echo (path / stream)
```

### `xattr name :namespace?`

Gets an extended attribute value.

#### Parameters

| Name        | Type                                                   | Description                           |
| ----------- | ------------------------------------------------------ | ------------------------------------- |
| `name`      | [`Str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs` |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)?        | Namespace to query                    |

#### Returns

[`Bin`](../std/bin.md)

```
open data.txt r do |file|
  let value = file.xattr "comment"
```

### `set_xattr name value :namespace?`

Sets an extended attribute value.

On Windows, empty values are rejected. NTFS deletes the attribute instead of
storing an empty value.

#### Parameters

| Name        | Type                                                   | Description                           |
| ----------- | ------------------------------------------------------ | ------------------------------------- |
| `name`      | [`Str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs` |
| `value`     | [`Str`](../std/str.md)\|[`Bin`](../std/bin.md)         | Attribute bytes; strings use UTF-8    |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)?        | Namespace to update                   |

```
open data.txt r+ do |file|
  file.set_xattr "comment" "ready"
```

### `remove_xattr name :namespace?`

Removes an extended attribute.

#### Parameters

| Name        | Type                                                   | Description                           |
| ----------- | ------------------------------------------------------ | ------------------------------------- |
| `name`      | [`Str`](../std/str.md)\|[`XattrEntry`](xattr-entry.md) | Attribute name or entry from `xattrs` |
| `namespace` | [`Str`](../std/str.md)\|[`Sym`](../std/sym.md)?        | Namespace to update                   |

```
open data.txt r+ do |file|
  file.remove_xattr "comment"
```

### `seek offset`

Moves the file cursor by a relative byte offset.

Buffered unread data is discarded before the seek so subsequent reads use the
new cursor position.

#### Parameters

| Name     | Type                     | Description                       |
| -------- | ------------------------ | --------------------------------- |
| `offset` | [`Int`](../std/index.md) | Relative byte offset from current |

#### Returns

[`Int`](../std/index.md) - New absolute byte position

#### Example

```
open data.bin rb do |file|
  file.seek 10
  file.seek (0 - 4)
```

### `seek start: ofs`

Moves the file cursor to an absolute byte offset from the start of the file.

Buffered unread data is discarded before the seek so subsequent reads use the
new cursor position.

#### Parameters

| Name  | Type                     | Description                          |
| ----- | ------------------------ | ------------------------------------ |
| `ofs` | [`Int`](../std/index.md) | Absolute byte offset from file start |

#### Returns

[`Int`](../std/index.md) - New absolute byte position

#### Example

```
open data.bin rb do |file|
  file.seek start: 10
  let pos = file.tell()
```

### `seek end: ofs`

Moves the file cursor to a byte offset relative to the end of the file.

Buffered unread data is discarded before the seek so subsequent reads use the
new cursor position.

#### Parameters

| Name  | Type                     | Description                      |
| ----- | ------------------------ | -------------------------------- |
| `ofs` | [`Int`](../std/index.md) | Byte offset relative to file end |

#### Returns

[`Int`](../std/index.md) - New absolute byte position

#### Example

```
open data.bin rb do |file|
  file.seek end: (0 - 1)
```

### `tell()`

Returns the current file cursor position in bytes.

#### Returns

[`Int`](../std/index.md)

#### Example

```
open data.txt r do |file|
  assert_eq (file.tell()) 0
  file.read 5
  assert_eq (file.tell()) 5
```

### `close()`

Explicitly closes the file. Required if you didn't use the `func` parameter to
`open()`.

#### Example

```
let file = open data.txt r
let data = file.read()
file.close()
```

## Iterator and Sink Protocols

Files implement the iterator and sink protocols, allowing them to be used
with `for` loops, `.next()`, `.put()`, and `strand.redirect`.

### `input`/`output`

Returns the file as its own iterator and sink.

#### Returns

The file object itself

### `next`

Fetches the next item from the file.

**Text mode:** Reads the next line (delimited by `\n`), stripping the line
ending. Handles both `\n` and `\r\n` line endings.

**Binary mode:** Reads a chunk of data of arbitrary length.

### `put`

Writes a value to the file.

**Text mode:**

- If the value is binary data (`Bin`), writes it unmodified
- Otherwise, converts to string and appends the line ending for the VFS target
  on which the file handle was opened

**Binary mode:** Writes bytes directly.
