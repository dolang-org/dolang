# OsInfo

Operating system target information returned by [`os_info`](./index.md).

## Fields

### `os`

Specific operating system, from Rust's
[`std::env::consts::OS`](https://doc.rust-lang.org/std/env/consts/constant.OS.html).
Typical values include `:LINUX:`, `:MACOS:`, and `:WINDOWS:`.

### `family`

Operating system family, from Rust's
[`std::env::consts::FAMILY`](https://doc.rust-lang.org/std/env/consts/constant.FAMILY.html).
Typical values include `:UNIX:` and `:WINDOWS:`.

### `is_wine`

Whether the process is running under Wine.

This field is only available on Windows.
