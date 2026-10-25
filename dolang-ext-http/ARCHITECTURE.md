# dolang-ext-http Architecture

HTTP client for Do via `reqwest`. A `Global` singleton holds VM registrations.
`Client` objects expose standard HTTP verbs; requests take a URL and optional
callback plus keyword arguments (`body`, `lines`, `json`, `multipart`,
`headers`, `query`, `status`). The extension registers an `http` module
exporting `Client`, the response and error types, and a function for each verb
that uses a fresh client. The `url` module and `Url` object live in
`dolang-ext-url`; this crate depends on that helper surface for Do URL interop.

## Response Bodies

`Response` captures the status, URL, headers, and error-status message when it
is created, then holds the body as `reqwest::Response::bytes_stream()`, the one
body API that native and browser builds share. The metadata stays until the
response is closed. Each consumer except `events()` takes the body:

- `body()`, `text()` and `json()` read it whole.
- `io()` returns it as a Do `io.Io` (`body_io.rs`) and keeps that stream in the
  response's slot, so closing the response closes it too. `chunks()` and
  `lines()` are `chunks` and `io.Buffered` lines over it.
- `events()` reads the body in place, and the body excerpt kept on `Status` is
  read from it directly.

On native targets the `Io` is a `ByteHandle` over the stream, so native
consumers such as `compress.decompress` read it without entering the VM. The
browser's stream holds JavaScript values, which aren't `Send`, so a
`ByteHandle` can't hold it. On wasm the body is instead a native `Io` subtype,
`ResponseBody`, that defines `read` and `close` and provides no `ByteIo`, so
`Io`'s bridge serves its byte handle on the VM thread.

## Request Bodies

A `Str` or `Bin` body is sent whole. Any other `body:` or multipart part body
goes through `body::reader`: a `ByteIo` provider is read through its handle,
and anything else through `io::concat` over its iterator. `lines:` always uses
`io::concat`, with a newline after each item. On native targets each handle is
streamed to reqwest while the request is sent; the first read error is kept
aside and replaces the error reqwest reports, so the request fails with the
stream's own error.

On `wasm32-unknown-unknown`, reqwest uses the browser's fetch API, which cannot
stream request bodies, so each handle is read to the end before sending. The
browser build also rejects the `Client` options that fetch has no equivalent
for: Unix sockets, proxies, cookie jars, and TLS configuration.
