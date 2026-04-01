# Indicator

A progress indicator created by
[`progress.show`](./index.md#show-func).

An indicator is either a progress bar (when `total` is set) or a spinner (when
`total` is not set). Setting `total` dynamically switches between the two
modes.

## Properties

### `message`

The indicator message text (`str`). Read/write.

### `icon`

The prefix icon (`str`). Read/write.

### `total`

The total value for bar mode (`int`), or `nil` for spinner mode. Read/write.

Setting `total` to an `int` switches to bar mode. Setting it to `nil` switches
to spinner mode.

### `position`

The current position (`int`). Read/write.

## Methods

### `delta n?`

Adjusts the position by `n` (default +1). Positive values increment, negative
values decrement.

| Name | Type                   | Description                  |
| ---- | ---------------------- | ---------------------------- |
| `n`  | [`int`](../std/int.md) | Amount to adjust (default 1) |
