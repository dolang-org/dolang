# `Blake3`

[`State`](./state.md) for BLAKE3.

## Inherits

- [`State`](./state.md)

## Constructor

### `Blake3()`

Creates a BLAKE3 digest state handle.

```
let state = Blake3()
state.update "abc"
assert_eq $state.digest().hex()
  6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85
```
