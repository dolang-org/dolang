# `digest`

Digest algorithms.

## Types

| Type                      | Description                                 |
| ------------------------- | ------------------------------------------- |
| [`State`](./state.md)     | Supertype for digest state handles          |
| [`Blake3`](./blake3.md)   | BLAKE3 state handle                         |
| [`Md5`](./md5.md)         | MD5 state handle                            |
| [`Sha1`](./sha1.md)       | SHA-1 state handle                          |
| [`Sha256`](./sha256.md)   | SHA-256 state handle                        |
| [`Sha512`](./sha512.md)   | SHA-512 state handle                        |

## Functions

### `blake3 data`

Computes the BLAKE3 digest.

**Parameters:**

| Name   | Type                                           | Description   |
| ------ | ---------------------------------------------- | ------------- |
| `data` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | Input to hash |

**Returns:** [`bin`](../std/bin.md) - 32-byte digest

```
let digest = blake3 "abc"
assert_eq $digest.len 32
assert_eq $digest.hex()
  6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85
```

### `md5 data`

Computes the MD5 digest.

**Parameters:**

| Name   | Type                                           | Description   |
| ------ | ---------------------------------------------- | ------------- |
| `data` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | Input to hash |

**Returns:** [`bin`](../std/bin.md) - 16-byte digest

```
assert_eq $md5("abc").hex()
  900150983cd24fb0d6963f7d28e17f72
```

### `sha1 data`

Computes the SHA-1 digest.

**Parameters:**

| Name   | Type                                           | Description   |
| ------ | ---------------------------------------------- | ------------- |
| `data` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | Input to hash |

**Returns:** [`bin`](../std/bin.md) - 20-byte digest

```
assert_eq $sha1("abc").hex()
  a9993e364706816aba3e25717850c26c9cd0d89d
```

### `sha256 data`

Computes the SHA-256 digest.

**Parameters:**

| Name   | Type                                           | Description   |
| ------ | ---------------------------------------------- | ------------- |
| `data` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | Input to hash |

**Returns:** [`bin`](../std/bin.md) - 32-byte digest

```
assert_eq $sha256("abc").hex()
  ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
```

### `sha512 data`

Computes the SHA-512 digest.

**Parameters:**

| Name   | Type                                           | Description   |
| ------ | ---------------------------------------------- | ------------- |
| `data` | [`str`](../std/str.md)\|[`bin`](../std/bin.md) | Input to hash |

**Returns:** [`bin`](../std/bin.md) - 64-byte digest

```
assert_eq $sha512("abc").hex()[..10]
  ddaf35a193
```
