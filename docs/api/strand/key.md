# Key

Each `Key` instance stores a per-strand value which is inherited by scoped
strands. Background strands created by
[`spawn`](./index.md#spawn-func) and [`stream`](./index.md#stream-func) start
fresh and do not inherit values.

## Constructor

### `Key()`

Creates a new strand-local key.

```
let key = strand.Key()
```

## Fields

### `value`

Gets or sets the value associated with this key in the current strand.

Defaults to `nil` when the key has not been set in the current
strand.

```
let key = strand.Key()
assert_eq $key.value nil

key.value = "parent"
assert_eq $key.value "parent"

let results = strand.fork
  do
    assert_eq $key.value "parent"
    key.value = "child"
    key.value

assert_eq $results ["child"]
assert_eq $key.value "parent"
```
