# Do Language

Do is a scripting language for cross-platform CI/CD, DevOps, and automation. It
combines shell-like commands and indentation-oriented data declartion with
ordinary functions, structured concurrency, and remote-capable system APIs.

[Documentation](https://dolang-org.github.io/dolang/) ·
[Source](https://github.com/dolang-org/dolang)

- Run the same automation on Linux, macOS, and Windows.
- Use structured concurrency, cancellation, channels, pipelines, and scoped
  resources.
- Redirect filesystem, process, environment, system, and security operations
  with block-scoped VFS contexts.
- Enter containers, SSH hosts, WSL, `sudo`, or Windows UAC elevation without
  rewriting the function that performs the work.
- Work with Windows paths, access tokens, SIDs, ACLs, security descriptors,
  and native error codes alongside Unix identities and error codes.
- Styled terminal output and progress displays.
- Editor support: LSP server, VS Code extension, Vim syntax definition

> **⚠️ Experimental:** Do is still in rapid development. The language syntax,
> standard library, and API are subject to change. Not recommended for
> production use.

## What Makes Do Different?

The interpreter stays local while a VFS context selects where system work
happens. The same function can operate on the local system, a container, an SSH
host, across WSL, or with administrator privileges. Filesystem access, external
programs, environment variables, system information, and security queries
follow the selected target.

```
import fs sys

def inspect_target()
  echo "$(sys.os_info().os): $(fs.Path(".").canonical())"
  run hostname

inspect_target()

import ssh
ssh.with build.example.com do inspect_target()
```

VFS contexts compose, so the same model supports paths such as local → SSH host
→ container. APIs that are not VFS-forwarded continue to run in the interpreter
process.

## Quick Look

**Shell-like commands:**

```
run gcc -o main main.c -Wall -Werror
```

**External programs as functions:**

```
import proc.run:
  - uname
  - git

let kernel = sub do uname -r
let branch = sub do git rev-parse --abbrev-ref HEAD
echo "Building on $kernel, branch $branch"
```

**Structured data and code together:**

```
import progress podman

let PACKAGES =
  - gcc
  - node

progress.with do podman.build
  from: fedora:42
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

## Platform Support

The core test suite runs on Linux, macOS, and Windows. Some integrations also
require platform tools or services.

| Capability           | Linux              | macOS              | Windows                      |
| -------------------- | ------------------ | ------------------ | ---------------------------- |
| Files and processes  | Tested             | Tested             | Tested                       |
| Native identity      | UID/GID and groups | UID/GID and groups | tokens, SIDs, ACLs, SecDescs |
| Privilege elevation  | `sudo`             | `sudo`             | UAC                          |
| Remoting             | SSH                | SSH                | SSH                          |
| Local containers/VMs | Docker/Podman      | Planned            | WSL                          |

## Included Modules

- **Automation and system integration:** processes, filesystems, containers,
  SSH, WSL, privilege elevation, argument parsing, systemd, XDG, progress, and
  terminal output.
- **Data and protocols:** HTTP, URLs, JSON, TOML, YAML, XML, SQLite, regex,
  base64, digests, zip, globbing, patches, and shell quoting.
- **Concurrency:** strands, cancellation, channels, pipelines, streams, and
  scoped resources.
- **Tooling:** compiler APIs, dynamic loading, the REPL, LSP, and VS Code
  extension.

Do also implements this repository's cross-platform GitHub Actions build and
release workflows, including the Do-based task runner used by those workflows.

## Getting Started

### Prerequisites

- Rust 1.93 or later

### Building from Source

```bash
# Build the project
cargo build --release --bin dolang --bin dolang-lsp --bin dolang-vfs

# Run the shell
./target/release/dolang

# Or run a script
./target/release/dolang example/cow.dol
```

See the [Language Guide](https://dolang-org.github.io/dolang/language/overview/)
or follow the
[command-line tool example](https://dolang-org.github.io/dolang/shell/cli-tools/).

## Acknowledgements

Do builds on a lot of excellent Rust ecosystem work.

- Vendored code: `hashbrown`
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
