# ImportHandler

Handle returned by [`load.import_handler`](./index.md#import_handler-callback).

## Methods

### `unregister()`

Removes this handler from the runtime import handler registry.

Calling `unregister()` more than once has no effect.

```
let handle = load.import_handler do |name|
  throw std.ImportError(name)

handle.unregister()
handle.unregister()
```
