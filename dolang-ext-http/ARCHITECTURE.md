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
- `events()` reads the body in place, and the body excerpt kept on
    `StatusError` is read from it directly.

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

## Errors

`error.rs` raises a failed request as a subclass of `http.Error`, chosen by
walking the `reqwest::Error`'s source chain, since reqwest's own kind doesn't
decide it (a truncated body is `Kind::Decode`). In order:

1. A builder error comes from the caller's arguments and raises `ValueError`.
2. A redirect error is `RequestError :REDIRECT:`.
3. A `native_tls::Error` anywhere in the chain is `TransportError :TLS:`. It
   comes before `is_connect`, which a handshake failure also satisfies.
4. A `hyper::Error` decides by its predicates: a parse error is a
   `DataError`, and an incomplete message is `TransportError :CLOSED:`.
5. `is_connect` is `TransportError :CONNECT:`.
6. Otherwise the innermost `io::Error` decides. hyper's body decoder reports
   bad framing as `InvalidData` or `InvalidInput`, which is
   `DataError :BODY:`, and a short body as `UnexpectedEof`, which is
   `:CLOSED:`. Any other I/O error is `:IO:`, and no I/O error at all is
   `RequestError :OTHER:`.

The browser build has neither hyper nor native-tls, and fetch reports no
detail, so it uses only reqwest's predicates.

The annex keeps the innermost `io::Error` as its kind, OS code and message.
The `cause` getter rebuilds it and maps it through `io::error`, so the shell's
handler gives it its `sys` class. The message is the source chain, joined
with `": "`, without the links that repeat text already in it.

The `io()` stream can't carry an `http.Error`, so `BodyRead` reports the
innermost `io::Error` instead, keeping its OS code or kind.
