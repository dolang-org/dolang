# Do Language

Do is a scripting language for cross-platform CI/CD, DevOps, and automation. It
combines shell-like commands and indentation-oriented data declartion with
ordinary functions, structured concurrency, and remote-capable system APIs.

!!! warning "Experimental Language"
    Do is still in rapid development.
    The language syntax, standard library, and API are subject to change. Not
    recommended for production use.

## What Makes Do Different?

The interpreter stays local while a [VFS context](./shell/vfs/index.md) selects
where system work happens. The same function can operate on the local system, a
container, an SSH host, across WSL, or with administrator privileges.
Filesystem access, external programs, environment variables, system
information, and security queries follow the selected target.

```
import fs sys

def inspect_target()
  echo "$(sys.os_info().os): $(fs.Path(".").canonical())"
  run hostname

inspect_target()

import ssh
ssh.with build.example.com do inspect_target()
```

Contexts compose, so a local interpreter can enter an SSH host and then a
container on that host. APIs that are not VFS-forwaded continue to run in the
interpreter process.

## Language at a Glance

**Shell-like commands.** Bare words are strings; `$` introduces variables and
compact expressions.

```
run gcc -o main main.c -Wall -Werror -lm
```

**Full expressions.** Parentheses switch to expression syntax with operators,
calls, and insignificant whitespace.

```
let total = (price * tax_rate + shipping)
```

**Structured data.** Vertical layout builds nested values without a separate
data language.

```
let config =
  host: localhost
  port: 8080
  features:
    - logging
    - metrics
```

**Structured concurrency.** Strands provide cancellation, channels, pipelines,
streams, and scoped resource limits.

```
import strand

let results = strand.fork
  do build linux
  do build windows
  do build macos
```

## Declarative Meets Procedural

Structured data and executable blocks use the same syntax and runtime:

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

The list, progress call, and loop are ordinary Do values and code. The `run:`
value is a callable executed inside the build container's VFS context.

## Included Features

**Automation and system integration** — external programs
([`proc.run`](./api/proc-run.md)), filesystems ([`fs`](./api/fs/index.md)),
containers ([`docker`](./api/docker/index.md),
[`podman`](./api/podman/index.md), [`toolbx`](./api/toolbx.md)),
[SSH](./api/ssh.md), [WSL](./api/wsl.md), privilege elevation
([`admin`](./api/admin.md), [`sudo`](./api/sudo.md)), argument parsing
([`args`](./api/args.md)), system integration
([`sys`](./api/sys/index.md), [`systemd`](./api/systemd.md),
[`xdg`](./api/xdg.md)), identity and Windows access control
([`security`](./api/security/index.md)), and safe terminal output
([`term`](./api/term/index.md), [`progress`](./api/progress/index.md)).

**Data and protocols** — artifact [transfers](./api/transfer.md),
[HTTP](./api/http/index.md),
[URLs](./api/url/index.md), [JSON](./api/json.md), [TOML](./api/toml.md),
[XML](./api/xml/index.md), [YAML](./api/yaml.md),
[SQLite](./api/sqlite/index.md), [regex](./api/regex/index.md),
[base64](./api/base64.md), [digests](./api/digest/index.md),
[zip](./api/zip/index.md), [tar](./api/tar/index.md),
[time](./api/time/index.md), [glob](./api/glob/index.md),
[patch](./api/patch/index.md), and
[shlex](./api/shlex.md).

**Concurrency** — [`strand`](./api/strand/index.md) provides structured
concurrency, cancellation, channels, pipelines, streams, background work, and
scoped resources.

**Tooling** — [LSP server and VS Code extension](./lsp.md), compiler and loading
APIs, and a REPL.

## Platform Support

Supported platforms are currently Linux, macOS, and Windows. Platform-specific
features follow the VFS context, not the host platform, so a Linux host can
remotely modify security descriptors on Windows, etc.

## Get Started

**New to Do?** Start with the [Language Guide](./language/index.md).

**Building a script?** Follow the
[command-line tool worked example](./shell/cli-tools.md).

**Targeting another system?** Read the [VFS Guide](./shell/vfs/index.md).
