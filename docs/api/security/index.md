# security

The `security` module reports the security identity of the active VFS target.

## Types

| Type                            | Description                         |
| ------------------------------- | ----------------------------------- |
| [`Sid`](./sid.md)               | Windows security identifier         |
| [`TokenGroup`](./tokengroup.md) | Windows token group membership      |
| [`UnixInfo`](./unixinfo.md)     | Unix process identity information   |
| [`TokenInfo`](./tokeninfo.md)   | Windows process token information   |

## Functions

### `unix_info()`

Returns Unix security information captured for the active VFS context.

**Returns:** [`UnixInfo`](./unixinfo.md)

**Errors:**

- Raises `UnsupportedError` when the active VFS target is Windows.

```
let info = unix_info()
echo "uid=$info.uid euid=$info.euid"
```

### `token_info()`

Returns Windows token information captured for the active VFS context.

**Returns:** [`TokenInfo`](./tokeninfo.md)

**Errors:**

- Raises `UnsupportedError` when the active VFS target is Unix.

```
if token_info().is_elevated
  echo elevated
```
