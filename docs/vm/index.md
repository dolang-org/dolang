# Overview

The `libvirt` module provisions guests and exposes them as
[VFS](../shell/vfs.md) contexts over SSH. It uses an unprivileged
`qemu:///session` connection, `passt` networking, and a host-loopback SSH port
forward by default.

A guest is provisioned one of three ways, and exactly one of them is given:

- `image:` provisions a cloud-init image
- `installer:` boots install media onto a blank disk and provisions it with an
  unattended answer file (see [Windows Guests](./windows.md))
- `bundle:` restores a guest that was already provisioned once (see [Gold
  Images](./gold-images.md)).

The host needs `virsh`, `qemu-img`, `passt`, `cloud-localds`, `ssh`, and
`ssh-keygen`. `image:` may be a local path or an HTTP(S) URL; externally
compressed images (`.gz`, `.xz`, etc.) are decompressed into the durable image
store — see [Caching Between Runs](./caching.md) for what that means for CI.

The domain disk defaults to `20G`, or `64G` for a Windows guest. Set
`disk_size:` to another `qemu-img` size when needed. Cloud images with a
first-boot growfs service expand their partition and filesystem into the
additional space.

## Target Platform

`os:` is required, and `arch:` defaults to the host architecture. Both use the
same symbol vocabulary as
[`sys.os_info().os`](sys)/[`sys.cpu_info().arch`](sys).

The domain is defined for KVM with a `host-passthrough` CPU, so `arch:` must be
the host architecture; a foreign one is rejected rather than emulated.
