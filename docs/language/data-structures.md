# Data Structures

## Arrays (`Array`)

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

See the [Array API](std.Array) for methods.

## Dictionaries (`Dict`)

Dictionaries are **ordered**, mutable key-value mappings. They preserve
insertion order and are actually **multi-maps**: a single key can have multiple
values.

### Literals

Inline with braces:

```
let d = {name: "Alice", age: 30}
```

### Symbol Keys vs String Keys

A literal `key:` in a dict literal creates a **symbol** key:

```
let d = {name: "Alice"}
# key is the symbol :name:
```

To interpret the key as a variable instead, prefix it with `$`:

```
let key = "name"
let d = {$key: "Alice"}
# key is the string "name"
```

Constants, quoted strings, parenthesized expressions, and other literals are
automatically treated as expressions. Values are always treated as full,
whitespace-insensitive expressions.

### Positional Elements

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

See the [Dict API](std.Dict) for details.

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
let entries = ["x=1", "y=2"].map do |e| e.split "="
let dict = {...entries.kv()}
```

Spreading of dicts preserves duplicate keys and order.

## Records (`Record`)

Records are immutable product values with symbol and integer keys. They allow
direct field access with dot syntax:

```
let r = (name: "Alice", age: 30)
echo $r.name # Alice
```

Parentheses make a record rather than a tuple when they hold at least one static
key: `key: value` or the ditto shorthand `:name`. Positional items get integer
keys counting from 0:

```
let name = "Alice"
let r = (:name, age: 30)
let mixed = (1, 2, tag: "x")
assert_eq $mixed[1] 2
```

The `record` function builds a record from its arguments:

```
let r = record name: Alice age: 30
```

Records support the same ordering and multi-map semantics as dicts where
applicable. They are iterable, unpackable, and support indexing for their key
types. Build a changed record by spreading the original into a new record:

```
let updated = (...r, age: 31)
```

Use a class when named fields need to be mutable.

See the [Record API](std.Record) for details.

## Sets (`Set`)

Sets are ordered, mutable collections with unique membership semantics.

Unlike arrays and dicts, sets do not have a dedicated literal syntax. Construct
them with the `Set` type object from any iterable:

```
let empty = Set()
let s = Set [3, 1, 2, 1]
assert_eq [...s] [3, 1, 2]
```

Iteration preserves insertion order. Adding an existing value is a no-op and
does not move it to the end.

See the [Set API](std.Set) for methods such as `add`, `contains`,
`union`, and `diff`.

## Tuples (`Tuple`)

Tuples are immutable, ordered sequences of values.

Write a tuple as comma-separated items in parentheses. A single item needs a
trailing comma, since `(x)` only groups `x`:

```
let tup = (1, "two", true)
assert_eq $tup[1] two
let empty = ()
let single = (1,)
```

Spread an iterable into a tuple with `...`:

```
let rest = [2, 3]
assert_eq (1, ...rest) (Tuple [1, 2, 3])
```

The `Tuple` type object also builds a tuple from an iterable:

```
let tup = Tuple [1, 2, 3]
```

At statement level, whitespace separates arguments, so `f (1, 2)` passes one
tuple while `f(1, 2)` passes two arguments. Within a full expression,
whitespace is insignificant and both are C-style calls, so write
`(f((1, 2)))` to pass a lone tuple.

Some APIs produce tuples, such as key/value pair iteration:

```
for pair = {name: "Alice"}
  echo $pair[0]
  echo $pair[1]
```

Note that mutable collections may be used as `dict` keys, so `tuple` usage is
not mandatory as in Python.

See the [Tuple API](std.Tuple) for details.
