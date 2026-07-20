# Do Language

A scripting language for CI/CD, DevOps, and automation that melds declarative
elegance with Unix elbow grease:

- Flexibly define structured data and operate on it with lightweight,
  indentation-oriented syntax.
- Run external CLI programs directly.
- Use built-in modules for common tasks such as HTTP, SQLite, and more.
- Write ordinary code too: data structures, closures, iterators, classes,
  exceptions, concurrency.

!!! warning "Experimental Language"
    Do is still in rapid development.
    The language syntax, standard library, and API are subject to change. Not
    recommended for production use.

## Why Do?

**Simple syntax for shell-script tasks.** Bare words are strings, `$` introduces
variables and expressions.

```
run gcc -o main main.c -Wall -Werror -lm
```

**Full expressions when you need them.** Parentheses switch to expression
syntax with operators, calls, and whitespace insignificance:

```
let total = (price * tax_rate + shipping)
```

**Structured data with YAML-like vertical layout.** Build nested data naturally
— no separate data format needed:

```
let config =
  host: localhost
  port: 8080
  features:
    - logging
    - metrics
  limits:
    max_connections: 100
    timeout: 30
```

## Declarative Meets Procedural

Mixed structured data and code in the same syntax and runtime:

```
# Build a container with podman with progress tracking
import progress podman

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

The `PACKAGES` list, `progress.show` call, and `for` loop are ordinary Do
code — the `run: do` block is a closure, not a quoted string passed to a shell.

## Unix Tool Integration

External programs become callable functions:

```
import proc.run:
  - uname
  - git

# Capture output
let kernel = sub do uname -r
let branch = sub do git rev-parse --abbrev-ref HEAD

echo "Building on $kernel, branch $branch"
```

## VFS

Do can run ordinary blocks in the context of containers, on SSH hosts, or with
administrator privileges. Filesystem access, launching external programs,
environment variables, working directory, and more follow the selected target:

```
import shlex
import podman
import fs:
  - open

# Read a key from a shell-quoted config file inside a container
def read_key ctr path key
  podman.with $ctr do open $path do |file|
    for line = file
      let k v = line.split = limit: 1
      if (k == key)
        return shlex.split(v).next()

echo $ read_key ubuntu:24.04 /etc/os-release PRETTY_NAME
```

## Included Features

**Automation** — run external programs as functions
([`proc.run`](./api/proc-run.md)), manage files ([`fs`](./api/fs/index.md)),
build and run containers ([podman](./api/podman/index.md),
[docker](./api/docker/index.md),
[toolbx](./api/toolbx.md)), parse CLI arguments
([`args`](./api/args.md)), elevate privileges ([`admin`](./api/admin.md),
[`sudo`](./api/sudo.md)), read
system configuration ([`systemd`](./api/systemd.md), [`xdg`](./api/xdg.md)),
cross between Windows and WSL ([`wsl`](./api/wsl.md)),
and show friendly [progress indicators](./api/progress/index.md).

**Data and protocols** — [HTTP](./api/http/index.md), [JSON](./api/json.md),
[TOML](./api/toml.md), [XML](./api/xml/index.md), [YAML](./api/yaml.md),
[SQLite](./api/sqlite/index.md), [regex](./api/regex/index.md),
[base64](./api/base64.md), [hashing](./api/digest/index.md),
[zip](./api/zip/index.md)

**Tooling** — [LSP server](./lsp.md), REPL

## Get Started

**New to Do?** Start with the [Language Guide](./language/overview.md).

**Building a script?** Follow the
[command-line tool worked example](./shell/cli-tools.md).
