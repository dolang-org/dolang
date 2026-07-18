# security

The `security` module reports the security identity of the active VFS target.

Platform-specific types are exposed by [`security.unix`](./unix/index.md) and
[`security.windows`](./windows/index.md).

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

**Returns:** [`security.unix.Identity`](./unix/identity.md)

**Errors:**

- Raises `UnsupportedError` when the active VFS target is Windows.

```
let info = unix_info()
echo "uid=$info.uid euid=$info.euid"
```

### `token_info()`

Returns Windows token information captured for the active VFS context.

**Returns:** [`security.windows.TokenInfo`](./windows/tokeninfo.md)

**Errors:**

- Raises `UnsupportedError` when the active VFS target is Unix.

```
if token_info().is_elevated
  echo elevated
```
