# Standard Types

The standard module, `std`, declares the types that annotations most
often name. Each has its own API reference entry; this page gives an overview of
the special types the checker understands, the projections that compute types,
and the protocols that relate the standard collections.

## Special Types

Some standard types have meanings that no declaration could give them:

| Type                         | Describes                                                                 |
| ---------------------------- | ------------------------------------------------------------------------- |
| [`Value`](std.Value)         | Every value; few operations are available on it without narrowing         |
| [`Never`](std.Never)         | No values; the result of a function that never returns                    |
| [`Unknown`](std.Unknown)     | Unchecked values; it converts to and from every type                      |
| [`Type[C]`](std.Type)        | The class object of `C` or of any subclass                                |
| [`Func`](std.Func)           | Any function                                                              |
| [`Phantom`](std.Phantom)     | Nothing at runtime; a private field of this type marks a class's variance |

`Value` is a class, and the supertype of every other class. The members it
declares, such as `(hash)`, `(eq)`, and `(str)`, are ones every value has.
`Never` is the empty union, `Union[]`. See
[Class Objects](./classes.md#class-objects) for how `Type[C]` relates to a
class's own name.

Every function type fits `Func`. A class that declares `(call)` names `Func`
among its supertypes to be passed as a function with its `(call)`'s signature.

A [constant type](./syntax.md#constants) fits the class of its value: `:A:` is a
[`Sym`](std.Sym), `"a"` a [`Str`](std.Str), `1` an [`Int`](std.Int), `true` a
[`Bool`](std.Bool), and `nil` a [`Nil`](std.Nil).

## Projections

Projections are aliases that compute a type or schema from their arguments:

| Alias                          | Computes                                                                   |
| ------------------------------ | -------------------------------------------------------------------------- |
| [`Union`](std.Union)           | The union of its type arguments, including expanded schemas                |
| [`Keys`](std.Keys)             | The union of a schema's keys                                               |
| [`Values`](std.Values)         | The union of a schema's values                                             |
| [`Entries`](std.Entries)       | The union of a key and value tuple for each item                           |
| [`IndexItem`](std.IndexItem)   | What a read at a key may give                                              |
| [`AssignItem`](std.AssignItem) | What a write at a key must fit                                             |
| [`InsertItem`](std.InsertItem) | What a value added at a repeatable key must fit                            |
| [`PushItem`](std.PushItem)     | What a position appended past the end must fit                             |
| [`Deletable`](std.Deletable)   | The schema, provided removing the items a key selects leaves a value of it |
| [`Clearable`](std.Clearable)   | The schema, provided removing every item leaves a value of it              |
| [`Fields`](std.Fields)         | The schema of a class's readable public members                            |
| [`MutFields`](std.MutFields)   | The schema of a class's settable public members                            |
| [`Repeat`](std.Repeat)         | The schema of a given number of positions of one type                      |

The standard collection protocols use them in their signatures: `Index[S]`
reads a key `K` as `IndexItem[S, K]`, and `MutMixed[S]`'s `delete` requires
`Deletable[S, K]`. See [Computing Types from
Schemas](./schemas.md#computing-types-from-schemas) for an example.

## Protocols

Some of the language's [special
methods](../language/classes.md#special-method-reference) have a protocol
describing a value that implements them. These protocols are generic in what
the method produces or accepts:

| Protocol                              | Special method | Required by                   |
| ------------------------------------- | -------------- | ----------------------------- |
| [`BaseIterable[T]`](std.BaseIterable) | `(iter)`       | `for`                         |
| [`BaseIter[T]`](std.BaseIter)         | `(next)`       | The iterator `(iter)` returns |
| [`BaseSinkable[T]`](std.BaseSinkable) | `(sink)`       | Redirecting output to a value |
| [`BaseSink[T]`](std.BaseSink)         | `(put)`        | [`put`](strand.put)           |
| [`Index[S]`](std.Index)               | `(index)`      | `value[key]`                  |
| [`Assign[S]`](std.Assign)             | `(assign)`     | `value[key] = item`           |
| [`Spread[S]`](std.Spread)             | `(spread)`     | `...value`                    |
| [`Unpack[S]`](std.Unpack)             | `(unpack)`     | Destructuring patterns        |

`Index`, `Assign`, `Spread`, and `Unpack` take a [schema](./schemas.md) of the
available item, so `Index[{name: Str, ?port: Int}]` reads `name` as a
`Str`.

[`Iterable`](std.Iterable), [`Iter`](std.Iter), [`Sinkable`](std.Sinkable), and
[`Sink`](std.Sink) are ordinary mixable classes that supply methods such as
`map` and `premap` in terms of the base protocol. A class that only implements
`(iter)` is a `BaseIterable` but not an `Iterable` unless it explicitly
inherits from `Iterable`.

### Abstract Collection Protocols

These protocols describe operations available on a collection type. A read-only
protocol has an assigning counterpart, which only assigns, and a mutable one,
which reads, assigns, inserts and removes:

| Reads                   | Assigns                             | Mutable                       |
| ----------------------- | ----------------------------------- | ----------------------------- |
| [`Seq[T]`](std.Seq)     | [`AssignSeq[T]`](std.AssignSeq)     | [`MutSeq[T]`](std.MutSeq)     |
| [`Mixed[S]`](std.Mixed) | [`AssignMixed[S]`](std.AssignMixed) | [`MutMixed[S]`](std.MutMixed) |
| [`Map[S]`](std.Map)     | [`AssignMap[S]`](std.AssignMap)     | [`MutMap[S]`](std.MutMap)     |

- A `Seq[T]` iterates as `T`, has a `len`, and is indexed by integers and
  ranges.
- A `Mixed[S]` has positional and keyed items, as a record does. It iterates as
  its entries, and indexes, spreads, and destructures as `S`.
- A `Map[S]` is a `Mixed[S]` whose items are all keyed, with dictionary methods
  such as `get`, `keys`, and `values`.
- An assigning protocol sets items without reading them, so a value may read
  one schema and assign another, as a [`fields`](std.fields) proxy does.

The rest that a pattern captures is a [`SeqRest`](std.SeqRest) when it iterates
as values, and a [`MixedRest`](std.MixedRest) when it iterates as entries.

### Covariant Collections

Each mutable collection class has a read-only view which is the same class at
runtime. A view is covariant in its type parameters, so `Array[Str]` is a
`BaseArray[Str | Sym]` but not an `Array[Str | Sym]`:

| Class                        | Read-only view                       |
| ---------------------------- | ------------------------------------ |
| [`Array[T]`](std.Array)      | [`BaseArray[T]`](std.BaseArray)      |
| [`Dict[S]`](std.Dict)        | [`BaseDict[S]`](std.BaseDict)        |
| [`Record[...S]`](std.Record) | [`BaseRecord[...S]`](std.BaseRecord) |
| [`Struct[...S]`](std.Struct) | [`BaseStruct[...S]`](std.BaseStruct) |
