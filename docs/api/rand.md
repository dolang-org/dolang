# rand

Random integer sampling, string generation, selection, and shuffling.

## Functions

### `int end`

Samples a uniformly distributed integer from the half-open range `[0, end)`.

#### Parameters

| Name  | Type  | Description           |
| ----- | ----- | --------------------- |
| `end` | `int` | exclusive upper bound |

#### Returns

`int` - Sampled integer

#### Errors

| Exception    | Condition               |
| ------------ | ----------------------- |
| `TypeError`  | `end` is not an integer |
| `ValueError` | `end <= 0`              |

```
assert_eq (int 1) 0
let n = int 10
assert ((n >= 0) && (n < 10))
```

### `int end start`

Samples a uniformly distributed integer from the half-open range `[start, end)`.

#### Parameters

| Name    | Type  | Description           |
| ------- | ----- | --------------------- |
| `end`   | `int` | exclusive upper bound |
| `start` | `int` | lower bound           |

#### Returns

`int` - Sampled integer

#### Errors

| Exception    | Condition                          |
| ------------ | ---------------------------------- |
| `TypeError`  | `start` or `end` is not an integer |
| `ValueError` | `end <= start`                     |

```
assert_eq (int 0 1) 0
let n = int 20 10
assert ((n >= 10) && (n < 20))
assert_eq (int -4 -5) -5
```

### `string len :alphabet?`

Generates a random string by sampling characters from `alphabet`.

#### Parameters

| Name       | Type   | Description                  |
| ---------- | ------ | ---------------------------- |
| `len`      | `int`  | number of characters to emit |
| `alphabet` | `str?` | characters to sample from    |

#### Returns

[`str`](./std/str.md) - Randomly generated text

#### Errors

| Exception    | Condition                                             |
| ------------ | ----------------------------------------------------- |
| `TypeError`  | `len` is not an integer or `alphabet` is not a string |
| `ValueError` | `len` is negative                                     |
| `ValueError` | `alphabet` is empty                                   |

`alphabet` is interpreted as characters, not raw UTF-8 bytes.

If `alphabet:` is omitted, it defaults to the URL-safe NanoID alphabet:
`_-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ`.

```
let token = string 12
assert_eq $token.len 12

let text = string 4 alphabet: "åß"
assert_eq $text.len 8
```

### `pick collection`

Selects one random element from an array.

#### Parameters

| Name         | Type    | Description         |
| ------------ | ------- | ------------------- |
| `collection` | `array` | source array        |

#### Returns

One element from `collection`

#### Errors

| Exception    | Condition                    |
| ------------ | ---------------------------- |
| `TypeError`  | `collection` is not an array |
| `ValueError` | `collection` is empty        |

```
let item = pick [1, 2, 3]
assert ([1, 2, 3].contains $item)
```

### `shuffle array`

Shuffles an array in place using a uniform Fisher-Yates pass.

#### Parameters

| Name    | Type    | Description      |
| ------- | ------- | ---------------- |
| `array` | `array` | array to mutate  |

#### Returns

`nil`

#### Errors

| Exception   | Condition               |
| ----------- | ----------------------- |
| `TypeError` | `array` is not an array |

```
let items = [1, 2, 3, 4]
shuffle $items
let sorted = [...items]
sorted.sort()
assert_eq $sorted [1, 2, 3, 4]
```
