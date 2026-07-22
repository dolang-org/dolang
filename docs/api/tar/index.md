# tar

Streams TAR archives with optional gzip or zstd compression.

## Types

| Type                                    | Description                         |
| --------------------------------------- | ----------------------------------- |
| [`Reader`](./tar-reader.md)             | Destructive archive entry iterator  |
| [`Entry`](./tar-entry.md)               | Metadata and content for one entry  |
| [`Writer`](./tar-writer.md)             | Sequential archive writer           |
| [`EntryWriter`](./entry-writer.md)      | Scoped entry content sink           |

## Functions

### `read path func`

Opens an archive and calls `func` with a [`Reader`](./tar-reader.md).
Compression is detected from gzip or zstd magic bytes.

**Parameters:**

| Name   | Type                                            | Description  |
| ------ | ----------------------------------------------- | ------------ |
| `path` | [`str`](../std/str.md)\|[`Path`](../fs/path.md) | Archive path |
| `func` | callable                                        | Reader scope |

**Returns:** the result of `func`.

```
read "archive.tar.gz" do |archive|
  for entry = archive
    echo "$entry.path: $entry.size bytes"
```

### `write path :compression? func`

Creates an archive and calls `func` with a [`Writer`](./tar-writer.md).

**Parameters:**

| Name          | Type                                            | Description                          |
| ------------- | ----------------------------------------------- | ------------------------------------ |
| `path`        | [`str`](../std/str.md)\|[`Path`](../fs/path.md) | Archive path                         |
| `compression` | [`sym`](../std/sym.md)?                         | `:NONE:`, `:GZIP:`, or `:ZSTD:`      |
| `func`        | callable                                        | Writer scope                         |

When `compression` is omitted, `.gz` and `.tgz` select gzip, `.zst` and
`.tzst` select zstd, and other extensions select no compression. Extension
matching is case-insensitive. An explicit `compression` overrides the path.

**Returns:** the result of `func`.

```
write "archive.tar.zst" do |archive|
  archive.entry greeting.txt size: 5 do |entry|
    entry.write hello
```
