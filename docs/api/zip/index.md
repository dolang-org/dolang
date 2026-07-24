# zip

The `zip` module provides functions and types for working with ZIP archives.
The API is VFS-transparent: archive paths use the active VFS context.

## Functions

### `open path mode? func?`

Opens a ZIP archive and returns an Archive object.

#### Parameters

| Name   | Type                   | Description                                             |
| ------ | ---------------------- | ------------------------------------------------------- |
| `path` | [`str`](../std/str.md) | Path to the ZIP archive file                            |
| `mode` | `str`                  | Access mode (default: `"r"`)                            |
| `func` | func                   | Callable to run with the archive; auto-closes when done |

**Archive modes:**

| Mode   | Description                                           |
| ------ | ----------------------------------------------------- |
| `"r"`  | Read mode - opens existing archive for reading        |
| `"w"`  | Write mode - creates new archive (truncates existing) |

#### Returns

[Archive](archive.md) when no func is provided, otherwise the
result of calling `func`

#### Example

```
# Read an existing archive
open "archive.zip" do |archive|
  for entry = archive.entries
    echo "Entry: $entry.name"

# Create a new archive
open "output.zip" "w" do |archive|
  archive.open "file.txt" do |file|
    file.write "Hello, World!"
```

## Types

- [Archive](./archive.md) - Represents a ZIP archive
- [File](./file.md) - Represents a file within a ZIP archive
- [Entry](./entry.md) - A single archive entry's metadata, and a handle to
  open it for reading
