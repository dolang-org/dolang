# Provisioning and Lifecycle

Everything on this page applies to a guest however it was provisioned — from a
cloud image, from install media, or from a bundle.

## One-Shot Guest

```
libvirt.with
  image: freebsd.qcow2.xz
  os: :FREEBSD:
  do
    echo $sys.os_info().os
    run uname -a
```

`with` waits for SSH, installs Do into the guest (see [Installing
Do](#installing-do)), waits for boot-time commands and the remote
`dolang-vfs`, enters the VFS for the block, then stops and undefines the guest
even if the block throws.

## Persistent Guest

```
let guest = libvirt.create
  image: freebsd.qcow2.xz
  os: :FREEBSD:
  name: freebsd-build

echo $guest.name
```

A later process can recover the `Guest` object by name.

```
let guest = libvirt.attach freebsd-build
guest.with do
  run uname -a
```

Call `.destroy()` to force-stop a running guest and `.undefine()` to unregister
it. For guests created by this module, `undefine` also removes the ephemeral
overlay after validating the ownership metadata embedded in the guest XML.

## Actions

`libvirt.create` permits specifying actions to perform after the guest
can respond to SSH but before the guest is considered complete. Actions
can be repeated and are run in the order specified, interleaved with each
other as written. These actions are also available in `libvirt.build` and
`libvirt.with` (where they run prior to the main block).

### Adding Files

`add:` writes one file into the guest, named by `target:`, whose content comes
from exactly one of `content:` or `source:` — the latter an [artifact
spec](#artifact-specs), so it may be a path, a URL, or a `Dict` pinning a
digest.

| Key        | Type                               | Description                            |
| ---------- | ---------------------------------- | -------------------------------------- |
| `target`   | [`Str`](std.Str)                   | Guest path to write                    |
| `source`?  | Artifact                           | Host file to copy; excludes `content:` |
| `content`? | [`Str`](std.Str)\|[`Bin`](std.Bin) | Literal content; excludes `source:`    |
| `chmod`?   | [`Int`](std.Int)                   | File mode, e.g. `0o644`                |

```
libvirt.create
  image: freebsd.qcow2.xz
  os: :FREEBSD:
  add:
    target: /home/ci/build.dol
    source: ./build.dol
    chmod: 0o755
```

### Running Commands

`run:` runs a function (typically a `do` block) inside the guest's VFS.

```
libvirt.create
  image: freebsd.qcow2.xz
  os: :FREEBSD:
  run: do
    fs.create_directory all: true /opt/build
    cd /opt/build do
      run git clone https://github.com/dolang-org/dolang
```

### Rebooting

`reboot` restarts the guest and waits for it to come back. It takes no
argument, so it is written as a bare positional item:

```
libvirt.create
  image: freebsd.qcow2.xz
  os: :FREEBSD:
  run: do
    run pkg install -y llvm19
  reboot
  run: do
    run uname -a
```

Otherwise causing the guest to reboot or power off during an action in
`libvirt.create`, `libvirt.with`, or `libvirt.build` is likely to cause an
error.

## Artifact Specs

Everything this module fetches — `image:`, `bundle:`, `installer:`, `drivers:`,
`dolang:`, and an `add:` `source:` — is spelled the same two ways. Either a
bare path or URL:

```
libvirt.create
  image: https://example.com/disk.qcow2
  os: :LINUX:
```

or a `Dict` naming the source under `source:`, with metadata alongside it:

```
libvirt.create
  image:
    source: https://example.com/disk.qcow2
    digest: blake3:0123...
  os: :LINUX:
```

| Key       | Type                                                  | Description                              |
| --------- | ----------------------------------------------------- | ---------------------------------------- |
| `source`  | [`Str`](std.Str)\|[`Path`](fs.Path)\|[`Url`](url.Url) | The source to fetch                      |
| `digest`? | [`Str`](std.Str)                                      | `algorithm:hex` digest to verify against |

`installer:` takes two more metadata keys, `edition:` and `index:` — see
[Windows Guests](./windows.md).

A local path is verified in place. A URL is downloaded, verified, and cached,
and a digest-pinned entry that has already been verified is reused without
contacting the server.

Every artifact a `create` names is resolved before the domain is defined,
including those used by actions.

## Installing Do

Do is installed into every guest before it is considered ready, as `dolang-vfs`
is the mechanism by which guests are controlled after early provisioning.
`dolang:` selects which build to install. If omitted, it installs the release
matching the running interpreter's version
([`shell.VERSION`](shell.VERSION)) for `os:`/`arch:`.

| Value                          | Behavior                                                                 |
| ------------------------------ | ------------------------------------------------------------------------ |
| a version tag, e.g. `"v0.1.1"` | Fetch that release's artifact for `os:`/`arch:`.                         |
| a [`Path`](fs.Path)            | Use a local archive directly.                                            |
| a [`Url`](url.Url)             | Fetch an achive, bypassing release resolution.                           |
| an artifact spec               | An archive with a pinned digest — see [Artifact Specs](#artifact-specs). |
| `{version: tag}`               | Explicit form of the version tag.                                        |

## File Transfer

Use `upload` and `download` to copy individual files into and out of a running
guest:

```
guest.upload ./input.tar /tmp/input.tar
guest.download /tmp/result.tar ./result.tar
```

## Running a Script in a Guest

The bundled `libvirt` entrypoint compiles a local script and runs it through an
existing Do-created guest's VFS:

```
dolang -m libvirt freebsd-build build.dol release
```

Every argument after the script path is passed through unchanged. Inside the
target script, `shell.program` is the local script path and `shell.args`
contains only those trailing arguments.

`--cd` sets the initial remote working directory; repeat `--env` to set or
inherit (a bare `NAME`, with no `=`, inherits the local value) an
environment variable; `--unset-env` unsets one:

```
dolang -m libvirt --cd build --env CARGO_TERM_COLOR freebsd-build build.dol
```

`-m` runs a bundled entrypoint in the guest instead of a local script, using
the same `-m NAME` spelling as the top-level command line:

```
dolang -m libvirt freebsd-build -m test tests/
```

The entrypoint sees its own name as `shell.program`, so it behaves as it would
when run directly.

## Troubleshooting

Setting the environment variable `DOLANG_LIBVIRT_KEEP_FAILED` leaves the
**libvirt** domain and its work directory in place for inspection
after a failed `dolang.create` or related operation.
