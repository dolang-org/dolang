# load

Execute Do bytecode and register runtime import handlers.

## Functions

### `run bytecode`

Executes compiled Do bytecode.

**Parameters:**

| Name       | Type                   | Description          |
| ---------- | ---------------------- | -------------------- |
| `bytecode` | [`bin`](../std/bin.md) | Compiled Do bytecode |

**Returns:** The result of executing the bytecode.

**Errors:**

- Raises a type error if `bytecode` is not `bin`
- Propagates bytecode verification and execution errors

```
import compile
import load

let result = load.run $ (compile.compile "example.dol" "(1 + 1)").bytecode
assert_eq $result 2
```

### `import_handler callback`

Registers a module import handler.

Handlers are tried after native modules and cached Do modules. The first
handler that returns successfully supplies the imported value.

To decline a module name, raise
[`ImportError`](../std/import-error.md). Any other error aborts the import.

**Parameters:**

| Name       | Type                  | Description                          |
| ---------- | --------------------- | ------------------------------------ |
| `callback` | [`func`](../std/func.md) | Called with the requested module name |

**Returns:** [`ImportHandler`](./importhandler.md)

```
let handle = load.import_handler do |name|
  if (name == "demo")
    record answer: 42
  else
    throw std.ImportError(name)

import demo
assert_eq $demo.answer 42
handle.unregister()
```
