# Receiver

A Receiver is used to receive values from a channel. Receivers are input
iterators, used with `for` loops or `.next()`.

## Inherits

- [`Iter`](../std/iter.md)

## Creating a Receiver

Receivers are created by the [`channel`](./index.md) function:

```
let send recv = channel()
let send recv = channel 10  # buffered channel
```

## Using a Receiver

### Iteration with `for`

```
for value = recv
  do_something $value
```

### Single value with `.next()`

```
let value = recv.next()
```

When the channel is closed and empty, `.next()` raises `IterStop`.
If the sender was closed with an error, `.next()` re-raises that error once any
buffered values have been drained.

## Methods

### `close()`

Closes the receiver. This prevents any more values from being sent and
wakes up any waiting senders.

```
recv.close()
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
