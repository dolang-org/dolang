# Data Structures

## Arrays (`array`)

Arrays are ordered, mutable sequences of values.

### Literals

Inline with brackets:

```
let arr = [1, 2, 3]
let mixed = [1, "hello", true, nil]
```

### Spreading

Use `...` to splice an iterable into an array literal:

```
let extras = [4, 5, 6]
let all = [1, 2, 3, ...extras]
assert_eq $all [1, 2, 3, 4, 5, 6]
```

See the [Array API](../api/std/array.md) for methods.

## Dictionaries (`dict`)

Dictionaries are **ordered**, mutable key-value mappings. They preserve
insertion order and are actually **multi-maps**: a single key can have multiple
values.

### Literals

Inline with braces:

```
let d = {name: "Alice", age: 30}
```

### Symbol Keys vs String Keys

A bare `key:` in a dict literal creates a **symbol** key:

```
let d = {name: "Alice"}
# key is the symbol :name:
```

To interpret the key as an expression instead, prefix it with `$`:

```
let key = "name"
let d = {$key: "Alice"}
# key is the string "name"
```

Constants, quoted strings, parenthesized expressions, and other literals are
automatically treated as expressions. Values are always treated as full,
whitespace-insensitive expressions.

### Non-Pair Elements

Dict literals can contain values without explicit keys. These receive
incrementing integer keys starting at 0:

```
let d = {1, foo: "bar", 3}
# d[0] == 1, d[:foo:] == "bar", d[1] == 3
```

### Ordering

Dictionaries preserve insertion order. Iteration yields entries in the order
they were inserted.

### Multi-Map Semantics

Dicts are multi-maps: a key can map to multiple values. This matters in
specific cases:

- **Construction**: Duplicate keys in a literal or spread preserve all values:

    ```
    let d = {...{a: 1}, ...{a: 2}}
    # {a: 1, a: 2}
    ```

- **Plain indexing** (`d[key]`) returns only the last (most recently inserted)
  value for a key.
- **Plain assignment** (`d[key] = value`) replaces all values for a key.
- Methods like `insert`, `get`, and `pop` respect the multi-map nature of `dict`

See the [Dict API](../api/std/dict.md) for details.

### Spreading

Spread an iterable of key/value pairs (e.g. another dict iterator) into a dict
literal:

```
let base = {name: "Alice"}
let extended = {...base, age: 30}
```

Use `kv()` when you want an ordinary iterator of 2-item sequences to spread as
key/value entries:

```
let entries = ["x=1", "y=2"].iter().map do |e| e.split "="
let dict = {...entries.kv()}
```

Spreading of dicts preserves duplicate keys and order.

## Records (`record`)

Records are similar to dicts but only support symbol and integer keys.
They allow direct field access with dot syntax:

```
let r = record name: Alice age: 30
echo $r.name # Alice
r.age = 31
```

Records support the same ordering and multi-map semantics as dicts where
applicable. They are iterable, unpackable, and support index/assignment for
their key types.

See the [Record API](../api/std/record.md) for details.

## Sets (`set`)

Sets are ordered, mutable collections with unique membership semantics.

Unlike arrays and dicts, sets do not have a dedicated literal syntax. Construct
them with the `set` type object from any iterable:

```
let empty = set()
let s = set [3, 1, 2, 1]
assert_eq [...s] [3, 1, 2]
```

Iteration preserves insertion order. Adding an existing value is a no-op and
does not move it to the end.

See the [Set API](../api/std/set.md) for methods such as `add`, `contains`,
`union`, and `diff`.

## Tuples (`tuple`)

Tuples are immutable, ordered sequences of values.

Like sets, tuples do not have a dedicated literal syntax. Construct them with
the `tuple` type object from an iterable:

```
let tup = tuple([1, 2, 3])
assert_eq $tup[1] 2
```

Some APIs produce tuples, such as key/value pair iteration:

```
for pair = {name: "Alice"}
  echo $pair[0]
  echo $pair[1]
```

Note that mutable collections may be used as `dict` keys, so `tuple` usage
is not mandatory.

See the [Tuple API](../api/std/tuple.md) for details.
