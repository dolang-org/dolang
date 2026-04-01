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
cargo build --release

# Run the shell
./target/release/dolang-shell

# Or run a script
./target/release/dolang-shell example/cow.dol
```

## Project Structure

This is a Cargo workspace. Core crates:

- **`dolang`** - Public API facade for embedding Do in Rust applications
- **`dolang-bytecode`** - Bytecode format, instruction set, and verification
- **`dolang-compile`** - Lexer, parser, name resolution, and bytecode emitter
- **`dolang-runtime`** - VM, garbage collector, strand concurrency, standard
  library
- **`dolang-private-util`** - Shared utilities (string interning, arena
  allocator, etc.)

Tooling:

- **`dolang-shell`** - Command-line shell and script executor with REPL
- **`dolang-lsp`** - Language Server Protocol server for editor integration
- **`dolang-highlight`** - Syntax highlighting tool

Extensions:

- **`dolang-ext-shell`** - Process execution, filesystem, environment,
  containers
- **`dolang-ext-http`** - HTTP client (reqwest)
- **`dolang-ext-url`** - URL parsing and construction
- **`dolang-ext-json`** - JSON serialization/deserialization (serde\_json)
- **`dolang-ext-xml`** - XML parsing (quick-xml)
- **`dolang-ext-yaml`** - YAML parsing
- **`dolang-ext-sqlite`** - SQLite database access (libsqlite3-sys)
- **`dolang-ext-regex`** - Regular expressions
- **`dolang-ext-base64`** - Base64 encoding and decoding
- **`dolang-ext-digest`** - Cryptographic hashing (blake3, sha2, md5)
- **`dolang-ext-progress`** - Progress indicators (indicatif)
- **`dolang-ext-zip`** - ZIP archive reading
- **`dolang-ext-compile`** - Runtime compilation support
- **`dolang-ext-load`** - Dynamic module loading

Standard libraries (implemented in Do):

- **`args`** - Command-line argument parsing with help generation
- **`test`** - Test framework with assertions
- **`xdg`** - XDG Base Directory support
- **`systemd`** - systemd integration (os-release)
- **`sudo`** - Privileged execution helpers
- **`container.docker`** - Docker container building and execution
- **`container.podman`** - Podman container building and execution
- **`container.toolbx`** - Toolbox container execution

## License

Do is available under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT license ([LICENSE-MIT](./LICENSE-MIT))

at your option.
