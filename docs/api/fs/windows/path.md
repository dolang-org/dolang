# Path

[`fs.Path`](../path.md) using Windows path syntax.

## Constructor

### `Path path`

**Parameters:**

| Name   | Type                                               | Description |
| ------ | -------------------------------------------------- | ----------- |
| `path` | [`Str`](../../std/str.md)\|[`fs.Path`](../path.md) | Path value  |

**Returns:** `Path`.

Converting a Unix path is allowed only when it is relative.

See [`fs.Path`](../path.md) for shared fields, methods, and operators.

## Fields

### `disk`

Drive letter for `C:`-style and `\\?\C:`-style prefixes, or `nil` otherwise.

```
let path = Path "C:/work/file.txt"
echo $path.disk  # C
```

### `server`

UNC server name, or `nil` if the path does not use a UNC prefix.

```
let path = Path "//server/share/file.txt"
echo $path.server  # server
```

### `share`

UNC share name, or `nil` if the path does not use a UNC prefix.

```
let path = Path "//server/share/file.txt"
echo $path.share  # share
```

### `device`

Device namespace name for `\\.\name` paths, or `nil` otherwise.

```
let path = Path r"\\.\COM42"
echo $path.device  # COM42
```

### `is_verbatim`

Returns whether the path uses a verbatim `\\?\...` prefix.

```
let path = Path r"\\?\C:\work\file.txt"
echo $path.is_verbatim  # true
```

### `stream_name`

Alternate data stream name, or `nil` if no stream is specified.

```
let path = Path "file.txt:zone"
echo $path.name         # file.txt
echo $path.stream_name  # zone
```

### `stream_type`

Alternate data stream type without the leading `$`, or `nil` if no alternate
data stream was specified, or an alternate data stream was specified without an
explicit type.

```
let path = Path "file.txt:zone:$DATA"
echo $path.stream_type  # DATA
```

## Methods

### `sec_desc :resolve? = :TARGET: ...`

Gets selected parts of the Windows security descriptor.

**Optional Parameters:**

| Name       | Type                        | Description                                              |
| ---------- | --------------------------- | -------------------------------------------------------- |
| `owner:`   | [`Bool`](../../std/bool.md) | Load the owner SID                                       |
| `group:`   | [`Bool`](../../std/bool.md) | Load the primary group SID                               |
| `dacl:`    | [`Bool`](../../std/bool.md) | Load the discretionary ACL                               |
| `sacl:`    | [`Bool`](../../std/bool.md) | Load the system ACL                                      |
| `resolve:` | `:TARGET:`\|`:LINK:`        | Resolution mode (see [fs](../index.md#resolution-modes)) |

**Returns:** [`security.windows.SecDesc`](../../security/windows/secdesc.md)

SACL access requires `SeSecurityPrivilege`.

### `set_sec_desc desc :resolve = :TARGET:`

Applies the components selected by a `SecDesc`'s `mask`.

**Parameters:**

| Name      | Type                                                            | Description                                              |
| --------- | --------------------------------------------------------------- | -------------------------------------------------------- |
| `desc`    | [`security.windows.SecDesc`](../../security/windows/secdesc.md) | Security descriptor to apply                             |
| `resolve` | `:TARGET:`\|`:LINK:`                                            | Resolution mode (see [fs](../index.md#resolution-modes)) |

Windows may normalize the descriptor when associating it with the
filesystem object.

### `streams :resolve = :TARGET:`

Lists alternate data streams for this path.

#### Parameters

| Name      | Type                 | Description                                              |
| --------- | -------------------- | -------------------------------------------------------- |
| `resolve` | `:TARGET:`\|`:LINK:` | Resolution mode (see [fs](../index.md#resolution-modes)) |

#### Returns

iterator of [`StreamEntry`](stream-entry.md)

```
let path = Path "data.txt"
for stream = path.streams()
  echo "$(stream.name) $(stream.type)"
  echo (path / stream)
```
