# DirEntry

DirEntry objects represent individual entries within a directory. They are
returned by the [`entries()`](index.md#entries-path) function and the
[`Path.entries()`](path.md#entries) method.

DirEntry objects are records with information about a directory entry, providing
access to the entry's path, name, and file type.

## Creating DirEntry Objects

DirEntry objects are created by iterating over directory entries:

```
# Iterate and get DirEntry objects
for entry = entries /home/user/docs
  echo "$(entry.name) - $(entry.type)"
```

## Fields

### `path`

Returns the full path to the directory entry as a [Path](path.md) object.

```
for entry = entries .
  echo "Full path: $(entry.path)"
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

### `type`

Returns the file type as a [`sym`](../std/sym.md), or
[`nil`](../std/index.md) if the type cannot be determined.

Possible values when present:

| Value           | Description                      |
| --------------- | -------------------------------- |
| `:file:`        | Regular file                     |
| `:dir:`         | Directory                        |
| `:symlink:`     | Symbolic link                    |
| `:fifo:`        | Named pipe (FIFO)                |
| `:char_device:` | Character device                 |
| `:block_device:`| Block device                     |
| `:socket:`      | Unix domain socket               |

```
for entry = entries .
  if entry.type == :dir:
    echo "Directory: $(entry.name)"
  else if entry.type == :file:
    echo "File: $(entry.name)"
  else if entry.type == nil:
    echo "Unknown type: $(entry.name)"
```

## Usage Examples

### Basic Iteration

```
for entry = entries /var/log
  let ty = (entry.type || :unknown:)
  echo "$(entry.name): $ty"
```

### Collecting to Array

```
# Get all entries as an array
let all_entries = [...entries "."]
echo "Found $(all_entries.len) entries"
```

### Working with Paths

```
for entry = entries ./src
  # entry.path is a Path object
  let full_path = entry.path
  echo "Path: $full_path"

  # Can use Path methods
  if full_path.exists()
    echo "  Confirmed: exists"
```

### Platform-Specific Handling

```
for entry = entries .
  echo "Name: $(entry.name)"

  if (sys.os_info().family != :windows:)
    # Type is Unix-only and may be nil
    if (entry.type != nil)
      echo "Type: $(entry.type)"
    else
      echo "Type: unavailable"

    # Inode is Unix-only
    echo "Inode: $(entry.ino)"
```
