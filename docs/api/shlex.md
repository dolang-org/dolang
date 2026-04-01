# shlex

Shell quoting and string splitting utilities.

## Functions

### `quote obj`

Quote a string for shell safety.

**Parameters:**

| Name  | Type | Description                              |
| ----- | ---- | ---------------------------------------- |
| `obj` |      | value to quote (converted with `std.arg` |

**Returns:** [`str`](../api/std/str.md) - The quoted string.

```
echo (quote "hello world")
# prints: 'hello world'
```

### `split string`

Split a shell-quoted string into tokens, returning an iterator.

**Parameters:**

| Name     | Type                       | Description     |
| -------- | -------------------------- | --------------- |
| `string` | [`str`](../api/std/str.md) | string to split |

**Returns:** `Iter` yielding each argument.

```
for arg = split "echo 'hello world'"
  echo $arg
done
# prints:
# echo
# hello world
```

### `join iterable`

Join an iterable of arguments into a shell-quoted string.

**Parameters:**

| Name       | Type | Description                                   |
| ---------- | ---- | --------------------------------------------- |
| `iterable` |      | iterable of values (converted with `std.arg`) |

**Returns:** [`str`](../api/std/str.md) - Joined string with proper quoting.

```
echo $ join ["echo", "hello world"]
# prints: echo 'hello world'
```
