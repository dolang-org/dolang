# Value

One named entry read from a key, returned by
[`Key.values()`](./key.md#values) or [`Key.get_value`](./key.md#get_value-name).
Supports keyword unpack, so `for :name :kind :value = key.values()` works.

## Fields

### `name`

The value's name. **Type:** [`Str`](../std/str.md)

The default value of a key has the name `""`.

### `kind`

The value's `REG_*` kind. **Type:** sym|[`Int`](../std/int.md)

One of `:SZ:`, `:EXPAND_SZ:`, `:MULTI_SZ:`, `:DWORD:`, `:DWORD_BIG_ENDIAN:`,
`:QWORD:`, `:BINARY:`, or `:NONE:` for a recognized kind. For an
unrecognized `REG_*` kind, this is the raw kind number instead, and `value`
is the raw [`Bin`](../std/bin.md) payload.

### `value`

The value's data, in its natural Do representation. **Type:**
[`Str`](../std/str.md)\|[`Array`](../std/array.md)\|[`Int`](../std/int.md)\|[`Bin`](../std/bin.md)\|`nil`

| `kind`                                     | `value` type        |
| ------------------------------------------ | ------------------- |
| `:SZ:`, `:EXPAND_SZ:`                      | `Str`               |
| `:MULTI_SZ:`                               | `Array` of `Str`    |
| `:DWORD:`, `:DWORD_BIG_ENDIAN:`, `:QWORD:` | `Int`               |
| `:BINARY:`                                 | `Bin`               |
| `:NONE:`                                   | `nil`               |
| unrecognized (int `kind`)                  | `Bin` (raw payload) |

```
for :name :kind :value = key.values()
  echo "$name ($kind): $value"
```
