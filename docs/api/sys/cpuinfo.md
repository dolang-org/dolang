# CpuInfo

CPU target information returned by [`cpu_info`](./index.md).

## Fields

### `arch`

Target architecture, from Rust's
[`std::env::consts::ARCH`](https://doc.rust-lang.org/std/env/consts/constant.ARCH.html).
Typical values include `:x86_64:` and `:aarch64:`.

### `logical_count`

Logical CPU count from
[`std::thread::available_parallelism`](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html).
