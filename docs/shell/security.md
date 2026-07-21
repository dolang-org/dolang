# Security

The `security` module provides portable name lookup, Unix process
identity, and Windows access token information and security descriptor
manipulation.

## Portable Identity Queries

[`security.user_name`](../api/security/index.md#user_name-uid) returns the
current target user on both platform families:

```
import security

echo "running as $(security.user_name())"
```

On Unix, `user_name uid`, `user_id name`, `group_name gid`, and
`group_id name` resolve accounts in the target's user and group databases.
Calling these functions when the current VFS context does not target a Unix
system raises [`sys.UnsupportedError`](../api/sys/unsupported-error.md).

## Unix Identity

[`security.unix_info()`](../api/security/index.md#unix_info) returns information
about the identity under which the shell or VFS process is running:

```
import security

let identity = security.unix_info()
echo "uid=$(identity.uid) gid=$(identity.gid)"
echo "effective uid=$(identity.euid) gid=$(identity.egid)"
for gid = identity.group_ids
  echo "group $gid: $(security.group_name(gid))"
```

## Windows Access Tokens

[`security.token_info()`](../api/security/index.md#token_info) returns a
[`TokenInfo`](../api/security/windows/tokeninfo.md) captured for the active
Windows target:

```
import security

let token = security.token_info()
let account = token.user_sid.lookup()
echo "$(account.qualified_name) ($(token.user_sid))"
echo "elevated: $(token.is_elevated)"

for group = token.groups
  echo "$(group.sid): enabled=$(group.enabled) deny-only=$(group.use_for_deny_only)"
```

The token also exposes its default owner, primary group, optional logon SID,
and complete group membership attributes. `is_elevated` reports whether the
Windows token has administrator rights.

## Resolving SIDs and Account Names

Use [`Sid.lookup()`](../api/security/windows/sid.md#lookup) for SID-to-name
resolution and
[`SidName.lookup`](../api/security/windows/sidname.md#lookup-value) for either
direction:

```
import security.windows:
  - SidName

let admins = SidName.lookup "BUILTIN\\Administrators"
echo "$(admins.sid): $(admins.qualified_name) ($(admins.kind))"
echo $admins.sid.lookup().qualified_name
```

SIDs and other identity types can always inspected on a Unix host once
obtained, but resolution is only possible on an active Windows VFS target.

## Filesystem Security Descriptors

Windows file ownership and access control are represented by
[`SecDesc`](../api/security/windows/secdesc.md),
[`Acl`](../api/security/windows/acl.md), and
[`Ace`](../api/security/windows/ace.md):

- `SecDesc` carries selected owner, group, DACL, and SACL components plus
  native control flags.
- `Acl` is an immutable ordered collection of access-control entries.
- `Ace` exposes its trustee SID, access mask, inheritance flags, and native
  ACE type.

Read selected components with
[`fs.sec_desc`](../api/fs/index.md#sec_desc-path-owner-group-dacl-sacl-resolve):

```
import fs

let desc = fs.sec_desc config.ini
echo "owner: $(desc.owner.lookup().qualified_name)"
if desc.dacl == nil
  echo "DACL: null"
else
  for ace = desc.dacl.aces
    echo "$(ace.type) $(ace.sid) $(ace.mask)"
```

Owner, group, and DACL are loaded by default. Request `sacl: true` only when
the caller has the required Windows access rights and privileges.

`SecDesc.with` creates a modified descriptor while preserving other components
Apply a modified descriptor with
[`fs.set_sec_desc`](../api/fs/index.md#set_sec_desc-path-desc-resolve):

```
import fs
import security.windows:
  - SidName

let desc = fs.sec_desc config.ini
let owner = (SidName.lookup "BUILTIN\\Administrators").sid
fs.set_sec_desc config.ini $ desc.with :owner
```

Changing a DACL normally requires `WRITE_DAC`; changing an owner normally
requires `WRITE_OWNER` or an applicable ownership privilege. Reading or
writing a SACL normally requires `ACCESS_SYSTEM_SECURITY` and the corresponding
security privilege. Windows returns `sys.PermissionDeniedError` or a more
specific native error when the VFS context lacks the required authority.

`SecDesc`, `Acl`, `Ace`, and `Sid` support native binary conversion.
Pure inspection and manipulation of descriptors works on Unix hosts.

## VFS Behavior

Security operations follow the active VFS context just like filesystem and
process operations. A Linux interpreter connected to Windows receives Windows
token, SID, descriptor, and error semantics; a Windows interpreter connected
to Unix receives UID/GID semantics. Nesting SSH, container, WSL, or elevation
contexts changes which identity is queried.
