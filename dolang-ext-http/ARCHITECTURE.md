# dolang-ext-http Architecture

HTTP client for Do via `reqwest`. A `Global` singleton holds VM registrations
and a shared client. `Client` objects expose standard HTTP verbs; requests take
a URL and optional callback plus keyword arguments (`body`, `json`, `headers`,
`query`). `Response` exposes `status`/`headers` and body-consuming methods
(`text()`, `json()`). The extension registers an `http` module exporting a
`client()` factory and convenience functions for each verb. The `url` module
and `Url` object live in `dolang-ext-url`; this crate depends on that helper
surface for Do URL interop.
