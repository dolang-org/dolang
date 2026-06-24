# dolang-shell-modules Architecture

Owns the stock Do modules bundled into the shell binary.

- **Source ownership**: `lib/` contains the standard Do modules previously
  embedded by `dolang-shell`.
- **Build-time compilation**: `build.rs` compiles each `.dol` source into
  bytecode and generates a static lookup table.
- **Shell-core integration**: the crate exposes bundled module lookup data; the
  stock `Config` lives in `dolang-shell`.
