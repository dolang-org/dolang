# SSH Remoting

The `ssh` module runs a block on another host through a remote
[VFS context](./vfs.md). Filesystem access, external programs,
environment, working directory, system information, and security queries use
the remote host for the duration of the block.

## Running a Remote Block

The remote host must provide a matching `dolang-vfs` in the command search
path:

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

## Running a Script over SSH

The bundled `ssh` entrypoint compiles a local script and runs it with an SSH
VFS context:

```
dolang -m ssh --batch --user builder build.example.com build.dol release
```

The destination and script follow the SSH options. Every argument after the
script path is passed through unchanged, including arguments beginning with
`-`. Inside the target script, `shell.program` is the script path and
`shell.args` contains only those trailing arguments.

`-m` runs a bundled entrypoint over the connection instead of a local script,
using the same `-m NAME` spelling as the top-level command line:

```
dolang -m ssh --batch build.example.com -m test tests/
```

The entrypoint sees its own name as `shell.program`, so it behaves as it would
when run directly.

The entrypoint accepts the connection options documented below using
hyphenated command-line names, such as `--identity`, `--jump`,
`--connect-timeout`, and `--host-key accept-new`. Repeated identities and jump
hosts preserve their order.

Use `--command` with a shell-quoted string to replace the remote VFS launcher:

```
dolang -m ssh --command "sudo -n /opt/dolang-vfs" host script.dol
```

The string is split into arguments locally; it is not run through a remote
shell. `--cd` selects the initial remote directory. Remote environment
overrides may be repeated:

```
dolang -m ssh \
  --env MODE=release \
  --env HOME \
  --unset-env DEBUG \
  host script.dol
```

`NAME=VALUE` sets a value, while a bare `NAME` inherits its local value.
`--unset-env` takes precedence when the same name is also passed with
`--env`. `--no-login-env` skips the login environment import described below,
and `--login-shell` names the shell to import it from. Run
`dolang -m ssh --help` for the complete option list.

Authentication is delegated to the user's `ssh` executable, configuration,
agent, and credential providers. Do does not replace or weaken that setup.

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
- `known_hosts:` selects a known-hosts file instead of the user's default.
- `command:` replaces the remote VFS command when it is installed elsewhere.
- `login_env:` controls the login environment import described below.

SSH configuration continues to supply settings not represented by these
options. Use `:STRICT:` for hosts whose keys have already been provisioned;
`:ACCEPT_NEW:` trusts a host on first use but still rejects a changed key.
The default policy is the user's normal SSH host-key policy.

## Login Environment

SSH runs the remote command through a non-login shell, so typically nothing in
the account's profile has run when the VFS starts. Tools installed under e.g.
`~/.local/bin` are typically missing from `PATH` as a result.

The remote VFS therefore reconstructs the missing environment before serving
requests. How it does so depends on the target.

On Unix hosts it runs the shell from the remote passwd database as a login
shell, which re-executes the VFS binary in a probe mode that reports the
resulting environment over a dedicated pipe. The shell is launched as a login
shell by prefixing its `argv[0]` with `-`, with the command to launch passed
via `-c`. It is confined to its own process group, which is terminated once
the environment has been read.

On Windows, the OpenSSH server constructs the command environment by
concatenating `PATH` from applicable HKCU/HKLM registry keys and inheriting the
rest from the `sshd` service account, so a remote command inherits
`LocalSystem`'s `TEMP`, `USERPROFILE` and `APPDATA`, and never sees user
variables such as `CARGO_HOME`. The VFS reads the machine and user environment
keys from the registry itself, resolves `USERPROFILE`, `APPDATA`, and
`LOCALAPPDATA` from the known folders of its own token, and expands
`REG_EXPAND_SZ` values against those. `PATH` is left as `sshd` composed it.

`env:` overrides are applied after the import, so they always win. Pass
`login_env: false` for hosts whose profiles are slow or broken, or a path to
import from a specific shell instead of the one in the passwd database. A shell
path has no meaning for a Windows target and is ignored there, so it is safe to
pass without knowing which operating system will answer:

```
ssh.with build.example.com login_env: /bin/zsh do
  run cargo build --release
```

`ssh.with` uses `ssh -T`, so it does not allocate a remote terminal. External
programs can use captured output, redirection, and pipes, but programs that
require an interactive TTY are not supported through this helper.

When the block returns, throws, or is canceled, `ssh.with` stops the VFS and
waits for the SSH stream to finish. Connection failure raises an error and
invalidates files and other handles opened through that session.

## Remote Argument Encoding

`ssh` joins the command it is given with spaces and hands the result to the
remote account's shell, which splits it again. There is no quoting style which
is reliable for both Unix and Windows hosts. The VFS helper therefore accepts a
`-base64` form of every option that carries a value (`--cd-base64`,
`--set-base64`, `--unset-base64`, `--login-env-base64=`). The values of `cd:`,
`env:`, and `login_env:` are sent that way whenever they contain anything
requiring quoting. Values that cannot be misparsed are sent as-is, so an
ordinary command line stays legible in `ps`.

`command:` is exempt: it is a remote command line you are writing directly, so
it is passed through untouched and any quoting in it is yours to supply.

## Cross-Platform Targets

The interpreter and target do not need to use the same operating system family.
Platform-specific APIs and behavior track the current VFS context, not that of
the host. For example, from a Unix host:

```
ssh.with windows-server.example.com do
  echo "OS: $(sys.os_info().os)"
  let name_sid = security.windows.token_info().user_sid.lookup()
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

The SSH server and remote `dolang-vfs` execute with the selected SSH account's
authority. Treat access to that account and its VFS stream as access to all
operations available to that identity.
