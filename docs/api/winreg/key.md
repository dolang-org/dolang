# Key

An open registry key, returned by
[`winreg.open`](./index.md#open-root-view-access-func) or one of `Key`'s own
`open`/`create` methods.

## Methods

### `open subpath view? access? func?`

Opens a subkey relative to this key.

**Parameters:**

| Name      | Type                   | Description                                                |
| --------- | ---------------------- | ---------------------------------------------------------- |
| `subpath` | [`Str`](../std/str.md) | Path to the subkey, relative to this key                   |
| `view`    | sym?                   | `:NATIVE:`, `:WOW32:`, or `:WOW64:` (default: `:NATIVE:`)  |
| `access`  | sym?                   | `:READ:`, `:WRITE:`, or `:READ_WRITE:` (default: `:READ:`) |
| `func`    | func?                  | Callable to run with the key; auto-closes when done        |

**Returns:** [`Key`](./key.md) when no `func` is given, otherwise the result
of calling `func`

**Errors:**

- [`sys.NotFoundError`](../sys/not-found-error.md) — the subkey does not exist
- [`sys.PermissionDeniedError`](../sys/permission-denied-error.md) — access
    was denied

```
winreg.open :CURRENT_USER: do |root|
  root.open "Environment" do |env|
    echo (env.get "TEMP")
```

### `create subpath view? access? func?`

Creates a subkey relative to this key, or opens it if it already exists.

**Parameters:**

Same as [`open`](#open-subpath-view-access-func).

**Returns:** [`Key`](./key.md) when no `func` is given, otherwise the result
of calling `func`

```
winreg.open :CURRENT_USER: access: :READ_WRITE: do |root|
  root.create "Software/MyApp" do |app|
    app.set "installed" true
```

### `delete subpath :view? :all? :ignore?`

Deletes a subkey. By default, the subkey must have no children. With `all:
true`, its values and descendants are deleted recursively. With `ignore: true`,
a missing subkey is ignored.

**Parameters:**

| Name      | Type                     | Description                                               |
| --------- | ------------------------ | --------------------------------------------------------- |
| `subpath` | [`Str`](../std/str.md)   | Path to the subkey, relative to this key                  |
| `view`    | sym?                     | `:NATIVE:`, `:WOW32:`, or `:WOW64:` (default: `:NATIVE:`) |
| `all`     | [`Bool`](../std/bool.md) | If `true`, deletes values and descendants recursively     |
| `ignore`  | [`Bool`](../std/bool.md) | If `true`, ignores a missing subkey                       |

**Errors:**

- [`sys.NotFoundError`](../sys/not-found-error.md) — the subkey does not exist
- Without `all: true`, deleting a subkey that still has children raises

```
winreg.open :CURRENT_USER: access: :READ_WRITE: do |root|
  root.delete "Software/MyApp"
  root.delete "Software/MyAppTree" all: true
  root.delete "Software/Missing" ignore: true
```

### `close()`

Closes the key. Keys not explicitly closed are closed when garbage
collected. Idempotent — closing an already-closed key is a no-op.

### `subkeys()`

Lists the names of every immediate child key.

**Returns:** [`Array`](../std/array.md) of [`Str`](../std/str.md)

```
for name = key.subkeys()
  echo $name
```

### `values()`

Reads every value under this key in a single call.

**Returns:** An iterable, unpackable snapshot of [`Value`](./value.md) entries

```
for :name :kind :value = key.values()
  echo "$name ($kind): $value"
```

### `get name`

Reads a value and returns its coerced Do representation.

**Parameters:**

| Name   | Type                   | Description                                        |
| ------ | ---------------------- | -------------------------------------------------- |
| `name` | [`Str`](../std/str.md) | Value name; `""` refers to the key's default value |

**Returns:** The value's natural Do representation — see
[`Value.value`](./value.md#value) for the kind mapping

**Errors:**

- [`sys.NotFoundError`](../sys/not-found-error.md) — no value with that name
    exists

```
let temp = env.get "TEMP"
```

### `get_value name`

Reads a value and returns it as a [`Value`](./value.md), or `nil` if it
doesn't exist. Unlike [`get`](#get-name), this never raises for a missing
value, and it preserves the value's kind rather than just its coerced data —
use it to test for existence or to inspect a value's raw `REG_*` kind.

**Parameters:**

Same as [`get`](#get-name).

**Returns:** [`Value`](./value.md) or `nil`

```
let entry = env.get_value "TEMP"
if entry
  echo "TEMP is a $entry.kind value: $entry.value"
else
  echo "TEMP is not set"
```

### `set name value kind?`

Writes a value.

Without `kind`, the Do value is coerced into whatever kind is already stored
under `name`, if any (an unrecognized raw `REG_*` kind — see
[`Value.kind`](./value.md#kind) — round-trips by writing `value` back as the
same raw kind, so it must be a [`Bin`](../std/bin.md)). If no value exists yet,
a kind is inferred from `value`'s own Do type: [`Str`](../std/str.md) → `:SZ:`,
[`Array`](../std/array.md) of `Str` → `:MULTI_SZ:`, [`Int`](../std/int.md) →
`:DWORD:` if it fits in 32 bits, else `:QWORD:`, [`Bin`](../std/bin.md) →
`:BINARY:`, `nil` → `:NONE:`.

**Parameters:**

| Name    | Type                   | Description                                                                                                                              |
| ------- | ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `name`  | [`Str`](../std/str.md) | Value name; `""` refers to the key's default value                                                                                       |
| `value` |                        | The Do value to write                                                                                                                    |
| `kind`  | sym?                   | Forces the exact kind to write: `:SZ:`, `:EXPAND_SZ:`, `:MULTI_SZ:`, `:DWORD:`, `:DWORD_BIG_ENDIAN:`, `:QWORD:`, `:BINARY:`, or `:NONE:` |

```
key.set "installed" true
key.set "path" "C:\\Program Files\\MyApp"
key.set "tags" ["a", "b"]
key.set "raw" b"\x01\x02" kind: :BINARY:
```

### `delete_value name`

Deletes a value.

**Parameters:**

| Name   | Type                   | Description                                        |
| ------ | ---------------------- | -------------------------------------------------- |
| `name` | [`Str`](../std/str.md) | Value name; `""` refers to the key's default value |

**Errors:**

- [`sys.NotFoundError`](../sys/not-found-error.md) — no value with that name
    exists

```
key.delete_value "installed"
```
