# load

The `load` module provides the ability to execute compiled Do bytecode at
runtime.

## Usage

```
import compile
let bytecode = (compile.compile "example.dol" "1 + 1").bytecode
let result = load.run(bytecode)
```

## Functions

### `run bytecode`

Executes compiled bytecode and returns the result.

**Parameters:**

| Name       | Type  | Description              |
| ---------- | ----- | ------------------------ |
| `bytecode` | `bin` | Compiled bytecode to run |

**Returns:** The result of executing the bytecode

**Errors:** Raises an error if bytecode execution fails, including:

- Invalid or corrupted bytecode
- Runtime errors in the executed code

## See Also

- [`compile`](./compile/index.md) -- Compile source code to bytecode
- [`diagnostic`](./diagnostic.md) -- Render compiler diagnostics and runtime
  errors
