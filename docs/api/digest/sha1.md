# `Sha1`

[`State`](./state.md) for SHA-1.

## Inherits

- [`State`](./state.md)

## Constructor

### `Sha1()`

Creates a SHA-1 digest state handle.

```
let state = Sha1()
state.update "abc"
assert_eq $state.digest().hex()
  a9993e364706816aba3e25717850c26c9cd0d89d
```
