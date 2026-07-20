# SSH Remoting

The `ssh` module runs a block on another host through a remote
[VFS context](./index.md). Filesystem access, external programs,
environment, working directory, system information, and security queries use
the remote host for the duration of the block.

## Running a Remote Block

The remote host must provide `dolang-vfs` in the command search path:

```
import ssh sys

ssh.with build.example.com
  user: builder
  identity: ~/.ssh/build
  batch: true
  host_key: :STRICT:
  do
    echo "target: $(sys.os_info().os)/$(sys.cpu_info().arch)"
    cd /srv/build do
      run git pull --ff-only
      run cargo build --release
```

The client launches `dolang-vfs --stdio` as the remote command and carries the
VFS protocol over its standard input and output. This is a protocol tunnel
through SSH, not an SSH filesystem mount or a TCP port forward. The remote
command must not write other data to standard output; diagnostics may use
standard error.

`command:` replaces `dolang-vfs` with a launcher prefix. The client appends
`--cd`, `--set`, and `--unset` overrides followed by `--stdio`; custom launchers
must pass through or interpret those trailing `dolang-vfs` arguments.

`ssh.with` stops the VFS session when the block returns or throws. Prefer it to
constructing a stream-backed `Vfs` manually.

## Connection Options

`ssh.with` accepts the common options needed for unattended automation:

- `user:` and `port:` select the SSH account and server port.
- `identity:` may be repeated; `identities_only:` restricts authentication to
  the configured identities.
- `jump:` may be repeated to form a proxy-jump chain.
- `forward_agent:` enables authentication-agent forwarding. It is disabled by
  default.
- `connect_timeout:`, `keepalive_interval:`, and `keepalive_count:` control
  connection failure detection.
- `batch:` disables interactive SSH prompts.
- `host_key:` accepts `:DEFAULT:`, `:STRICT:`, or `:ACCEPT_NEW:`.
- `command:` replaces the remote VFS command when it is installed elsewhere.

SSH configuration continues to supply settings not represented by these
options. Use `:STRICT:` for hosts whose keys have already been provisioned;
`:ACCEPT_NEW:` trusts a host on first use but still rejects a changed key.

## Cross-Platform Targets

The interpreter and target do not need to use the same operating system family.
Platform-specific APIs and behavior track the current VFS context, not that of
the host. For example, from a Unix host:

```
ssh.with windows-server.example.com do
  echo "OS: $(sys.os_info().os)"
  let name_sid = security.token_info().user_sid.lookup()
  echo "User: $(name_sid.name) ($(name_sid.sid) $(name_sid.kind))"
  echo "AppData hidden: $(fs.metadata("AppData").attrs.hidden)"
```

## Combining SSH and other VFS Targets

Container access can be used on a remote host:

```
import ssh docker

ssh.with builder.example.com do
  docker.with ubuntu:24.04 do
    run_build()
```

Unix privilege elevation also composes with SSH: `admin.with` or `sudo.with`
inside an ssh block invokes `sudo` on the remote Unix host. `sudo` within a
container within an `ssh` block likewise works as expected. UAC elevation on
Windows hosts is not supported in this manner.
