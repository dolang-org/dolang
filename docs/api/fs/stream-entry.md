# StreamEntry

Alternate data stream entry returned by
[`streams`](index.md).

This is only supported on Windows.

## Fields

### `name`

Stream name. The unnamed default stream is reported as `""`.

```
for stream = streams "data.txt"
  echo $stream.name
```

### `type`

Stream type without the leading `$`.

```
for stream = streams "data.txt"
  echo $stream.type
```

### `size`

Logical stream size in bytes.

### `alloc_size`

Allocated stream size in bytes.

## Operators

```
for stream = streams "data.txt"
  echo (Path "data.txt" / stream)
```

### `/`

`path / stream` returns the stream-qualified [`Path`](path.md) for that entry.
