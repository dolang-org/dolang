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
is created, then holds the body as `reqwest::Response::bytes_stream()`. Every
body consumer (`body()`, `text()`, `json()`, `chunks()`, `lines()`, `events()`,
and the body excerpt kept on `Status`) reads from that stream, so native and
browser builds share one code path.

## Request Bodies

Iterable `body:` and `lines:` values and iterable multipart parts go through
`body::IterBodies`. On native targets each iterator is rooted in an array and
pumped into a channel by a scoped strand while the request is sent.

On `wasm32-unknown-unknown`, reqwest uses the browser's fetch API, which cannot
stream request bodies, so each iterator is collected before sending. The
browser build also rejects the `Client` options that fetch has no equivalent
for: Unix sockets, proxies, cookie jars, and TLS configuration.
