# Modules

Do modules are named namespaces of values. A module can export functions,
classes, constants, and other public bindings.

## Importing Modules

### Whole-Module Imports

Import a module by name:

```
import math
math.add 1 2
```

Module names may be dotted:

```
import build.tools
build.tools.compile sources
```

When a dotted module name is imported without renaming, the local binding is
the first component. That binding is a namespace object whose nested fields
mirror the rest of the dotted name.

```
import build.tools
import build.images

build.tools.compile sources
build.images.pack rootfs
```

Conceptually, `import build.tools` binds a local `build` object, then inserts
the imported module at `build.tools`. A later `import build.images` extends the
same local `build` namespace instead of replacing it.

### Renaming

Use `module: alias` to bind the imported module directly under a different
local name:

```
import math: m
m.add 1 2
```

This also works with dotted module names:

```
import build.tools: tools
tools.compile sources
```

Unlike `import build.tools`, this binds `tools` directly to the imported module.
It does not create a local `build` namespace object.

### Importing Specific Items

Import selected public items from a module:

```
import math:
  - add
  - subtract

add 1 2
```

### Renaming Imported Items

Rename individual imported items:

```
import math:
  add: plus

plus 1 2
```

The left-hand side is the exported name in the module; the right-hand side is
the local binding.

### Combined Forms

```
import math:
  - add
  subtract: minus
```

The vertical form is useful when importing multiple modules at once:

```
import
  build.tools
  build.images: images
  build.deploy:
    - push
    status: deploy_status
```

## Exporting with `pub`

Mark definitions as public with `pub`:

```
pub def helper x
  (x + 1)

pub let VERSION = "1.0"

# Private (not exported)
def internal_detail
  42
```

Only `pub` items are visible when a module is imported.

## Module Resolution

Modules are resolved according to the host application. See the [Shell
Guide](../shell/modules.md) for details on module path configuration for
`dolang`.
