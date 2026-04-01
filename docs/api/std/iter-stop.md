# IterStop

Error raised to signal that an input iterator is exhausted. This is used
internally by the iteration protocol and can be caught in `try`/`catch`
statements.

`IterStop` is the only error type that can be constructed directly:

```
let err = IterStop()
throw err
```

## Inherits

[`Error`](./error.md)
