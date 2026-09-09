# Security

The `security` module provides portable name lookup, Unix process identity,
POSIX ACLs, and Windows access token information and security descriptor
manipulation.

## Portable Identity Queries

[`security.user_name`](security.user_name) returns the
current target user on both platform families:

```
import security

echo "running as $(security.user_name())"
```

[`security.unix.user_name`](security.unix.user_name),
[`security.unix.user_id`](security.unix.user_id),
[`security.unix.group_name`](security.unix.group_name),
and [`security.unix.group_id`](security.unix.group_id)
resolve accounts in the target's user and group databases. Calling these
functions when the current VFS context does not target a Unix system raises
[`sys.UnsupportedError`](sys.UnsupportedError).

## Unix Identity

[`security.unix.id()`](security.unix.id) returns information
about the identity under which the shell or VFS process is running:

```
import security.unix:
  - id
  - group_name

let identity = id()
echo "uid=$(identity.uid) gid=$(identity.gid)"
echo "effective uid=$(identity.euid) gid=$(identity.egid)"
for gid = identity.groups
  echo "group $gid: $(group_name(gid))"
```

## POSIX ACLs

[`security.unix.Acl`](security.unix.Acl) is an immutable collection
of [`security.unix.Ace`](security.unix.Ace) entries. The object model
is available on every platform. Filesystem get and set operations are supported
on Linux and FreeBSD.

```
import fs
import security.unix:
  - Acl
  - Ace
  - Permission
  - id

let identity = id()
let rw = Permission(:READ:, :WRITE:)
let r = Permission(:READ:)
let access = Acl $
  $ Ace.user_obj $rw
  $ Ace.user $identity.euid $r
  $ Ace.group_obj $r
  $ Ace.mask $r
  $ Ace.other()
fs.set_acl config.ini $access
```

Use [`fs.acl`](fs.acl) to read
stored ACL metadata. It returns `nil` when no ACL is stored; it does not
construct an ACL from file mode bits. Pass `nil` to
[`fs.set_acl`](fs.set_acl) to
remove the ACL. Set `default: true` to operate on a directory's inheritable
default ACL.

`Acl` validates the required base entries and requires a mask when named user
or group entries are present. It preserves the supplied mask without
recalculating it.

## NFSv4 ACLs

[`security.nfs4.Acl`](security.nfs4.Acl) is an immutable collection
of [`security.nfs4.Ace`](security.nfs4.Ace) entries. The object
model is available on every platform. Filesystem get and set operations are
supported on FreeBSD only.

```
import fs
import security.nfs4:
  - Acl
  - Ace
  - Mask
import security.unix:
  - id

let identity = id()
let read = Mask(:READ_DATA:, :READ_ATTRIBUTES:, :READ_ACL:)
let access = Acl $
  $ Ace.owner type: :ALLOW: mask: (Mask())
  $ Ace.user $identity.euid type: :ALLOW: mask: $read
  $ Ace.everyone type: :DENY: mask: $read
fs.set_acl config.ini $access
```

Pass `kind: :NFS4:` to `fs.acl` to read an NFSv4 ACL instead of the default
POSIX one; a built ACL supplies its format to `fs.set_acl`. Declarative ACE
sequences require `kind: :NFS4:`. `default:
true` is not valid with an NFSv4 ACL — inheritance is expressed through
[`Ace`](security.nfs4.Ace) flags instead of a separate default-ACL
object. Unlike a POSIX ACL, an NFSv4 ACL is a file's native security
descriptor: it can be replaced with `fs.set_acl`, but FreeBSD provides no
operation to remove it back to "none".

## macOS ACLs

[`security.macos.Acl`](security.macos.Acl) is an immutable
collection of [`security.macos.Ace`](security.macos.Ace) entries.
The object model is available on every platform. Filesystem get and set
operations are supported on macOS only.

Unlike NFSv4 or POSIX.1e ACL entries, macOS resolves every principal to a
UUID before it reaches the file's ACL, so an `Ace`'s principal is a
[`uuid.Uuid`](uuid.Uuid) rather than a special-cased qualifier:

```
import fs
import security.macos:
  - Acl
  - Ace
  - Mask

let owner = uuid.Uuid "..."
let read = Mask(:READ_DATA:, :READ_ATTRIBUTES:, :READ_SECURITY:)
let access = Acl $
  $ Ace.allow $owner mask: $read
fs.set_acl config.ini $access
```

Pass `kind: :MACOS:` to `fs.acl` to read a macOS extended ACL instead of the
default POSIX one; a built ACL supplies its format to `fs.set_acl`.
Declarative ACE sequences require `kind: :MACOS:`.
`default: true` is not valid with a macOS ACL, the same as with an NFSv4
one. Unlike an NFSv4 ACL, a macOS extended ACL is an optional overlay on top
of POSIX permissions, so it can be removed back to "none" with `fs.set_acl
config.ini nil kind: :MACOS:`.

Since ACL principals are UUIDs, building or inspecting a macOS ACE usually
means converting between a Unix uid/gid and its UUID.
[`security.macos.uuid_for_uid`](security.macos.uuid_for_uid)
and
[`uuid_for_gid`](security.macos.uuid_for_gid) go from
id to UUID;
[`id_for_uuid`](security.macos.id_for_uuid) goes the
other way, returning which kind (`:UID:` or `:GID:`) the UUID resolved to
alongside the id itself, since a bare UUID doesn't say which it is:

```
import security.macos
import security.unix: id

let owner = security.macos.uuid_for_uid id().euid
let access = Acl $
  $ Ace.allow $owner mask: $read
fs.set_acl config.ini $access

let kind id = security.macos.id_for_uuid owner
echo "$kind $id"  # UID 501
```

[`security.unix.user_name`](security.unix.user_name) and
[`group_name`](security.unix.group_name) also accept a
UUID directly on macOS, resolving it internally, so an ACE principal can be
turned into a name without a separate `id_for_uuid` call.

## Windows Access Tokens

[`security.windows.token_info()`](security.windows.token_info)
returns a [`TokenInfo`](security.windows.TokenInfo) captured for the
active Windows target:

```
import security.windows:
  - token_info

let token = token_info()
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

Use [`Sid.lookup()`](security.windows.Sid.lookup) for SID-to-name
resolution and
[`SidName.lookup`](security.windows.SidName.lookup) for either
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
[`SecDesc`](security.windows.SecDesc),
[`Acl`](security.windows.Acl), and
[`Ace`](security.windows.Ace):

- `SecDesc` carries selected owner, group, DACL, and SACL components plus
  native control flags.
- `Acl` is an immutable ordered collection of access-control entries.
- `Ace` exposes its trustee SID, access mask, inheritance flags, and native
  ACE type.

Read selected components with
[`fs.windows.sec_desc`](fs.windows.sec_desc):

```
import fs.windows:
  - sec_desc

let desc = sec_desc config.ini
echo "owner: $(desc.owner.lookup().qualified_name)"
if desc.dacl == nil
  echo "DACL: null"
else
  for ace = desc.dacl.aces
    echo "$(ace.type) $(ace.sid) $(ace.mask)"
```

Owner, group, and DACL are loaded by default. Request `sacl: true` only when
the caller has the required Windows access rights and privileges.

`SecDesc.with` creates a modified descriptor while preserving other components.
Apply a modified descriptor with
[`fs.windows.update_sec_desc`](fs.windows.update_sec_desc):

```
import fs.windows:
  - sec_desc
  - update_sec_desc
import security.windows:
  - SidName

let desc = sec_desc config.ini
let owner = (SidName.lookup "BUILTIN\\Administrators").sid
update_sec_desc config.ini $ desc.with :owner
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
