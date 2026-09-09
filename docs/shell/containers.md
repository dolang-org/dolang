# Containers

The `docker` and `podman` modules allow managing containers and using [VFS
contexts](./vfs.md) to run Do functions in container contexts. Filesystem
operations, external program launching, and other supported APIs target the
container, while the interpreter remains on the host.

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

`run`, `with`, and `build` also accept a `pull:` policy.
Starting a container waits for its VFS agent to come up with no built-in
timeout; wrap the call in `time.timeout` if a bound is needed.

Use [`with_host`](./vfs.md#returning-to-the-host) to temporarily return to the
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
    - type: :CACHE:
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

## Management

The Docker and Podman modules also provide a small management API:

- `images` and `containers` list and filter objects.
- `image` and `container` inspect an image reference or container name/ID.
- `Image`s expose metadata and can be tagged, removed, saved, loaded, pulled,
  and pushed.
- `Container`s expose metadata and can be started, stopped, killed, restarted,
  or removed.

Use [`docker.create`](docker) or
[`podman.create`](podman) when configuration and execution need
separate phases:

```
import docker

let ctr = docker.create ubuntu:24.04 -c "exit 42"
  name: example
  entrypoint: /bin/sh
  env:
    MODE: batch
  mounts:
    - type: :BIND:
      source: ./input
      target: /input
      readonly: true
  labels:
    app: example
  ports:
    - container_port: 8080
      protocol: :TCP:
  networks:
    - bridge
  user: 1000:1000
  cd: /input
  restart:
    policy: :NO:

ctr.start()
try
  ctr.wait()
catch docker.ContainerExitError: err
  echo "container exited with status $(err.rc)"
finally
  ctr.remove force: true
```

See the [`docker`](docker) and
[`podman`](podman) references for the complete interfaces.

`Container` objects inspect and change the lifecycle of existing containers.
Their `with` method copies `dolang-vfs` to a session-specific path under
`/tmp`, starts it through `docker exec` or `podman exec`, and runs a block with
that container as its VFS target. The copied helper uses the standard I/O
transport, so file and process handles use opaque RPC identities rather than
Unix `SCM_RIGHTS` handle passing.

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

Be careful to suitably restrict access to the shared directory and ensure UID
mappings are accurate. `dolang-vfs` will refuse to create a socket in a
directory that is not exclusively accessible by its owner (mode `0700`).
A shared secret can be used to increase security; see
[Connections](./vfs.md#connections).
