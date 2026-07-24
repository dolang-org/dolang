# Response

HTTP response object returned by HTTP requests.

An HTTP response object might hold open resources such as a connection pending
receipt of the response body. Methods that fetch the response body
automatically close or release the connection. After the response is closed,
most subsequent methods will return errors.

## Fields

### `url`

The final response URL after redirects.

#### Type

[`url.Url`](../url/url.md)

### `status`

The HTTP status code of the response.

#### Type

[`int`](../std/int.md)

```

let response = get https://api.example.com/users
echo $response.status  # 200 for success
```

### `headers`

The response headers as a [`dict`](../std/dict.md). Duplicate keys can be
fetched via iteration, unpacking, or the `pop` or `get` methods.

Header values are usually returned as strings. If a header value parses as an
HTTP-date, it is returned as a [`DateTime`](../time/datetime.md) instead.

#### Type

[`dict`](../std/dict.md)

```

let response = get https://api.example.com/users
echo $response.headers["content-type"]
echo $response.headers["content-length"]
```

```
import time:
  - DateTime

get https://api.example.com/archive do |response|
  let modified = response.headers["last-modified"]
  assert (type modified DateTime)
```

## Methods

### `close`

Closes the response if it hasn't been already.

### `text`

Reads the response body as text. This method consumes the response and leaves it
in a "closed" state.

#### Returns

[`str`](../std/str.md) -- The response body as text

#### Errors

| Exception             | Condition                              |
| --------------------- | -------------------------------------- |
| `RuntimeError`        | The response has already been closed   |
| [`Error`](./error.md) | A transport or protocol failure occurs |

```

let response = get https://api.example.com/users
echo $response.text()
```

### `body`

Reads the response body as binary data. This method consumes the response and
leaves it in a "closed" state.

#### Returns

[`bin`](../std/bin.md) -- The response body as bytes

```

let response = get https://api.example.com/image.png
let data = response.body()
echo "Downloaded $data.len bytes"
```

### `json`

Reads the response body and parses it as JSON. This method consumes the response
and leaves it in a "closed" state.

#### Returns

The parsed JSON value as a tree of
[`int`](../std/int.md),
[`float`](../std/float.md), [`str`](../std/str.md),
[`array`](../std/array.md), and [`dict`](../std/dict.md), as
appropriate.

#### Errors

| Exception             | Condition                                  |
| --------------------- | ------------------------------------------ |
| `RuntimeError`        | The response has already been closed       |
| [`Error`](./error.md) | An error occurs while reading the response |
| `ValueError`          | The JSON is invalid                        |

```

let response = get https://api.example.com/users
let data = response.json()
echo $data["users"][0]["name"]
```

### `chunks`

Returns an iterator that yields the response body as raw bytes chunks. This
method is useful for processing large responses without loading the entire
body into memory.

#### Returns

An iterator of [`bin`](../std/bin.md) values

```

get https://api.example.com/large-file do |response|
  let total_size = 0
  for chunk = response.chunks()
    total_size = total_size + chunk.len
  echo "Downloaded $total_size bytes"
```

### `lines`

Returns an iterator that yields the response body as lines (split on `\n` or
`\r\n`). Line endings are stripped from the returned values.

#### Returns

An iterator of [`str`](../std/str.md) values

```

get https://api.example.com/logs do |response|
  for line = response.lines()
    if (line.contains "ERROR")
      echo $line
```

### `events`

Returns an iterator that parses the response body as a Server-Sent Events
stream. This is useful for streaming LLM responses, log tails, and other
event feeds delivered as `text/event-stream`.

This method consumes the response body incrementally. Once iteration begins,
the response should be treated as body-owned by the iterator, just like
[`chunks`](#chunks) and [`lines`](#lines).

Each yielded item is an [`Event`](./event.md) with `type`, `data`, `id`,
and `retry` fields.

#### Returns

An iterator of [`Event`](./event.md) values

#### Errors

| Exception             | Condition                               |
| --------------------- | --------------------------------------- |
| `RuntimeError`        | The response has already been closed    |
| [`Error`](./error.md) | The underlying body read fails          |
| `ValueError`          | The event stream contains invalid UTF-8 |

```

get https://api.example.com/stream do |response|
  for event = response.events()
    echo "event=$event.type id=$event.id"
    echo $event.data
```
