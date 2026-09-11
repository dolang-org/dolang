# Data and Caches

Everything `libvirt` persists lives under two directories, scoped by `app:`
(default: derived from `shell.program`).

`$XDG_DATA_HOME/<app>/libvirt/`:

| Path           | Contents                                |
| -------------- | --------------------------------------- |
| `ssh/`         | SSH keys                                |
| `seeds/`       | Cloud-init seed ISOs                    |
| `provision/`   | Unattended-install provisioning discs   |
| `images/`      | Base disk images                        |
| `domains/`     | Per-domain working state                |
| `screenshots/` | Consoles captured when a wait timed out |

`$XDG_CACHE_HOME/<app>/`:

| Path        | Contents       |
| ----------- | -------------- |
| `transfer/` | Download cache |

Of these, it is recommended that you retain `ssh/` and `seeds/` in
`$XDG_DATA_HOME` and the entire cache directory in `$XDG_CACHE_HOME`. SSH
public keys are baked into seed ISOs and exported VMs, so they are the
most critical to retain across build runs.

Pass an explicit `app:` if you need to use the same data directory and cache
from multiple scripts, as the default is derived from `shell.program`.
