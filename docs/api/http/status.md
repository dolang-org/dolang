# Status

`Status` is raised when an HTTP request completes but returns a status
outside `200..=299`. It is a subtype of [`Error`](./error.md), and also of
[`std.RuntimeError`](../std/runtime-error.md).

```
try
  get "https://example.com/missing"
catch Status: err
  echo $err.status
```

The error stores response metadata and the first 64 KiB of the response body.

## Fields

### `status`

The HTTP status code.

### `headers`

The saved response headers as a [`dict`](../std/dict.md).

### `url`

The response URL as [`url.Url`](../url/index.md), or `nil` if none
was attached.

### `truncated`

`true` if the saved body excerpt was cut off either by the 64 KiB limit or by a
read error while buffering it.

## Methods

### `body`

Returns the saved body excerpt as [`bin`](../std/bin.md).

### `text`

Returns the saved body excerpt as [`str`](../std/str.md), failing on
invalid UTF-8.

### `json`

When the `json` feature is enabled, parses the saved body excerpt as JSON.
