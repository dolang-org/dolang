# compile

The `compile` module provides programmatic access to the Do compiler, allowing
you to compile source code to bytecode at runtime.

## Usage

```
let source = "let x = 1 + 2"
let result = compile "example.dol" source
```

## Functions

### `compile path source :module? :prelude? :recover? :document?`

Parses and elaborates Do source code into a staged compilation unit.

#### Parameters

| Name       | Type        | Description                                          |
| ---------- | ----------- | ---------------------------------------------------- |
| `path`     | `Str`       | Source path (for debug information)                  |
| `source`   | `Str`/`Bin` | Source code to compile                               |
| `module`   | `Str`       | Optional. Compile in module mode with the given name |
| `prelude`  | various     | Optional. Additional prelude imports to include      |
| `recover`  | `bool`      | Continue parsing after syntax errors                 |
| `document` | `bool?`     | Build document nodes. Defaults to `false`            |

##### Compilation Modes

- **Script mode** (default): Compiles as a script. The result of running the
  bytecode is the value of the final expression or any early return.
- **Module mode**: When `module` is specified, compiles as a named module.
  The result of running the bytecode is a module object containing exported
  bindings (or the value of an early return).

##### Prelude Format

The `prelude` parameter specifies additional imports to prepend to the source.
It accepts the same logical import shapes used by the LSP prelude settings.

#### Returns

[`Unit`](./unit.md), which exposes diagnostics and, with `document: true`,
document nodes before emission.

#### Errors

| Exception    | Condition                         |
| ------------ | --------------------------------- |
| `TypeError`  | `source` is not `Str` or `Bin`    |
| `TypeError`  | `module` is present but not `Str` |
| `ValueError` | `prelude` is malformed            |

Ordinary compiler diagnostics are available from `Unit.diagnostics()`.

#### Example

```
# Modules
compile "test.dol" $source
  prelude:
    - sys
    - fs

# Module with alias
compile "test.dol" $source
  prelude:
    sys: shell

# Import specific items
compile "test.dol" $source
  prelude:
    sys:
      - echo
      - exit

# Items with aliases
compile "test.dol" source
  prelude:
    sys:
      echo: echo_alias
```

## See Also

- [`load`](../load/index.md) -- Run compiled bytecode and register import
  handlers
