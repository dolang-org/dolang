# security

The `security` module reports the security identity of the active VFS target.

## Types

| Type                            | Description                         |
| ------------------------------- | ----------------------------------- |
| [`SecDesc`](./secdesc.md)       | Windows security descriptor         |
| [`Sid`](./sid.md)               | Windows security identifier         |
| [`SidName`](./sidname.md)       | Resolved Windows account identity   |
| [`TokenGroup`](./tokengroup.md) | Windows token group membership      |
| [`UnixInfo`](./unixinfo.md)     | Unix process identity information   |
| [`TokenInfo`](./tokeninfo.md)   | Windows process token information   |

## Functions

### `user_name uid?`

Returns a user name from the active VFS target. With no argument, resolves the
real user ID on Unix or the access token's user SID on Windows. With a user ID,
resolves that ID on Unix.

**Parameters:**

| Name  | Type   | Description                   |
| ----- | ------ | ----------------------------- |
| `uid` | `int`? | Unix user ID; defaults to UID |

**Returns:** [`str`](../std/str.md)

**Errors:**

- Raises [`sys.NotFoundError`](../sys/not-found-error.md) when the ID is
  unknown.
- Raises `UnsupportedError` when an ID is supplied for a Windows target.

### `user_id name`

Resolves a Unix user name in the active VFS target.

**Returns:** [`int`](../std/int.md)

**Errors:**

- Raises [`sys.NotFoundError`](../sys/not-found-error.md) when the name is
  unknown.
- Raises `UnsupportedError` for Windows targets.

### `group_name gid`

Resolves a Unix group ID in the active VFS target.

**Returns:** [`str`](../std/str.md)

**Errors:**

- Raises [`sys.NotFoundError`](../sys/not-found-error.md) when the ID is
  unknown.
- Raises `UnsupportedError` for Windows targets.

### `group_id name`

Resolves a Unix group name in the active VFS target.

**Returns:** [`int`](../std/int.md)

**Errors:**

- Raises [`sys.NotFoundError`](../sys/not-found-error.md) when the name is
  unknown.
- Raises `UnsupportedError` for Windows targets.

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
