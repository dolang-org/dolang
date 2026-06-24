# Do Language

A scripting language for CI/CD, DevOps, and automation that melds declarative
elegance with Unix elbow grease:

- Flexibly define structured data and operate on it with lightweight,
  indentation-oriented syntax.
- Run external CLI programs directly.
- Use built-in modules for common tasks such as HTTP, SQLite, and more.
- Write ordinary code too: data structures, closures, iterators, classes,
  exceptions, concurrency.

> **⚠️ Experimental:** Do is still in rapid development. The language syntax,
> standard library, and API are subject to change. Not recommended for
> production use.

## Quick Look

**Shell-like simplicity:**

```
run gcc -o main main.c -Wall -Werror
```

**Call programs like functions:**

```
import proc.run:
  - uname
  - git

let kernel = sub do uname -r
let branch = sub do git rev-parse --abbrev-ref HEAD
echo "Building on $kernel, branch $branch"
```

**Declarative and procedural in one syntax:**

```
import progress container.podman: podman

let PACKAGES =
  - gcc
  - node

progress.with do podman.build
  from: fedora:42
  add:
    target: /etc/sudoers.d/wheel
    chmod: 0o640
    content: |
      %wheel ALL=(ALL) NOPASSWD: ALL

  run: do progress.show
    total: $PACKAGES.len
    message: installing packages
    icon: 📦
    do |i|
      for pkg = PACKAGES
        i.message = "installing $pkg"
        run dnf install -y $pkg
        i.delta()

  tag: my-image
```

## Getting Started

### Prerequisites

- Rust 1.92.0 or later

### Building from Source

```bash
# Build the project
cargo build --release --bin dolang --bin dolang-shell-vfs

# Run the shell
./target/release/dolang

# Or run a script
env DOLANG_SHELL_VFS=./target/release/dolang-shell-vfs ./target/release/dolang example/cow.dol
```

## Acknowledgements

Do builds on a lot of excellent Rust ecosystem work.

- Vendored code: `hashbrown`; `tokio-unix-ipc` by Armin Ronacher (`mitsuhiko`)
- Implementation inspiration: `vint64` by Tony Arcieri; `tiny-sort-rs` by Lukas
  Bergdoll
- Major building blocks: `tokio`, `reqwest`, `sqlite-plugin`, `libsqlite3-sys`,
  `linkme`, `annotate-snippets`, `tower-lsp`

Thanks to the authors and maintainers of these projects.

## License

Do is available under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license ([LICENSE-MIT](./LICENSE-MIT))

at your option.
