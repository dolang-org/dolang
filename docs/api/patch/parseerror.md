# ParseError

[`ValueError`](../std/value-error.md) for malformed patch data.

`ParseError` is raised by [`patch.decode`](./index.md#decode-input) when the
input cannot be parsed as a patch stream.

## Example

```
try
  let _ = [...patch.decode "not a patch"]
catch patch.ParseError: err
  echo "bad patch: $err"
```
