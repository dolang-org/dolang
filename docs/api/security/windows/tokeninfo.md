# `TokenInfo`

Windows process token information for the active VFS target.

## Fields

### `is_elevated`

Whether the token is elevated.

### `user_sid`

SID of the token's user.

### `owner_sid`

Default owner SID for objects created by the token.

### `primary_group_sid`

Primary group SID for objects created by the token.

### `logon_sid`

The group SID marked as the token's logon SID, or `nil` if none is present.

### `groups`

A lazy array-like view of the token's [`TokenGroup`](./tokengroup.md) objects.
