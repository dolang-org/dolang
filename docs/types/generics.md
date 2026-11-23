# Generics

`[]` directly after the name of a `def`, `class`, or `@let` declares binders:
names that stand for types within the declaration. `@[]` declares them for a
[generic function type](./functions.md#generic-function-types).

```
def first[T] items @ Array[T] -> T
  items[0]

class Table[K, V]
  pub field rows @ Dict[K, V] = {}

@let Pair[T] = Tuple[T, T]
```

A use of the declaration supplies the binders' types. A class or alias takes
them as [type arguments](./syntax.md#type-arguments), as in `Table[Str, Int]`.
A call of a generic function infers them from its arguments; see
[Inference](./checking.md#inference). How a class's binders affect which of its
types fit each other is described under
[Generic Variance](./checking.md#generic-variance).

## Binder Forms

| Binder    | Binds                                                       |
| --------- | ----------------------------------------------------------- |
| `name`    | A positional type argument                                  |
| `:name`   | A key type argument                                         |
| `...name` | Any number of further positional and key type arguments     |
| `*name`   | Any number of further positional type arguments             |
| `**name`  | Any number of further key type arguments                    |

The last three are variadic binders. Binders of any form may appear in any
order, and a declaration may have several variadic binders:

```
def apply[R, *Ps, **Ks] func@((*Ps, **Ks) -> R) *args@Ps **kw@Ks -> R
  func ...args ...kw
```

## Bounds and Defaults

`@` gives a binder a bound and `=` gives it a default; both are type
expressions. Variadic binders cannot have defaults.

```
def lookup[K @ (Str | Sym), V = nil] key@K -> V
  nil
```

## Schema Binders

A binder other than a variadic one takes a single argument: a type, unless a
schema bound such as `S @ {...}` makes it take a [schema](./schemas.md). A
variadic binder instead collects the remaining arguments into a schema. The two
differ only in the arguments they accept; within the declaration, either binder
is a schema. Like any schema, it becomes type arguments only through
[inclusion](./schemas.md#type-arguments), so `class Tuple[*Ts]` is passed on as
`Tuple[...Ts]`. See [Kinds](./checking.md#kinds) for how the checker
determines which binders are schemas.

A rest binding can [map a type](./annotations.md#rest-bindings) over a schema
binder, giving each argument it collects its own type.
