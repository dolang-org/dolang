# compile

The `compile` module provides programmatic access to the Do compiler, allowing
you to compile source code to bytecode at runtime.

## Usage

```
let source = "let x = 1 + 2"
let result = compile "example.dol" source
```

## Functions

### `compile path source :module? :prelude?`

Compiles Do source code and returns a structured result.

#### Parameters

| Name      | Type        | Description                                          |
| --------- | ----------- | ---------------------------------------------------- |
| `path`    | `str`       | Source path (for debug information)                  |
| `source`  | `str`/`bin` | Source code to compile                               |
| `module`  | `str`       | Optional. Compile in module mode with the given name |
| `prelude` | various     | Optional. Additional prelude imports to include      |

#### Returns

[`Result`](./result.md), containing the compile output and any
diagnostics emitted during compilation.

#### Errors

| Exception    | Condition                         |
| ------------ | --------------------------------- |
| `TypeError`  | `source` is not `str` or `bin`    |
| `TypeError`  | `module` is present but not `str` |
| `ValueError` | `prelude` is malformed            |

Ordinary compiler diagnostics from the compiled source are returned on the
result object instead. Unexpected compiler failures that are not ordinary source
diagnostics are also raised as errors.

**Compilation Modes:**

- **Script mode** (default): Compiles as a script. The result of running the
  bytecode is the value of the final expression or any early return.
- **Module mode**: When `module` is specified, compiles as a named module.
  The result of running the bytecode is a module object containing exported
  bindings (or the value of an early return).

**Prelude Format:**

The `prelude` parameter specifies additional imports to prepend to the source.
It accepts the same logical import shapes used by the LSP prelude settings:

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
