# Glob

A compiled glob pattern.

## Constructor

### `Glob pattern`

Compiles a glob pattern.

#### Parameters

| Name      | Type                   | Description   |
| --------- | ---------------------- | ------------- |
| `pattern` | [`str`](../std/str.md) | Glob pattern  |

#### Errors

| Exception    | Condition              |
| ------------ | ---------------------- |
| `ValueError` | The pattern is invalid |

```
let png = Glob "**/*.png"
let tree = Glob "src/**/mod.rs"
```

## Methods

### `matches value`

Tests whether `value` matches this glob.

#### Parameters

| Name    | Type                   | Description               |
| ------- | ---------------------- | ------------------------- |
| `value` | [`str`](../std/str.md) | Candidate string to test  |

#### Returns

[`bool`](../std/index.md) indicating whether the value matches.

```
let png = Glob "**/*.png"

assert (png.matches "assets/logo.png")
assert (!(png.matches "assets/logo.jpg"))
```
