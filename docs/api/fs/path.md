# Path

Abstract supertype of [`UnixPath`](unix-path.md) and
[`WindowsPath`](windows-path.md).

## Constructor

### `Path path`

**Parameters:**

| Name   | Type                                      | Description |
| ------ | ----------------------------------------- | ----------- |
| `path` | [`str`](../std/str.md)\|[`Path`](path.md) | Path value  |

**Returns:** [`UnixPath`](unix-path.md) or [`WindowsPath`](windows-path.md).

The returned path type is chosen according to the current VFS context.

## Fields

### `name`

The final component of the path, or `nil` if the path is empty.

```
let path = Path /home/user/file.txt
echo $path.name  # file.txt
```

### `stem`

The final component without its last extension, or `nil` if the path is
empty.

```
let path = Path "/home/user/archive.tar.gz"
echo $path.stem  # archive.tar

let no_ext = Path "/home/user/Makefile"
echo $no_ext.stem  # Makefile
```

### `parent`

The parent directory as a `Path` of the same subtype, or `nil` if the path is
empty or contains only one component.

```
let path = Path /home/user/file.txt
let parent = path.parent
echo parent  # /home/user
```

### `ext`

The file extension (without the leading dot), or `nil` if the final
component has no extension.

```
let path = Path "/home/user/file.txt"
echo $path.ext  # txt

let no_ext = Path "/home/user/noextension"
echo $no_ext.ext  # nil
```

### `is_absolute`

Whether the path is absolute (starts from the filesystem root).

```
let abs = Path "/home/user/file.txt"
echo $abs.is_absolute  # true

let rel = Path "./file.txt"
echo $rel.is_absolute  # false
```

### `components`

Immutable array-like view of the lexical path components.

```
let path = Path "alpha/beta/gamma"
assert_eq [...path.components] ["alpha", "beta", "gamma"]
```

Windows paths carry the alternate data stream specified in the final
path component if present.

## Class Methods

### `join ...components`

Joins multiple path components into a single path. Components may be Path
objects or strings.

If any component is an absolute path, it replaces everything before it.

#### Parameters

| Name         | Type          | Description             |
| ------------ | ------------- | ----------------------- |
| `components` | `str`\|`Path` | Path components to join |

#### Returns

`Path`

#### Example

```
let path = Path.join home user docs file.txt
echo $path.name  # file.txt

# Absolute path replaces everything before it
let abs = Path.join home /etc config.txt
echo $abs  # /etc/config.txt
```

## Methods

### `open :mode? :block?`

Equivalent to [`fs.open`](index.md#open-path-mode-func)

### `metadata :resolve = :TARGET:`

Equivalent to [`fs.metadata`](index.md#metadata-path-resolve)

### `fs_metadata :resolve = :TARGET:`

Equivalent to [`fs.fs_metadata`](index.md).

### `exists()`

Equivalent to [`fs.exists`](index.md#exists-path).

### `read mode?`

Equivalent to [`fs.read`](index.md#read-path-mode).

### `write content`

Equivalent to [`fs.write`](index.md#write-path-content).

### `append content`

Equivalent to [`fs.append`](index.md#append-path-content).

### `set_len size`

Equivalent to [`fs.set_len`](index.md#set_len-path-size).

### `set_metadata :resolve? ...`

Equivalent to [`fs.set_metadata`](index.md#set_metadata-resolve-paths).

### `xattrs :namespace? :resolve = :TARGET:`

Equivalent to [`fs.xattrs`](index.md).

### `xattr name :namespace? :resolve = :TARGET:`

Equivalent to [`fs.xattr`](index.md).

### `set_xattr name value :namespace? :resolve = :TARGET:`

Equivalent to [`fs.set_xattr`](index.md).

### `remove_xattr name :namespace? :resolve = :TARGET:`

Equivalent to [`fs.remove_xattr`](index.md).

### `copy to :all?`

Equivalent to [`fs.copy`](index.md#copy-from-to-all).

### `rename to`

Equivalent to [`fs.rename`](index.md#rename-from-to).

### `move to :all?`

Equivalent to [`fs.move`](index.md#move-from-to-all).

### `hard_link to`

Equivalent to [`fs.hard_link`](index.md#hard_link-src-dst).

### `entries()`

Equivalent to [`fs.entries`](index.md#entries-path).

### `add_ext ext`

Returns a new path with `ext` appended as an additional extension.

#### Parameters

| Name  | Type                   | Description                  |
| ----- | ---------------------- | ---------------------------- |
| `ext` | [`str`](../std/str.md) | Extension to append          |

#### Returns

[Path](path.md)

#### Example

```
let path = Path "archive.tar"
echo path.add_ext "gz"  # archive.tar.gz

let file = Path "report"
echo file.add_ext "txt"  # report.txt
```

### `canonical()`

Equivalent to [`fs.canonical`](index.md#canonical-path).

### `read_link()`

Equivalent to [`fs.read_link`](index.md#read_link-path).

### `without_ext()`

Returns a new path with the final extension removed.

#### Returns

[Path](path.md)

```
let path = Path "archive.tar.gz"
echo path.without_ext()  # archive.tar

let plain = Path "Makefile"
echo plain.without_ext()  # Makefile
```

### `with_ext ext`

Returns a new path with the final extension replaced.

#### Parameters

| Name  | Type                   | Description             |
| ----- | ---------------------- | ----------------------- |
| `ext` | [`str`](../std/str.md) | Replacement extension   |

#### Returns

[Path](path.md)

```
let path = Path "archive.tar.gz"
echo path.with_ext "zip"  # archive.tar.zip

let plain = Path "Makefile"
echo plain.with_ext "txt"  # Makefile.txt
```

### `with_name name`

Returns a new path with the final component replaced.

#### Parameters

| Name   | Type                   | Description                 |
| ------ | ---------------------- | --------------------------- |
| `name` | [`str`](../std/str.md) | Replacement final component |

#### Returns

[Path](path.md)

```
let path = Path "src/main.rs"
echo path.with_name "lib.rs"  # src/lib.rs
```

### `with_stem stem`

Returns a new path with the final stem replaced, preserving the final
extension when present.

#### Parameters

| Name   | Type                   | Description         |
| ------ | ---------------------- | ------------------- |
| `stem` | [`str`](../std/str.md) | Replacement stem    |

#### Returns

[Path](path.md)

```
let path = Path "archive.tar.gz"
echo path.with_stem "bundle"  # bundle.gz

let plain = Path "Makefile"
echo plain.with_stem "Dockerfile"  # Dockerfile
```

### `remove :all? :ignore?`

Equivalent to [`remove`](index.md#remove-path-all-ignore).

### `create_dir :all?`

Equivalent to [`create_dir`](index.md#create_dir-path-all).

### `remove_dir :all? :ignore?`

Equivalent to [`remove_dir`](index.md#remove_dir-path-all-ignore).

### `set_timestamps :modified? :accessed? :created? :resolve?`

Equivalent to
[`set_timestamps`](index.md#set_timestamps-path-modified-accessed-created-resolve)

### `normalize()`

Equivalent to [`normalize`](index.md#normalize-path)

### `absolute()`

Equivalent to [`absolute`](index.md#absolute-path)

### `relative base?`

Equivalent to [`relative`](index.md#relative-path-base)

### `glob pattern :max_depth? :resolve?`

Equivalent to [`glob`](index.md#glob-pattern-max_depth-resolve), but searches
within this path. Yielded paths will contain this path as a prefix.
