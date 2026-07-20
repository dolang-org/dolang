# DirEntry

Represent individual entries within a directory. The
[`entries()`](index.md#entries-path) function and the
[`Path.entries()`](path.md#entries) method return iterators over this type.

## Fields

### `name`

Returns the final path component for the entry.

```
for entry = entries .
  echo "Name: $(entry.name)"
```

### `type`

Returns the file type as a [`sym`](../std/sym.md).

Possible values when present:

| Value           | Description                      |
| --------------- | -------------------------------- |
| `:FILE:`        | Regular file                     |
| `:DIR:`         | Directory                        |
| `:SYMLINK:`     | Symbolic link                    |
| `:FIFO:`        | Named pipe (FIFO)                |
| `:CHAR_DEVICE:` | Character device                 |
| `:BLOCK_DEVICE:`| Block device                     |
| `:SOCKET:`      | Unix domain socket               |
| `:UNKNOWN:`     | Type could not be determined     |

```
for entry = entries .
  if entry.type == :DIR:
    echo "Directory: $(entry.name)"
  else if entry.type == :FILE:
    echo "File: $(entry.name)"
  else if entry.type == :UNKNOWN:
    echo "Unknown type: $(entry.name)"
```

## Platform-Specific Fields

### Unix-Only Fields

The following fields are only available on Unix systems:

#### `ino`

The inode number of the file system entry.

```
for entry = entries .
  echo "$(entry.name) has inode $(entry.ino)"
```

## Usage Examples

### Basic Iteration

```
for entry = entries /var/log
  echo "$(entry.name): $(entry.type)"
```

### Collecting to Array

```
# Get all entries as an array
let all_entries = [...entries "."]
echo "Found $(all_entries.len) entries"
```

### Working with Paths

```
let dir = Path ./src
for entry = dir.entries()
  let full_path = (dir / entry)
  echo "Path: $full_path"

  if full_path.exists()
    echo "  Confirmed: exists"
```

## Operators

### `/`

`path / entry` returns the derived [`Path`](path.md) for that directory entry.

### Platform-Specific Handling

```
for entry = entries .
  echo "Name: $(entry.name)"

  if (sys.os_info().family != :WINDOWS:)
    echo "Type: $(entry.type)"

    # Inode is Unix-only
    echo "Inode: $(entry.ino)"
```
