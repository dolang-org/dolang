# HTTP Extension

HTTP client for making web requests.

## Types

| Type                    | Description                         |
| ----------------------- | ----------------------------------- |
| [`Client`](./client.md) | HTTP client for making requests     |
| [`Error`](./error.md)   | Transport or protocol request error |
| [`Event`](./event.md)   | Server-Sent Events stream item      |
| [`Status`](./status.md) | Non-2xx HTTP response error         |

HTTP request functions and client methods accept either a plain
[`str`](../std/str.md) URL or a [`url.Url`](../url/index.md)
instance.

Cookie-backed session handling is available through
[`Client`](./client.md), which can be constructed with `cookies: true`.
The top-level request helpers create a fresh stateless client per call.

## Functions

### `get` | `post` | `...`

Makes an HTTP request using the specified verb. See
[Client](./client.md#methods) for detailed documentation on all HTTP request
methods and their parameters.
