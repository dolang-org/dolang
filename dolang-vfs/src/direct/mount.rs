//! Finding mount points on Unix.
//!
//! A mount root is only an explicit mount: a btrfs subvolume reached by
//! ordinary traversal has a device of its own but isn't one.

use std::{ffi::CStr, os::fd::BorrowedFd, path::Path};
#[cfg(target_os = "linux")]
use std::{mem::MaybeUninit, os::fd::AsRawFd};

/// `struct statx`, as far as `stx_mnt_id`, padded to the full size the kernel
/// writes.
#[cfg(target_os = "linux")]
#[repr(C)]
pub(super) struct Statx {
    pub(super) mask: u32,
    _blksize: u32,
    pub(super) attributes: u64,
    _head: [u32; 10],
    pub(super) attributes_mask: u64,
    _times: [u64; 8],
    _rdev: [u32; 2],
    _dev: [u32; 2],
    pub(super) mnt_id: u64,
    _tail: [u64; 13],
}
#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::size_of::<Statx>() == 0x100);
#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::offset_of!(Statx, attributes_mask) == 0x38);
#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::offset_of!(Statx, mnt_id) == 0x90);

#[cfg(target_os = "linux")]
pub(super) const STATX_MNT_ID: u32 = 0x1000;
#[cfg(target_os = "linux")]
const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000;

/// Calls `statx` through the raw syscall: the libc wrapper and struct are
/// missing on older musl. `dir` of `None` means the working directory.
#[cfg(target_os = "linux")]
pub(super) fn statx(
    dir: Option<BorrowedFd<'_>>,
    path: &CStr,
    flags: libc::c_int,
    mask: u32,
) -> Option<Statx> {
    let mut buf = MaybeUninit::<Statx>::zeroed();
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dir.map_or(libc::AT_FDCWD, |dir| dir.as_raw_fd()),
            path.as_ptr(),
            flags,
            mask,
            buf.as_mut_ptr(),
        )
    };
    (rc == 0).then(|| unsafe { buf.assume_init() })
}

/// Returns whether the directory `name` in `dir` (the working directory when
/// `None`) is the root of a mount. `path` names the same directory, `dev` is
/// its device and `parent_dev` that of the directory containing it, if known.
///
/// Linux before 5.8 can only find mounts with a device of their own when
/// `parent_dev` is given, so a bind mount from the same filesystem goes unseen.
pub(super) fn is_mount_root(
    dir: Option<BorrowedFd<'_>>,
    name: &CStr,
    path: &Path,
    dev: u64,
    parent_dev: Option<u64>,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Some(stat) = statx(dir, name, libc::AT_SYMLINK_NOFOLLOW, 0)
            && stat.attributes_mask & STATX_ATTR_MOUNT_ROOT != 0
        {
            return stat.attributes & STATX_ATTR_MOUNT_ROOT != 0;
        }
        if parent_dev == Some(dev) {
            return false;
        }
        let Ok(path) = std::fs::canonicalize(path) else {
            return false;
        };
        std::fs::read("/proc/self/mountinfo")
            .is_ok_and(|mountinfo| mountinfo_has_mount_point(&mountinfo, path.as_os_str()))
    }
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let _ = (dir, name);
        let parent_dev = match parent_dev {
            Some(parent_dev) => parent_dev,
            None => {
                use std::os::unix::fs::MetadataExt;
                let Some(parent) = path.parent() else {
                    return true;
                };
                match std::fs::metadata(parent) {
                    Ok(metadata) => metadata.dev(),
                    Err(_) => return false,
                }
            }
        };
        // Every BSD mount has a device of its own; a change of device alone
        // also covers macOS firmlinks, which aren't mounts
        if parent_dev == dev {
            return false;
        }
        let (Ok(stat), Ok(path)) = (
            super::Direct::statfs_from_path(path),
            std::fs::canonicalize(path),
        ) else {
            return false;
        };
        let mount_point: Vec<u8> = stat
            .f_mntonname
            .iter()
            .map(|&c| c as u8)
            .take_while(|&b| b != 0)
            .collect();
        mount_point == path.as_os_str().as_bytes()
    }
}

/// Decodes the octal escapes (`\040` for a space) in a mountinfo field.
#[cfg(target_os = "linux")]
fn mountinfo_unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let escaped = field
            .get(i + 1..i + 4)
            .filter(|_| field[i] == b'\\')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if let Some(byte) = escaped {
            out.push(byte);
            i += 4;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    out
}

/// Returns whether `/proc/self/mountinfo` content lists `path` as a mount
/// point.
#[cfg(target_os = "linux")]
fn mountinfo_has_mount_point(mountinfo: &[u8], path: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    mountinfo.split(|&b| b == b'\n').any(|line| {
        // Mount ID, parent ID, major:minor and root precede the mount point
        line.split(|&b| b == b' ')
            .nth(4)
            .is_some_and(|field| mountinfo_unescape(field) == path.as_bytes())
    })
}

/// Finds the filesystem type of mount `mount_id` in `/proc/self/mountinfo`
/// content: the field after the ` - ` separator, with octal escapes decoded.
#[cfg(target_os = "linux")]
pub(super) fn mountinfo_fs_type(mountinfo: &[u8], mount_id: u64) -> Option<String> {
    mountinfo.split(|&b| b == b'\n').find_map(|line| {
        let mut fields = line.split(|&b| b == b' ');
        let id: u64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
        if id != mount_id {
            return None;
        }
        // Skip parent ID, major:minor, root, and mount point, then the
        // variable-length optional fields up to the separator.
        fields.nth(3)?;
        fields.find(|field| *field == b"-")?;
        let name = mountinfo_unescape(fields.next()?);
        String::from_utf8(name).ok().filter(|name| !name.is_empty())
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{mountinfo_fs_type, mountinfo_has_mount_point};
    use std::ffi::OsStr;

    const MOUNTINFO: &[u8] = b"\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
36 22 0:32 / / rw,relatime shared:1 master:2 - btrfs /dev/sda2 rw,subvol=/root
41 36 0:40 / /mnt/a\\040b rw,relatime - fuse.my\\040fs remote:/ rw,user_id=0
42 36 0:41 / /mnt/- rw - tmpfs tmpfs rw
";

    #[test]
    fn mountinfo_lookup() {
        assert_eq!(mountinfo_fs_type(MOUNTINFO, 22).as_deref(), Some("proc"));
        assert_eq!(mountinfo_fs_type(MOUNTINFO, 36).as_deref(), Some("btrfs"));
        assert_eq!(
            mountinfo_fs_type(MOUNTINFO, 41).as_deref(),
            Some("fuse.my fs")
        );
        assert_eq!(mountinfo_fs_type(MOUNTINFO, 42).as_deref(), Some("tmpfs"));
        assert_eq!(mountinfo_fs_type(MOUNTINFO, 99), None);
    }

    #[test]
    fn mountinfo_mount_points() {
        for path in ["/proc", "/", "/mnt/a b", "/mnt/-"] {
            assert!(
                mountinfo_has_mount_point(MOUNTINFO, OsStr::new(path)),
                "{path}"
            );
        }
        for path in ["/mnt", "/mnt/a\\040b", "/proc/self"] {
            assert!(
                !mountinfo_has_mount_point(MOUNTINFO, OsStr::new(path)),
                "{path}"
            );
        }
    }
}
