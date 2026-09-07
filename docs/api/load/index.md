# load

Execute Do bytecode and register runtime import handlers.

## Functions

### `import name`

Imports a module through the VM's ordinary global import resolution.

Use this from a program-specific importer to explicitly fall back to native
modules, cached modules, and registered import handlers.

#### Parameters

| Name   | Type                   | Description           |
| ------ | ---------------------- | --------------------- |
| `name` | [`Str`](../std/str.md) | Module name to import |

#### Returns

The imported module value.

#### Errors

| Exception     | Condition                       |
| ------------- | ------------------------------- |
| `TypeError`   | `name` is not `Str`             |
| `ImportError` | No global importer accepts name |

#### Example

```
import load:
  import: global_import

let importer = do |name|
  if (name == "virtual")
    record answer: 42
  else
    global_import $name

load.run $bytecode importer: $importer
```

### `import_handler callback`

Registers a module import handler.

Handlers are tried after native modules and cached Do modules. The first
handler that returns successfully supplies the imported value.

To decline a module name, raise
[`ImportError`](../std/import-error.md). Any other error aborts the import.

#### Parameters

| Name       | Type                     | Description                           |
| ---------- | ------------------------ | ------------------------------------- |
| `callback` | [`Func`](../std/func.md) | Called with the requested module name |

#### Returns

[`ImportHandler`](./importhandler.md)

#### Example

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

### `run bytecode :importer?`

Executes compiled Do bytecode.

#### Parameters

| Name       | Type                      | Description                      |
| ---------- | ------------------------- | -------------------------------- |
| `bytecode` | [`Bin`](../std/bin.md)    | Compiled Do bytecode             |
| `importer` | [`Func`](../std/func.md)? | Program-specific module importer |

The optional importer belongs to the loaded program. It remains active for
functions and modules retained after `run` returns. Each import calls it
directly, bypassing native modules, the import cache, and registered import
handlers. Results are not cached, and `ImportError` is returned without
automatic fallback.

#### Returns

The result of executing the bytecode.

#### Errors

| Exception   | Condition                                |
| ----------- | ---------------------------------------- |
| `TypeError` | `bytecode` is not `Bin`                  |
| Various     | Bytecode verification or execution fails |

#### Example

```
import compile
import load

let result = load.run $ (compile.compile "example.dol" "(1 + 1)").emit()
assert_eq $result 2
```
