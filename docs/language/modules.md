# Modules

Do has a module system.

## Importing Modules

### Import the whole module

```
import math
math.add 1 2
```

### Import with an alias

```
import math: m
m.add 1 2
```

### Import specific names

```
import math:
  - add
  - subtract

add 1 2
```

### Import with renaming

```
import
  math:
    add: plus

plus 1 2
```

### Combined forms

```
import
  math:
    - add
    subtract: minus
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

Only `pub` definitions are visible when a module is imported.

## Module Resolution

Modules are resolved according to the host application. See the [Shell
Guide](../shell/modules.md) for details on module path configuration for
`dolang`.
