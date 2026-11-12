# Data Structures

## Arrays (`Array`)

Arrays are ordered, mutable sequences of values.

### Literals

Inline with brackets:

```
let arr = [1, 2, 3]
let mixed = [1, "hello", true, nil]
```

`...` splices another collection or an iterator into the literal; see
[Spreading](#spreading):

```
let extras = [4, 5, 6]
let all = [1, 2, 3, ...extras]
assert_eq $all [1, 2, 3, 4, 5, 6]
```

See the [Array API](std.Array) for methods.

## Tuples (`Tuple`)

Tuples are immutable, ordered sequences of values.

Comma-separated items within parentheses construct a tuple. A single item needs
a trailing comma to distinguish it from mere grouping:

```
let tup = (1, "two", true)
assert_eq $tup[1] two
let empty = ()
let single = (1,)
```

Spread a sequence into a tuple with `...`:

```
let rest = [2, 3]
assert_eq (1, ...rest) (Tuple [1, 2, 3])
```

The `Tuple` type object also builds a tuple from an iterable:

```
let tup = Tuple [1, 2, 3]
```

See the [Tuple API](std.Tuple) for details.

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

### Keys Only

A dict holds only keyed items. A brace literal with values that have no keys
makes a [record](#records-record) instead:

```
let r = {1, foo: "bar", 3}
assert (type r Record)
```

Integer keys are ordinary keys, never positions, so a pattern names them as
keys:

```
let d = {0: "a", 1: "b"}
let 0: first 1: second = d
```

Spreading positional items into a dict raises an error. Adapt pairs with
`kv()` to build a dict from them:

```
let d = Dict $ [(0, "a"), (1, "b")].kv()
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

## Packs (`Pack`)

A `Pack` is an immutable sequence of positional items and symbol-keyed items,
like a tuple with keys. It is the type of a `...` rest capture and of keyed
tuple literals. Read an item by indexing with its symbol or position; negative
positions count from the end:

```
let r = (name: "Alice", age: 30)
echo $r[:name:] # Alice
```

Parentheses make a `Pack` rather than a tuple when at least one key is
specified. Positional items get integer keys counting from 0:

```
let name = "Alice"
let r = (:name, age: 30)
let mixed = (1, 2, tag: "x")
assert_eq $mixed[1] 2
```

The `pack` function builds a `Pack` from its arguments:

```
let r = pack name: Alice age: 30
```

A key may repeat, and a lookup finds its latest value. Iterating a `Pack`
yields key/value tuples in order, with a positional item's position as its key.

Use `with` to replace keys and `without` to remove them in a new `Pack`:

```
let updated = r.with status: ready
let cleaned = updated.without :status:
```

`with` keeps each replaced key at its first position, removes later occurrences,
and appends new keys. `without` removes all occurrences of the supplied keys.
Both leave the original unchanged.

Spreading a repeated key into a call binds its first value to a parameter of
that name, and a `**` or `...` rest takes the later ones in order. Without a
rest, the repeats raise [UnexpectedKeyError](std.UnexpectedKeyError).

See the [Pack API](std.Pack) for details.

## Records (`Record`)

A `Record` is a mutable sequence of positional items and symbol-keyed items:
the mutable counterpart of `Pack`, as an array is of a tuple. A brace literal
with positional items makes one, as does a vertical data block with both dash
items and keys:

```
let r = {1, 2, tag: "x"}
r.push 3
r[:tag:] = "y"

let task = $
  - build
  help: compile the sources
```

The `record` function builds a record from its arguments, and `Record` from
one spreadable source:

```
assert_eq (record 1 2 tag: x) {1, 2, tag: "x"}
assert_eq (Record (1, tag: "x")) {1, tag: "x"}
```

A spread alone doesn't make a record, so `{...source}` is still a dict, which
raises an error if `source` has positional items. Use `Record source` for
those.

The keys of a record literal must be written symbols, `name: value`, `:name`
or `:name:: value`, since the literal is a record whatever they evaluate to.
Spread a dict to add a computed key:

```
let key = :tag:
let r = {1, ...{$key: "x"}}
```

An integer always selects a positional item and a symbol always selects a key,
so the two can't be confused. Negative positions count from the end.

Positional items behave like an array's: `insert` and `pop` shift the items
after the one they add or remove, and `push` appends.

```
let r = {"a", "b", "c"}
r.insert 0 z
assert_eq (r.pop -1) c
assert_eq $r[0] z
```

Keys follow a dict's [multi-map rules](#multi-map-semantics). Indexing reads a
key's latest value, assignment replaces all its values, `insert` adds another
value, and `get` and `pop` take an instance that selects one.

Iterating a record yields `(index, value)` for each positional item and
`(key, value)` for each keyed item, in the order they were added. Spreading
and unpacking give positional items positionally and keyed items as keys.
Equality compares the items in that order, so the same items added in a
different order are unequal. Hashing follows the same order, as a dict's does.

The type `Record` takes its items as type arguments, as `Pack` does:
`Record[Str, tag: Str]`. Expand a schema for optional or repeated items, as in
`Record[Str, ...{*Int, ?tag: Str}]`.

`BaseRecord` is the read-only view of a record. A parameter that only reads
takes a `BaseRecord`, which accepts records with narrower items, as
[`BaseDict`](std.BaseDict) does for dicts.

See the [Record API](std.Record) for details.

## Structs (`Struct`)

Structs are mutable sequences of positional items and symbol-keyed items,
like a `Pack` whose keys are fields. A struct has no methods of its own, so a
name reads or writes the item it keys:

```
let s = struct host: localhost port: 22
s.port = 2222
s.user = "root"
echo $s.host # localhost
```

Reading a name with no item raises [FieldError](std.FieldError). Assigning
one replaces its item in place, or adds an item at the end. Calling a name
calls its item. Indexing reads and writes items by symbol or position, and
negative positions count from the end.

The `struct` function builds a struct from its arguments, and `Struct` from one
spreadable source, as `Pack` does:

```
let s = Struct {host: "localhost", port: 22}
```

A key appears at most once, so building a struct with a repeated key raises
[ValueError](std.ValueError). Iterating, spreading and unpacking work as for an
`Pack`.

Since every plain name is an item, operations take the struct as an argument
to a class method instead:

```
let port = Struct.get $s :port: default: 22
if (Struct.contains s :user:)
  echo $s.user
```

The type `Struct` takes its items as type arguments, as `Pack` does, with a
schema expanded for optional items. Reading or writing a name they don't admit
is an error:

```
def connect target @ Struct[host: Str, port: Int, ...{?user: Str}]
  echo "$(target.host):$(target.port)"
```

`BaseStruct` is the read-only view of a struct. A parameter that only reads
takes a `BaseStruct`, which accepts structs with narrower items, as
[`BaseDict`](std.BaseDict) does for dicts.

See the [Struct API](std.Struct) for details.

## Sets (`Set`)

Sets are ordered, mutable collections with unique membership semantics.

Sets do not have a dedicated literal syntax. Construct them with the `Set` type
object from any iterable:

```
let empty = Set()
let s = Set [3, 1, 2, 1]
assert_eq [...s] [3, 1, 2]
```

Iteration preserves insertion order. Adding an existing value is a no-op and
does not move it to the end.

See the [Set API](std.Set) for methods such as `add`, `contains`,
`union`, and `diff`.

## Spreading

`...` spreads a value into an array, tuple, dictionary, record, or pack literal,
or into a call's arguments. What it contributes depends on both the value and
the context:

| Context                     | Spread input                          | Behavior                                                    |
| --------------------------- | ------------------------------------- | ----------------------------------------------------------- |
| Arguments, `Pack` or record | Array, tuple, set, [`Iter`](std.Iter) | Positional items, including pair elements                   |
| Arguments, `Pack` or record | `Pack`, record, struct                | Positional items stay positional, symbol keys are keywords  |
| Arguments, `Pack` or record | Dict                                  | Symbol keys are keywords; another key raises an error       |
| Array or tuple              | Array, tuple, set, [`Iter`](std.Iter) | Positional elements                                         |
| Array or tuple              | `Pack`, record, struct                | Positional elements; a keyed entry raises an error          |
| Array or tuple              | Dict                                  | Raises an error, since every entry is keyed                 |
| Dictionary                  | Array, tuple, set, [`Iter`](std.Iter) | Raises an error, since every element is positional          |
| Dictionary                  | Dict, `Pack`, record, struct          | Keyed entries; a positional item raises an error            |
| Any                         | `kv()` adapter                        | Keyed entries; arrays and tuples reject them                |

Spread a dictionary directly to preserve its keys:

```
let base = {name: "Alice"}
let extended = {...base, age: 30}
```

Spreading dicts preserves duplicate keys and order. Spreading any `Iter`,
including a dict or `Pack` pair iterator, gives its elements positionally, so
`iter()` gives an array of a dict's key/value pairs:

```
let d = {a: 1, b: 2}
assert_eq [...d.iter()] [(:a:, 1), (:b:, 2)]
```

Use `kv()` when you want an iterable of pairs to spread as key/value entries:

```
let entries = [("x", 1), ("y", 2)]
let dict = {...entries.kv()}
```

The adapter returned by `kv()` emits keyed entries in every spread context.
It supports spreading only: it cannot be iterated or used with iterator methods.
A keyed spread into an array or tuple raises an error.

An `Iterable` without spread support must first be converted to an iterator
with `iter()`. A rest bound by a pattern can be spread repeatedly, unless it is
the rest of an iterator.
