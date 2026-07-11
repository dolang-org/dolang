# `Md5`

[`State`](./state.md) for MD5.

## Inherits

- [`State`](./state.md)

## Constructor

### `Md5()`

Creates an MD5 digest state handle.

```
let state = Md5()
state.update "abc"
assert_eq $state.digest().hex()
  900150983cd24fb0d6963f7d28e17f72
```
