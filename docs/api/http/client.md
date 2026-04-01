# Client

HTTP client for making web requests.

## Constructor

### `Client ... func?`

!!! warning
    `unix_socket` is not container-transparent. It connects to a Unix socket on
    the host where the Do process is running, not through the shell agent's
    container filesystem/network translation layer.

**Parameters:**

| Name            | Type                                                        | Description                                                  |
| --------------- | ----------------------------------------------------------- | ------------------------------------------------------------ |
| `unix_socket`   | [`str`](../std/str.md)                                      | Path to a Unix domain socket to connect through (Unix only)  |
| `proxy`         | [`str`](../std/str.md)\|[`url.Url`](../url/index.md)\|`nil` | Proxy URL for all requests, or `nil` to disable system proxy |
| `cookies`       | `bool`                                                      | Enable a per-client cookie jar for session-style workflows   |
| `ca_cert`       | [`str`](../std/str.md)\|[`bin`](../std/bin.md)              | PEM-encoded CA certificate to add to the trust store         |
| `identity`      | [`str`](../std/str.md)\|[`bin`](../std/bin.md)              | PKCS#12/PFX client certificate for mutual TLS                |
| `password`      | [`str`](../std/str.md)                                      | Password for the PKCS#12 identity (defaults to empty)        |
| `invalid_certs` | `:DANGER_ACCEPT:`                                           | Pass `:DANGER_ACCEPT:` to disable TLS certificate validation |
| `func`          | func                                                        | Callable to run with the client; auto-closes when done       |

**Returns:** `Client` when no `func` is provided, otherwise the result of
calling `func`

```

let client = Client()
```

#### Proxy

By default, the client respects system proxy environment variables
(`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`). Pass `proxy:` to set an explicit
proxy or `proxy: nil` to disable proxy detection entirely.

```
# Explicit proxy
let client = Client proxy: "http://proxy.corp:8080"

# Disable system proxy
let client = Client proxy: nil
```

#### Cookies

Pass `cookies: true` to enable automatic storage and replay of cookies on that
client instance. Cookie state is isolated per client and is not shared with the
top-level `http.get` / `http.post` helpers.

```
let client = Client cookies: true

client.post https://example.com/login
client.get https://example.com/dashboard do |resp|
  echo $resp.status
```

#### TLS Certificates

Use `ca_cert:` to trust an additional CA certificate (PEM format), and
`identity:` with `password:` for client certificate authentication (PKCS#12/PFX
format).

```
# Custom CA certificate
let client = Client ca_cert: (fs.read /path/to/ca.pem)

# Client certificate authentication
let client = Client
  identity: (fs.read /path/to/client.pfx)
  password: secret
```

#### Disabling Certificate Validation

Pass `invalid_certs: :DANGER_ACCEPT:` to disable TLS certificate validation.
This is dangerous and should only be used for testing.

```
let client = Client invalid_certs: :DANGER_ACCEPT:
```

## Methods

### `get` | `post` | `...`

```text
get url :headers? :query? :status? block?
head ...
delete ...
options ...
trace ...
connect ...

post url :headers? :body? :json? :lines? :query? :status? block?
put ...
patch ...
```

Makes an HTTP request using the specified verb.

**Parameters:**

| Name      | Type                                                  | Description                                                   |
| --------- | ----------------------------------------------------- | ------------------------------------------------------------- |
| `url`     | [`str`](../std/str.md)                                | The URL to request                                            |
| `body`    | [`str`](../std/str.md)\|[`bin`](../std/bin.md)\|input | Request body; accepts iterables for streaming                 |
| `json`    | any                                                   | Request body as JSON value (auto-serialized)                  |
| `lines`   | input                                                 | Stream request body with newlines between elements            |
| `headers` | [`dict`](../std/dict.md)                              | Dictionary of headers; repeated keys are accepted             |
| `query`   | [`dict`](../std/dict.md)                              | Dictionary of query parameters; repeated keys are accepted    |
| `status`  | `:ignore:`\|`"ignore"`                                | Return the response even when the status is outside 200-299   |
| `block`   | func                                                  | Called with response; response is closed upon return or error |

**Returns:** [`Response`](./response.md) -- The HTTP response

**Errors:** Raises [`Status`](./status.md) on non-2xx responses by
default, and [`Error`](./error.md) on transport or protocol failure

```

let client = Client()

let response = client.get https://api.example.com/users
  query: {page: 1 limit: 10}
  headers: {authorization: "Bearer token123"}
echo $response.status

let response = client.post https://api.example.com/users
  json:
    name: Alice
    age: 30
```

To keep the old behavior and inspect non-2xx responses directly, pass
`status: :ignore:`:

```
let response = client.get https://api.example.com/missing
  status: :ignore:
assert_eq $response.status 404
```

## Request Options

### `body`

Specifies the raw request body. Cannot be used together with `json` or `lines`.

When a string or binary value is passed, it is sent as-is. When an iterable is
passed, the values are streamed directly to the request body without adding any
delimiters.

```
# Direct string body
let response = client.post https://api.example.com/data
  body: "raw text data"
  headers: {"content-type": "text/plain"}

# Streaming without delimiters (useful for binary data)
let response = client.post https://api.example.com/stream
  body: ["chunk1", "chunk2", "chunk3"]
  headers: {"content-type": "application/octet-stream"}
```

### `json`

Specifies the request body as a Do value that will be automatically serialized
to JSON. Cannot be used together with `body` or `lines`.

```
let response = client.post https://api.example.com/users
  json: {name: "Alice", email: "alice@example.com"}
```

### `lines`

Streams the request body from an iterable, adding a newline after each element.
Cannot be used together with `body` or `json`. Unlike `body` which accepts any
value, `lines` requires an iterable.

```
let response = client.post https://api.example.com/data
  lines:
    - first line
    - second line
    - third line

# Sends: "first line\nsecond line\nthird line\n"
```

### `headers`

Specifies request headers as a dictionary. Duplicate header names can be
specified by repeating keys.

Header values are normally stringified. If a value is a
[`DateTime`](../time/datetime.md), it is formatted as an HTTP-date
(`IMF-fixdate`), which is useful for headers such as `If-Modified-Since`.

```
let response = client.get https://api.example.com/users
  headers:
    authorization: Bearer token123
    "user-agent": MyApp/1.0
    accept: application/json
    accept: application/json+verbose
```

```
import time:
  - DateTime

let response = client.get https://api.example.com/archive
  headers:
    "if-modified-since": DateTime.from_unix(1700000000)
  status: :ignore:
```

### `query`

Specifies URL query parameters as a dictionary. Duplicate parameter names can be
specified by repeating keys.

```
let response = client.get https://api.example.com/users
  query:
    page: 1
    limit: 10
    search: alice
    sort: name
    sort: age
```
