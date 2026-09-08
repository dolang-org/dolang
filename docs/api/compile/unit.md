# Unit

Provides diagnostics and document nodes for one source file before emitting
bytecode. A unit borrows and pins the source passed to `compile`.

## Methods

### `diagnostics()`

Returns a fresh iterator of [`Diagnostic`](./diagnostic.md) objects.

### `nodes()`

Returns a fresh iterator of `[NodeId, Node]` pairs. Nodes expose `parent`,
[`span`](./span.md), and `doc` -- the span of the comment block documenting the
node, or `nil` where none attaches. Concrete node types add projections such as
`name`, `is_pub`, `default`, `target`, and `supers`.

Empty unless the unit was compiled with `document: true`. An enabled unit always
contains one `Root`, even for empty source. It spans the complete source, has no
parent, and parents every other top-level node. Its `doc` is the initial comment
block on the first line, or immediately after an initial `#!` line.

### `node id`

Looks up a node by `NodeId`, returning `nil` when the ID belongs to another
unit.

### `emit()`

Consumes the unit and returns bytecode as [`Bin`](../std/bin.md).

Throws `std.CompileError` when compilation failed. After emission, using the
unit, its node iterators, or its nodes throws `std.StateError`.

## Example

```
let unit = compile "example.dol" "pub let answer = 42\n" document: true
for id node = unit.nodes()
  echo $id $node.span
let bytecode = unit.emit()
```
