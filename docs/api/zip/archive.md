# Archive

Archive objects are returned by [`open()`](./index.md#open-path-mode-func)
and provide methods for working with ZIP archives.

## Methods

### `open name func?`

Opens a file within the archive.

#### Parameters

| Name   | Type                   | Description                                          |
| ------ | ---------------------- | ---------------------------------------------------- |
| `name` | [`str`](../std/str.md) | Name/path of the file within the archive             |
| `func` | func                   | Callable to run with the file; auto-closes when done |

**Mode-specific behavior:**

- **Read mode:** Opens an existing file for reading
- **Write/Append mode:** Creates a new file entry for writing

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
  archive.open "data.txt" do |file|
    file.write "Hello, World!"
```

### `entries()`

Returns an iterator over the names of all entries in the archive.

**Availability:** Read mode only

#### Returns

[EntryIter](entryiter.md)

#### Example

```
open "archive.zip" do |archive|
  for name = archive.entries()
    echo "Found: $name"

  # Or collect into an array
  let names = [...archive.entries()]
  echo "Total files: $(names.len)"
```

**Error:** Raises a runtime error if called on an archive opened in write or
append mode.

### `close()`

Closes the archive and releases resources.

**Mode-specific behavior:**

- **Read mode:** Simply closes the archive
- **Write/Append mode:** Finalizes the archive (writes central directory) before
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
