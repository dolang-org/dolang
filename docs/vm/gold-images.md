# Gold Images

`libvirt` allows exporting a guest to a template bundle from which new guests
can be rapidly provisioned. `libvirt.build` orchestrates creation, shutdown,
export, and deletion.

```
libvirt.build freebsd-gold.dolvm
  image: freebsd.qcow2.xz
  os: :FREEBSD:
  init:
    packages:
      - git
```

The result can then be passed as `bundle`:

```
let domain = libvirt.create
  bundle: freebsd-gold.dolvm
  app: dolang-libvirt-test
```

`Domain.export` performs the export operation without the surrounding
orchestration.

```
domain.shutdown()
domain.export freebsd-gold.dolvm
domain.undefine()
```

The resulting bundle is tied to the used `app:` and its associated persistent
state, such as ssh keys. See [Caching](./caching.md) for what to preserve
along with any bundles.

Because the bundle already answers them, `os:`, `arch:`, `user:`, `dolang:`,
and `init:` are not accepted alongside `bundle:`. `memory:`, `vcpus:`, and
`disk_size:` default to what the exported domain used, and may be overridden.

A bundle may be a local path or an HTTP(S) URL — it is fetched and cached like
any other download, and the [artifact spec](./provisioning.md#artifact-specs)'s
`digest:` pins it.

## The Bundle Format

A bundle is a ZIP, conventionally named with a `.dolvm` extension.

| Member          | Contents                                                                                                                                              |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `manifest.json` | Format version, `app`, `os`, `arch`, guest user, VFS command path, memory, vCPUs, the machine profile, and each member's name, size and BLAKE3 digest |
| `disk.qcow2`    | The flattened disk image                                                                                                                              |
| `nvram.bin`     | The UEFI variable store, for a guest that has one                                                                                                     |
| `tpm.tar`       | The vTPM state directory, for a guest that has one                                                                                                    |

The disk image is already compressed (`qemu-img convert -c`), so the ZIP
stores it uncompressed, and it stays compressed in the image store after
extraction. Its BLAKE3 digest is checked again when it is extracted. The
manifest also embeds the domain XML for reference; importing a bundle uses
the structured fields, not the XML.

## Preparing For Export

Prior to exporting the guest, it's best to remove anything
unneeded from within the guest and then trim unused disk blocks.
Windows also has additional special considerations.

### Linux

```
bind systemd.os_release()
  "ID": id
  "ID_LIKE": id_like = ""
  ...
let family = Set [id, ...id_like.split " "]
admin.with do
  if family.contains debian
    run apt-get clean
    run rm -rf /var/lib/apt/lists
  else if family.contains fedora
    run dnf clean all
  else if family.contains suse
    run zypper clean --all
  run journalctl --vacuum-size=1M
  run fstrim -a
```

`fstrim -a` covers every mounted filesystem that supports discard; a ZFS root
wants `zpool trim -w <pool>` instead.

### FreeBSD

```
admin.with do
  run pkg clean -ay
  run zpool trim -w zroot
```

UFS has no online batch trim: `tunefs -t enable` affects only later deletions
and `fsck_ffs -E` needs the filesystem unmounted. In a pinch, fill the free
space with zeroes instead: the disk is defined with `detect_zeroes="unmap"`, so
those writes return clusters to the image rather than filling it.

```
admin.with do
  try
    run dd if=/dev/zero of=/zeroes bs=1m
  catch proc.Error: _
    nil
  fs.sync /zeroes
  fs.remove /zeroes
  run sync
```

### Windows

!!! warning "Hibernation Considered Harmful"

    With Fast Startup on, Windows may shut down by partially hibernating, and
    the exported disk then tries to restore kernel state for virtual hardware
    that may have changed. Provisioning therefore disables hibernation by
    default. It also changes the default power button action to shutdown,
    so an ACPI power off event results in a clean shutdown. Changing these
    settings or explicitly stopping the guest via sleep or hibernation
    is not recommended.

```
run Dism /Online /Cleanup-Image /StartComponentCleanup /ResetBase
fs.remove C:\Windows\SoftwareDistribution\Download all: true ignore: true
run defrag "C:" /L
run C:\Windows\System32\Sysprep\sysprep.exe /generalize /oobe /quit
```

## Clean Shutdown

`export` refuses a domain that did not reach `:SHUTOFF:` through a guest
shutdown. `force: true` overrides this, but should be used with care.

## Windows Image

`sysprep` is the caller's as well, and generalizing is effectively terminal:
`sysprep /generalize /oobe` leaves a guest that comes up in OOBE on its next
boot, where `sshd` does not return, so a `reboot` after one only waits until it
times out. Run it with `/quit` rather than `/shutdown` and let `build` perform
the shutdown.

```
libvirt.build ./windows-gold.dolvm
  installer: Win11_24H2_x64.iso
  drivers: virtio-win.iso
  os: :WINDOWS:
  run: do
    run C:\Windows\System32\Sysprep\sysprep.exe /generalize /oobe /quit
```

The bundle carries the account named by `user:` and the password generated for
it, unless an action removes or disables the account before generalizing.
