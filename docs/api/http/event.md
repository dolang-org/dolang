# Event

Server-Sent Event item yielded by [`Response.events()`](./response.md#events).

## Fields

### `type`

The event type. When the stream omits `event:`, or provides an empty `event:`
field, this defaults to `"message"`.

#### Type

[`str`](../std/str.md)

### `data`

The event payload text. Multiple `data:` lines are joined with `\n`.

#### Type

[`str`](../std/str.md)

### `id`

The event identifier, if present.

#### Type

[`str`](../std/str.md) or `nil`

### `retry`

The reconnection delay hint from the stream, if present.

#### Type

[`int`](../std/int.md) or `nil`

```

get https://api.example.com/stream do |response|
  for event = response.events()
    echo "[$event.type] $event.data"
```
