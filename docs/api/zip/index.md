# zip

The `zip` module provides functions and types for working with ZIP archives.

## Functions

### `open path mode? func?`

Opens a ZIP archive and returns an Archive object.

**Parameters:**

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
| `"a"`  | Append mode - opens existing archive for appending    |

**Returns:** [Archive](archive.md) when no func is provided, otherwise the
result of calling `func`

**Example:**

```
# Read an existing archive
open "archive.zip" do |archive|
  for name = archive.entries()
    echo "Entry: $name"

# Create a new archive
open "output.zip" "w" do |archive|
  archive.open "file.txt" do |file|
    file.write "Hello, World!"

# Append to existing archive
open "existing.zip" "a" do |archive|
  archive.open "new.txt" do |file|
    file.write "Appended content"
```

## Types

- [Archive](./archive.md) - Represents a ZIP archive
- [File](./file.md) - Represents a file within a ZIP archive
- [EntryIter](./entryiter.md) - Iterator over archive entries
