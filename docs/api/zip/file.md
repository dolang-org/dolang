# File

File objects are returned by [`Archive.open()`](./archive.md#open-name-func) and
provide methods for reading from or writing to files within a ZIP archive.

## Methods

### `read size`

Reads data from the file.

**Availability:** Read mode only

#### Parameters

| Name   | Type                   | Description                     |
| ------ | ---------------------- | ------------------------------- |
| `size` | [`int`](../std/int.md) | Maximum number of bytes to read |

#### Returns

[bin](../std/bin.md)

#### Example

```
open "archive.zip" do |archive|
  archive.open "document.txt" do |file|
    let data = file.read 4096
    echo "Read $(data.len) bytes"

    # Convert to string if text
    let text = str data
    echo text
```

**Error:** Raises a runtime error if called on a file opened in write or append
mode.

### `write data`

Writes data to the file.

**Availability:** Write/Append mode only

#### Parameters

| Name   | Type                                             | Description   |
| ------ | ------------------------------------------------ | ------------- |
| `data` | [`bin`](../std/bin.md) or [`str`](../std/str.md) | Data to write |

#### Example

```
open "output.zip" "w" do |archive|
  archive.open "data.bin" do |file|
    file.write "Hello, World!"
    file.write b"\x00\x01\x02\x03"
```

**Error:** Raises a runtime error if called on a file opened in read mode.

### `close()`

Closes the file.

#### Example

```
open "archive.zip" do |archive|
  let file = archive.open "data.txt"
  let content = file.read 1024
  file.close()
```
