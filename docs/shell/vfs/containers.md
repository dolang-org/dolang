# Containers

The `docker` and `podman` modules use [VFS contexts](./index.md) to run Do
functions in the context of containers. Filesystem operations, external program
launching, and other supported APIs target the container, while the interpreter
remains on the host.

## Running in a Container

Use `docker.with` or `podman.with` to run a block targeting a fresh container:

```
import podman systemd

let release = podman.with ubuntu:24.04 do
  systemd.os_release()

echo $release["PRETTY_NAME"]
```

`run`, `with`, and `build` accept `cd:` and `env:`. Environment keys may be
strings or symbols; `nil` unsets a variable and `:INHERIT:` copies its current
strand value into the container.

Use [`with_host`](./index.md#returning-to-the-host) to temporarily return to the
host VFS context:

```
import podman
import fs:
  - Path

podman.with ubuntu:24.04 do
  let release = Path("/etc/os-release").read()
  with_host do Path("release.txt").write $release
```

## Building Images

Container builds use the VFS to keep build steps as executable Do blocks. A
`run` step can call programs, inspect structured data, loop, and use any
VFS-aware API:

```
import podman

let image = podman.build
  from: ubuntu:24.04
  mounts:
    - type: cache
      target: /var/cache/apt
  run: do
    run apt-get update
    run apt-get install -y curl
  add:
    target: /etc/example.conf
    content: |
      enabled=false
    chmod: 0o644
  patch:
    content: |
      --- /etc/example.conf
      +++ /etc/example.conf
      @@ -1 +1 @@
      -enabled=false
      +enabled=true
  tag: example:latest
```

Build steps are applied in order:

- `run:` enters the build container's VFS and runs a block.
- `add:` copies a host path, URL, or inline content into the image.
- `patch:` applies a patch supplied by host path or inline content.
- `tag:` names the final image and may be repeated.

Top-level `mounts:` are available throughout the build. Cache mounts retain
downloaded data between builds; bind mounts expose an explicit host path.

## Images and Containers

The Docker and Podman modules also provide a small management API:

- `images` and `containers` list and filter objects.
- `image` and `container` inspect an image reference or container name/ID.
- `Image`s expose metadata and can be tagged, removed, saved, loaded, pulled,
  and pushed.
- `Container`s expose metadata and can be started, stopped, killed, restarted,
  or removed.

See the [`docker`](../../api/docker/index.md) and
[`podman`](../../api/podman/index.md) references for the complete interfaces.

`Container` objects inspect and change the lifecycle of existing containers;
`docker.run` and `podman.run` run a single direct command in a temporary
container, while `docker.with` and `podman.with` run a Do block with a
temporary container as a VFS target.

Image builds use a longer-lived temporary container VFS while applying their
ordered `run`, `add`, `patch`, and `commit` steps.

## Manual Container VFS

The module helpers mount `dolang-vfs` into a temporary container and connect
through a jointly accessible Unix socket. For a long-lived or externally
managed container, the same setup can be performed manually:

1. Make the `dolang-vfs` binary available inside the container.
2. Bind mount a shared private directory in the container.
3. Start `dolang-vfs` in the container with a socket path in that directory.
4. Instantiate `Vfs` with the socket path on the host

```
import shell:
  - Vfs

let agent = Vfs.unix_socket /run/container-vfs/socket
try
  agent.with do run cat /etc/os-release
finally
  agent.stop()
```

Be careful to suitably restrict access to the shared directory. `dolang-vfs`
will refuse to create a socket in a directory that is not exclusively
accessible by its owner (mode `0700`). When connecting, the socket path is
resolved through the current VFS context, so a container can be reached via an
[SSH context](./ssh.md).

This gives the following common lifetimes:

| Form                            | Container and VFS lifetime                                |
| ------------------------------- | --------------------------------------------------------- |
| `docker.with` / `podman.with`   | Temporary container and session for one block             |
| `docker.build` / `podman.build` | Temporary container spanning the build steps              |
| `Container` object              | Management handle                                         |
| Manual socket connection        | Controlled by the caller and external container lifecycle |

To target Docker on another host, enter that host first and then use the
ordinary Docker module:

```
import ssh docker

ssh.with builder.example.com do
  docker.with ubuntu:24.04 do
    run uname -a
```
