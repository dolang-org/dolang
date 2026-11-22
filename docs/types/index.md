# Overview

Do supports gradual type checking in the manner of Python or TypeScript. You can
annotate variables, parameters, return values, and fields with types; the
checker reports code that would misuse them before the script runs. "Gradual"
means that annotations are optional: where type information is missing, the
checker leaves code unchecked instead of rejecting it, so types can be added to
code incrementally as it evolves.

```
def connect :host@Str = "localhost" :port@Int = 8080 -> Str
  "$host:$port"

let address @ Str = connect port: 5432
```

Annotations and other checker-only declarations have *no effect* on program
behavior. They describe but do not influence how it runs.

`dolang --check` runs the checker without executing the script; see
[Checking Scripts](../shell/index.md#checking-scripts). The
[language server](../tooling/lsp.md) reports the same diagnostics as you edit.

## Features

- **Classes and protocols.** A class's type is nominal, following its declared
  supertypes. A [protocol](./classes.md#protocols) is structural: any class with
  the right members conforms, whether or not it names the protocol.
- **Unions and narrowing.** A [union](./syntax.md#unions) admits any of its
  members, and conditions and patterns [narrow](./checking.md#narrowing) a
  variable's type on the branches they select. Constants such as `:STABLE:` and
  `"x"` are types of their own, so unions of them act as enumerations.
- **Schemas.** A [schema](./schemas.md) describes the items in a tuple,
    dictionary, argument pack, or other type. They support keyed and positional
    items with optionality and repetition quantifiers.
- **Generics.** Functions, classes, and aliases take
  [binders](./generics.md) with bounds and defaults, including variadic binders
  that stand for whole schemas. The checker infers each class binder's
  [variance](./checking.md#generic-variance) from its members.
- [**Function types**](./functions.md) describe function signatures, including
  parameter schemas, return types, and streaming input/output types.
  [Overloads](./functions.md#overloads) give one function
  several signatures.
- **Recursive aliases.** An [alias](./aliases.md) may refer to itself, as a
  JSON type does, and recursive types are compared by structure.
- **Initialization.** The checker follows each `(init)` to verify that
  [fields are assigned](./classes.md#object-initialization) before the object
  can be observed.

## Gradual and Strict Checking

By default, unknown information leaves operations unchecked rather than making
every unannotated program an error. A file can opt into [strict
checking](./strict.md), which requires annotations on named functions, fields,
and public interfaces, gives omissions conservative types instead of unknown
ones, and warns about code the checker can't verify because of implementation
restrictions.
