# `State`

Supertype for stateful digest handles.

Subtype of [`Sink`](../std/sink.md). Putting a [`str`](../std/str.md)
or [`bin`](../std/bin.md) value updates the digest state with its bytes.

## Methods

### `update data`

Updates the digest state with the bytes of `data`.

**Parameters:**

| Name   | Type                                           | Description     |
| ------ | ---------------------------------------------- | --------------- |
| `data` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | Input to hash   |

**Returns:** The same handle, for chaining.

```
let state = Blake3()
state.update "ab".update b"c"
assert_eq $state.digest().hex()
  6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85

let sink = Blake3()
sink.put "ab"
sink.put b"c"
assert_eq $sink.digest() (blake3 "abc")
```

### `digest`

Returns the current digest bytes without consuming the handle.

**Returns:** [`bin`](../std/bin.md) - Digest snapshot

```
let state = Blake3()
state.update "abc"
let first = state.digest()
let second = state.digest()
assert_eq $first $second
state.update "def"
assert_eq $state.digest().hex()
  b22b3b2ee0e7c0a8e75a988d1d7e874e3c6de8b00a4427a47887877454b45db1
```
