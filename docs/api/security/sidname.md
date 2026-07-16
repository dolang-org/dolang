# SidName

Resolved Windows account identity.

## Class Methods

### `lookup value`

Resolves an account name or [`Sid`](./sid.md) in the active VFS target.

**Parameters:**

| Name    | Type                                      | Description         |
| ------- | ----------------------------------------- | ------------------- |
| `value` | [`str`](../std/str.md)\|[`Sid`](./sid.md) | Account name or SID |

**Returns:** `SidName`

**Errors:**

- Raises [`sys.NotFoundError`](../sys/not-found-error.md) when the identity is
  unmapped.
- Raises `UnsupportedError` for Unix targets.

## Fields

### `sid`

Resolved [`Sid`](./sid.md).

### `name`

Unqualified account name.

### `domain`

Account domain returned by Windows.

### `qualified_name`

The `domain\name` form, or `name` when the domain is empty.

### `kind`

Windows SID name-use classification as an uppercase symbol.

| Value                 | Meaning                         |
| --------------------- | ------------------------------- |
| `:USER:`              | User SID                        |
| `:GROUP:`             | Group SID                       |
| `:DOMAIN:`            | Domain SID                      |
| `:ALIAS:`             | Alias SID                       |
| `:WELL_KNOWN_GROUP:`  | Well-known group SID            |
| `:DELETED_ACCOUNT:`   | Deleted account SID             |
| `:INVALID:`           | Invalid SID                     |
| `:UNKNOWN:`           | SID of an unknown type          |
| `:COMPUTER:`          | Computer SID                    |
| `:LABEL:`             | Mandatory integrity label SID   |
| `:LOGON_SESSION:`     | Logon session SID               |

```
let account = SidName.lookup "BUILTIN\\Users"
echo "$account.qualified_name ($account.kind)"
```
