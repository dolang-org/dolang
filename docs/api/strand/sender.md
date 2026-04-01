# Sender

A Sender is used to send values into a channel. Senders are output iterators,
used with `put` or the `.put()` method.

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

### `close()`

Closes the sender, signaling EOF to receivers. After closing:

- Sends will fail
- Receivers will eventually see no more values

```
send.close()
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
