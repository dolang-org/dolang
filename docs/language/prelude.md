# Prelude

The following items are available globally in every Do program without any
`import` statement.

The `dolang` executable layers its
[shell prelude](../shell/index.md#shell-prelude) on top of this core prelude.
Embedded runtimes may provide a different additional prelude.

## `std`

| Name                   | Description                        |
| ---------------------- | ---------------------------------- |
| [`Array`](std.Array)   | [`Array`](std.Array) type          |
| [`array`](std.array)   | Variadic array factory             |
| [`Bin`](std.Bin)       | [`Bin`](std.Bin) type              |
| [`Bool`](std.Bool)     | [`Bool`](std.Bool) type            |
| [`bool`](std.bool)     | Truthiness coercion                |
| [`class`](std.class)   | Inherited class member decorator   |
| [`dbg`](std.dbg)       | Debug representation               |
| [`Dict`](std.Dict)     | [`Dict`](std.Dict) type            |
| [`dict`](std.dict)     | Variadic dictionary factory        |
| [`Float`](std.Float)   | [`Float`](std.Float) type          |
| [`float`](std.float)   | Numeric coercion and parsing       |
| [`Func`](std.Func)     | [`Func`](std.Func) type            |
| [`getter`](std.getter) | Class field getter decorator       |
| [`Int`](std.Int)       | [`Int`](std.Int) type              |
| [`int`](std.int)       | Integer coercion and parsing       |
| [`Range`](std.Range)   | [`Range`](std.Range) type          |
| [`Record`](std.Record) | [`Record`](std.Record) type        |
| [`record`](std.record) | Variadic record factory            |
| [`Set`](std.Set)       | [`Set`](std.Set) type              |
| [`setter`](std.setter) | Class field setter decorator       |
| [`static`](std.static) | Uninherited class member decorator |
| [`Str`](std.Str)       | [`Str`](std.Str) type              |
| [`str`](std.str)       | Textual representation             |
| [`Sym`](std.Sym)       | [`Sym`](std.Sym) type              |
| [`sym`](std.sym)       | Symbol interning                   |
| [`Tuple`](std.Tuple)   | [`Tuple`](std.Tuple) type          |
| [`tuple`](std.tuple)   | Variadic tuple factory             |
| [`Type`](std.Type)     | [`Type`](std.Type) type            |
| [`type`](std.type)     | Type query and test function       |

## `strand`

The module itself is imported, along with these functions:

| Name                          | Description                         |
| ----------------------------- | ----------------------------------- |
| [`fork`](strand.fork)         | Executes blocks concurrently        |
| [`pipeline`](strand.pipeline) | Connects concurrent pipeline stages |
| [`stream`](strand.stream)     | Creates a background stream strand  |
| [`put`](strand.put)           | Writes to the strand-local output   |
| [`spawn`](strand.spawn)       | Creates a background strand         |
