# Error

`Error` is raised for transport and protocol failures originating from the
HTTP extension. It is a subtype of
[`std.RuntimeError`](../std/runtime-error.md), so it can be caught
either specifically or through the broader runtime error type.

```
try
  get "http://127.0.0.1:1"
catch Error: err
  echo $str(err)
```

`str(err)` returns the underlying `reqwest` error message. `dbg(err)` includes
the nominal type name together with that message.

Non-2xx HTTP responses are reported through
[`Status`](./status.md), which is a nominal subtype of `Error`.

When the underlying error carries a URL, `Error` exposes it through a
`url` field:

```
try
  get "http://example.invalid"
catch Error: err
  if (err.url != nil)
    echo $err.url.host
```
