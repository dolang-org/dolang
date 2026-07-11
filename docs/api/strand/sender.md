# Sender

A Sender is used to send values into a channel. Senders are sinks,
used with `put` or the `.put()` method.

## Inherits

- [`Sink`](../std/sink.md)

## Creating a Sender

Senders are created by the [`channel`](./index.md) function:

```
let send recv = channel()
let send recv = channel 10  # buffered channel
```

## Using a Sender

Send values using `put` or `.put()`:

```
send.put 42
send.put "hello"
```

## Methods

### `close error? :backtrace?`

Closes the sender, signaling EOF to receivers. After closing:

- Sends will fail
- Receivers will eventually see no more values
- If `error` is provided, the receiver re-raises it from `.next()` once any
  buffered values have been drained

#### Parameters

| Name        | Type                               | Description                         |
| ----------- | ---------------------------------- | ----------------------------------- |
| `error`     |                                    | Optional error value to propagate   |
| `backtrace` | [`strand.Backtrace`](./index.md)?  | Optional backtrace for that error   |

```
send.close()
send.close "boom"
```

## Example

```

let send recv = channel()

fork
  do
    send.put 1
    send.put 2
    send.put 3
    send.close()
  do for value = recv
    echo $value
```
