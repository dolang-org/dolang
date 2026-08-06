use super::Direct;
use crate::PosixAcl;
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
use crate::{PosixAce, PosixAclPermissions, PosixAclQualifier};
#[cfg(target_os = "freebsd")]
use std::os::fd::AsRawFd;
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
use std::{
    ffi::CString,
    os::{fd::AsFd, unix::ffi::OsStrExt},
};
use std::{io, path::Path};
use tokio::fs::File;

#[cfg(target_os = "linux")]
use super::unix::UnixXattrTarget;

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn canonical_entries(acl: &PosixAcl) -> Vec<PosixAce> {
    let mut entries = acl.entries().to_vec();
    entries.sort_by_key(|entry| match entry.qualifier {
        PosixAclQualifier::UserObj => (0, 0),
        PosixAclQualifier::User(id) => (1, id),
        PosixAclQualifier::GroupObj => (2, 0),
        PosixAclQualifier::Group(id) => (3, id),
        PosixAclQualifier::Mask => (4, 0),
        PosixAclQualifier::Other => (5, 0),
    });
    entries
}

#[cfg(target_os = "linux")]
const ACCESS_XATTR: &[u8] = b"system.posix_acl_access\0";
#[cfg(target_os = "linux")]
const DEFAULT_XATTR: &[u8] = b"system.posix_acl_default\0";

#[cfg(target_os = "linux")]
fn linux_name(default: bool) -> &'static std::ffi::CStr {
    std::ffi::CStr::from_bytes_with_nul(if default { DEFAULT_XATTR } else { ACCESS_XATTR }).unwrap()
}

#[cfg(target_os = "linux")]
fn missing_xattr(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODATA)
}

#[cfg(target_os = "linux")]
fn decode_linux(bytes: &[u8]) -> io::Result<PosixAcl> {
    if bytes.len() < 4 || !(bytes.len() - 4).is_multiple_of(8) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "POSIX ACL xattr has invalid length",
        ));
    }
    let version = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if version != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported POSIX ACL xattr version {version}"),
        ));
    }
    let mut entries = Vec::with_capacity((bytes.len() - 4) / 8);
    for raw in bytes[4..].chunks_exact(8) {
        let tag = u16::from_le_bytes(raw[0..2].try_into().unwrap());
        let perm = u16::from_le_bytes(raw[2..4].try_into().unwrap());
        let id = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        if perm & !7 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "POSIX ACL entry has invalid permissions",
            ));
        }
        let qualifier = match tag {
            0x01 if id == u32::MAX => PosixAclQualifier::UserObj,
            0x02 if id != u32::MAX => PosixAclQualifier::User(id),
            0x04 if id == u32::MAX => PosixAclQualifier::GroupObj,
            0x08 if id != u32::MAX => PosixAclQualifier::Group(id),
            0x10 if id == u32::MAX => PosixAclQualifier::Mask,
            0x20 if id == u32::MAX => PosixAclQualifier::Other,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "POSIX ACL entry has invalid tag or qualifier",
                ));
            }
        };
        entries.push(PosixAce {
            qualifier,
            permissions: PosixAclPermissions {
                read: perm & 4 != 0,
                write: perm & 2 != 0,
                execute: perm & 1 != 0,
            },
        });
    }
    PosixAcl::new(entries).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(target_os = "linux")]
fn encode_linux(acl: &PosixAcl) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + acl.entries().len() * 8);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    for entry in canonical_entries(acl) {
        let (tag, id) = match entry.qualifier {
            PosixAclQualifier::UserObj => (0x01u16, u32::MAX),
            PosixAclQualifier::User(id) => (0x02, id),
            PosixAclQualifier::GroupObj => (0x04, u32::MAX),
            PosixAclQualifier::Group(id) => (0x08, id),
            PosixAclQualifier::Mask => (0x10, u32::MAX),
            PosixAclQualifier::Other => (0x20, u32::MAX),
        };
        let perm = u16::from(entry.permissions.execute)
            | (u16::from(entry.permissions.write) << 1)
            | (u16::from(entry.permissions.read) << 2);
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&perm.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

#[cfg(target_os = "linux")]
fn linux_get(target: UnixXattrTarget<'_>, default: bool) -> io::Result<Option<PosixAcl>> {
    match Direct::unix_get_xattr(target, linux_name(default)) {
        Ok(bytes) => decode_linux(&bytes).map(Some),
        Err(error) if missing_xattr(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn linux_set(target: UnixXattrTarget<'_>, acl: Option<&PosixAcl>, default: bool) -> io::Result<()> {
    match acl {
        Some(acl) => Direct::unix_set_xattr(target, linux_name(default), &encode_linux(acl)),
        None => match Direct::unix_remove_xattr(target, linux_name(default)) {
            Err(error) if missing_xattr(&error) => Ok(()),
            result => result,
        },
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::*;
    use std::{ffi::c_void, ptr};

    type Acl = *mut c_void;
    type Entry = *mut c_void;
    type Permset = *mut u32;

    const ACL_BRAND_POSIX: libc::c_int = 1;
    const ACL_TYPE_ACCESS: u32 = 2;
    const ACL_TYPE_DEFAULT: u32 = 3;
    const ACL_USER_OBJ: u32 = 0x01;
    const ACL_USER: u32 = 0x02;
    const ACL_GROUP_OBJ: u32 = 0x04;
    const ACL_GROUP: u32 = 0x08;
    const ACL_MASK: u32 = 0x10;
    const ACL_OTHER: u32 = 0x20;
    const ACL_EXECUTE: u32 = 0x01;
    const ACL_WRITE: u32 = 0x02;
    const ACL_READ: u32 = 0x04;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    const ACL_NEXT_ENTRY: libc::c_int = 1;

    unsafe extern "C" {
        fn acl_add_perm(permset: Permset, permission: u32) -> libc::c_int;
        fn acl_clear_perms(permset: Permset) -> libc::c_int;
        fn acl_create_entry(acl: *mut Acl, entry: *mut Entry) -> libc::c_int;
        fn acl_delete_fd_np(fd: libc::c_int, acl_type: u32) -> libc::c_int;
        fn acl_delete_file_np(path: *const libc::c_char, acl_type: u32) -> libc::c_int;
        fn acl_delete_link_np(path: *const libc::c_char, acl_type: u32) -> libc::c_int;
        fn acl_extended_file_np(path: *const libc::c_char) -> libc::c_int;
        fn acl_extended_link_np(path: *const libc::c_char) -> libc::c_int;
        fn acl_free(object: *mut c_void) -> libc::c_int;
        fn acl_get_brand_np(acl: Acl, brand: *mut libc::c_int) -> libc::c_int;
        fn acl_get_entry(acl: Acl, entry_id: libc::c_int, entry: *mut Entry) -> libc::c_int;
        fn acl_get_fd_np(fd: libc::c_int, acl_type: u32) -> Acl;
        fn acl_get_file(path: *const libc::c_char, acl_type: u32) -> Acl;
        fn acl_get_link_np(path: *const libc::c_char, acl_type: u32) -> Acl;
        fn acl_get_perm_np(permset: Permset, permission: u32) -> libc::c_int;
        fn acl_get_permset(entry: Entry, permset: *mut Permset) -> libc::c_int;
        fn acl_get_qualifier(entry: Entry) -> *mut c_void;
        fn acl_get_tag_type(entry: Entry, tag: *mut u32) -> libc::c_int;
        fn acl_init(count: libc::c_int) -> Acl;
        fn acl_is_trivial_np(acl: Acl, trivial: *mut libc::c_int) -> libc::c_int;
        fn acl_set_fd_np(fd: libc::c_int, acl: Acl, acl_type: u32) -> libc::c_int;
        fn acl_set_file(path: *const libc::c_char, acl_type: u32, acl: Acl) -> libc::c_int;
        fn acl_set_link_np(path: *const libc::c_char, acl_type: u32, acl: Acl) -> libc::c_int;
        fn acl_set_permset(entry: Entry, permset: Permset) -> libc::c_int;
        fn acl_set_qualifier(entry: Entry, qualifier: *const c_void) -> libc::c_int;
        fn acl_set_tag_type(entry: Entry, tag: u32) -> libc::c_int;
        fn acl_strip_np(acl: Acl, recalculate_mask: libc::c_int) -> Acl;
    }

    struct OwnedAcl(Acl);

    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }

    fn acl_type(default: bool) -> u32 {
        if default {
            ACL_TYPE_DEFAULT
        } else {
            ACL_TYPE_ACCESS
        }
    }

    fn call(result: libc::c_int) -> io::Result<()> {
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    unsafe fn get_native(
        path: Option<&std::ffi::CStr>,
        fd: libc::c_int,
        default: bool,
        follow: bool,
    ) -> Acl {
        let acl_type = acl_type(default);
        if fd >= 0 {
            unsafe { acl_get_fd_np(fd, acl_type) }
        } else if follow {
            unsafe { acl_get_file(path.unwrap().as_ptr(), acl_type) }
        } else {
            unsafe { acl_get_link_np(path.unwrap().as_ptr(), acl_type) }
        }
    }

    pub(super) fn get(
        path: Option<&std::ffi::CStr>,
        fd: libc::c_int,
        default: bool,
        follow: bool,
    ) -> io::Result<Option<PosixAcl>> {
        if !default && fd < 0 {
            let result = unsafe {
                if follow {
                    acl_extended_file_np(path.unwrap().as_ptr())
                } else {
                    acl_extended_link_np(path.unwrap().as_ptr())
                }
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Ok(None);
            }
        }

        let acl = OwnedAcl(unsafe { get_native(path, fd, default, follow) });
        if acl.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut brand = 0;
        call(unsafe { acl_get_brand_np(acl.0, &mut brand) })?;
        if brand != ACL_BRAND_POSIX {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem ACL is not POSIX.1e",
            ));
        }
        if !default {
            let mut trivial = 0;
            call(unsafe { acl_is_trivial_np(acl.0, &mut trivial) })?;
            if trivial != 0 {
                return Ok(None);
            }
        }

        let mut entries = Vec::new();
        let mut entry = ptr::null_mut();
        let mut result = unsafe { acl_get_entry(acl.0, ACL_FIRST_ENTRY, &mut entry) };
        while result == 1 {
            let mut tag = 0;
            let mut permset = ptr::null_mut();
            call(unsafe { acl_get_tag_type(entry, &mut tag) })?;
            call(unsafe { acl_get_permset(entry, &mut permset) })?;
            let qualifier = match tag {
                ACL_USER_OBJ => PosixAclQualifier::UserObj,
                ACL_GROUP_OBJ => PosixAclQualifier::GroupObj,
                ACL_MASK => PosixAclQualifier::Mask,
                ACL_OTHER => PosixAclQualifier::Other,
                ACL_USER | ACL_GROUP => {
                    let value = unsafe { acl_get_qualifier(entry) };
                    if value.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let id = unsafe { *(value.cast::<u32>()) };
                    unsafe {
                        acl_free(value);
                    }
                    if tag == ACL_USER {
                        PosixAclQualifier::User(id)
                    } else {
                        PosixAclQualifier::Group(id)
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "FreeBSD ACL contains a non-POSIX entry",
                    ));
                }
            };
            let has = |permission| {
                let value = unsafe { acl_get_perm_np(permset, permission) };
                if value < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(value != 0)
                }
            };
            entries.push(PosixAce {
                qualifier,
                permissions: PosixAclPermissions {
                    read: has(ACL_READ)?,
                    write: has(ACL_WRITE)?,
                    execute: has(ACL_EXECUTE)?,
                },
            });
            result = unsafe { acl_get_entry(acl.0, ACL_NEXT_ENTRY, &mut entry) };
        }
        call(result)?;
        if entries.is_empty() {
            Ok(None)
        } else {
            PosixAcl::new(entries)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
    }

    unsafe fn delete_native(
        path: Option<&std::ffi::CStr>,
        fd: libc::c_int,
        default: bool,
        follow: bool,
    ) -> libc::c_int {
        let acl_type = acl_type(default);
        if fd >= 0 {
            unsafe { acl_delete_fd_np(fd, acl_type) }
        } else if follow {
            unsafe { acl_delete_file_np(path.unwrap().as_ptr(), acl_type) }
        } else {
            unsafe { acl_delete_link_np(path.unwrap().as_ptr(), acl_type) }
        }
    }

    unsafe fn set_native(
        path: Option<&std::ffi::CStr>,
        fd: libc::c_int,
        default: bool,
        follow: bool,
        acl: Acl,
    ) -> libc::c_int {
        let acl_type = acl_type(default);
        if fd >= 0 {
            unsafe { acl_set_fd_np(fd, acl, acl_type) }
        } else if follow {
            unsafe { acl_set_file(path.unwrap().as_ptr(), acl_type, acl) }
        } else {
            unsafe { acl_set_link_np(path.unwrap().as_ptr(), acl_type, acl) }
        }
    }

    fn strip_access(
        path: Option<&std::ffi::CStr>,
        fd: libc::c_int,
        follow: bool,
    ) -> io::Result<()> {
        let current = OwnedAcl(unsafe { get_native(path, fd, false, follow) });
        if current.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut trivial = 0;
        call(unsafe { acl_is_trivial_np(current.0, &mut trivial) })?;
        if trivial != 0 {
            return Ok(());
        }

        // Removing an extended ACL must leave the owning group's effective
        // permissions in the base ACL.  acl_strip_np(acl, 1) recalculates and
        // re-adds a mask entry, so apply the old mask to ACL_GROUP_OBJ first
        // and then strip without recalculating it.
        let mut group_entry = ptr::null_mut();
        let mut group_permset = ptr::null_mut();
        let mut mask_permissions = None;
        let mut entry = ptr::null_mut();
        let mut result = unsafe { acl_get_entry(current.0, ACL_FIRST_ENTRY, &mut entry) };
        while result == 1 {
            let mut tag = 0;
            call(unsafe { acl_get_tag_type(entry, &mut tag) })?;
            if tag == ACL_GROUP_OBJ || tag == ACL_MASK {
                let mut permset = ptr::null_mut();
                call(unsafe { acl_get_permset(entry, &mut permset) })?;
                if tag == ACL_GROUP_OBJ {
                    group_entry = entry;
                    group_permset = permset;
                } else {
                    let mut permissions = 0;
                    for permission in [ACL_READ, ACL_WRITE, ACL_EXECUTE] {
                        let value = unsafe { acl_get_perm_np(permset, permission) };
                        if value < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        if value != 0 {
                            permissions |= permission;
                        }
                    }
                    mask_permissions = Some(permissions);
                }
            }
            result = unsafe { acl_get_entry(current.0, ACL_NEXT_ENTRY, &mut entry) };
        }
        call(result)?;

        if let Some(mask_permissions) = mask_permissions {
            if group_permset.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "POSIX ACL has no owning-group entry",
                ));
            }
            let mut group_permissions = 0;
            for permission in [ACL_READ, ACL_WRITE, ACL_EXECUTE] {
                let value = unsafe { acl_get_perm_np(group_permset, permission) };
                if value < 0 {
                    return Err(io::Error::last_os_error());
                }
                if value != 0 {
                    group_permissions |= permission;
                }
            }
            call(unsafe { acl_clear_perms(group_permset) })?;
            for permission in [ACL_READ, ACL_WRITE, ACL_EXECUTE] {
                if group_permissions & mask_permissions & permission != 0 {
                    call(unsafe { acl_add_perm(group_permset, permission) })?;
                }
            }
            call(unsafe { acl_set_permset(group_entry, group_permset) })?;
        }

        let stripped = OwnedAcl(unsafe { acl_strip_np(current.0, 0) });
        if stripped.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        call(unsafe { set_native(path, fd, false, follow, stripped.0) })
    }

    pub(super) fn set(
        path: Option<&std::ffi::CStr>,
        fd: libc::c_int,
        acl: Option<&PosixAcl>,
        default: bool,
        follow: bool,
    ) -> io::Result<()> {
        let Some(acl) = acl else {
            if !default {
                return strip_access(path, fd, follow);
            }
            let result = unsafe { delete_native(path, fd, default, follow) };
            return match call(result) {
                Err(error) if error.raw_os_error() == Some(libc::ENOATTR) => Ok(()),
                result => result,
            };
        };

        let mut native = OwnedAcl(unsafe {
            acl_init(
                acl.entries()
                    .len()
                    .try_into()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ACL is too large"))?,
            )
        });
        if native.0.is_null() {
            return Err(io::Error::last_os_error());
        }
        for ace in canonical_entries(acl) {
            let (tag, id) = match ace.qualifier {
                PosixAclQualifier::UserObj => (ACL_USER_OBJ, None),
                PosixAclQualifier::User(id) => (ACL_USER, Some(id)),
                PosixAclQualifier::GroupObj => (ACL_GROUP_OBJ, None),
                PosixAclQualifier::Group(id) => (ACL_GROUP, Some(id)),
                PosixAclQualifier::Mask => (ACL_MASK, None),
                PosixAclQualifier::Other => (ACL_OTHER, None),
            };
            let mut entry = ptr::null_mut();
            call(unsafe { acl_create_entry(&mut native.0, &mut entry) })?;
            call(unsafe { acl_set_tag_type(entry, tag) })?;
            if let Some(id) = id {
                call(unsafe { acl_set_qualifier(entry, (&id as *const u32).cast()) })?;
            }
            let mut permset = ptr::null_mut();
            call(unsafe { acl_get_permset(entry, &mut permset) })?;
            call(unsafe { acl_clear_perms(permset) })?;
            for (enabled, permission) in [
                (ace.permissions.read, ACL_READ),
                (ace.permissions.write, ACL_WRITE),
                (ace.permissions.execute, ACL_EXECUTE),
            ] {
                if enabled {
                    call(unsafe { acl_add_perm(permset, permission) })?;
                }
            }
            call(unsafe { acl_set_permset(entry, permset) })?;
        }
        call(unsafe { set_native(path, fd, default, follow, native.0) })
    }
}

impl Direct {
    pub(super) fn acl_from_path(
        path: &Path,
        default: bool,
        follow: bool,
    ) -> io::Result<Option<PosixAcl>> {
        #[cfg(target_os = "linux")]
        {
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
            linux_get(UnixXattrTarget::Path(&path, follow), default)
        }
        #[cfg(target_os = "freebsd")]
        {
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
            freebsd::get(Some(&path), -1, default, follow)
        }
        #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
        {
            let _ = (path, default, follow);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX ACLs are not supported on this platform",
            ))
        }
    }

    pub(super) fn set_acl_path(
        path: &Path,
        acl: Option<&PosixAcl>,
        default: bool,
        follow: bool,
    ) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
            linux_set(UnixXattrTarget::Path(&path, follow), acl, default)
        }
        #[cfg(target_os = "freebsd")]
        {
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
            freebsd::set(Some(&path), -1, acl, default, follow)
        }
        #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
        {
            let _ = (path, acl, default, follow);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX ACLs are not supported on this platform",
            ))
        }
    }

    pub(super) fn acl_from_file(file: &File, default: bool) -> io::Result<Option<PosixAcl>> {
        #[cfg(target_os = "linux")]
        {
            linux_get(UnixXattrTarget::Fd(file.as_fd()), default)
        }
        #[cfg(target_os = "freebsd")]
        {
            freebsd::get(None, file.as_fd().as_raw_fd(), default, true)
        }
        #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
        {
            let _ = (file, default);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX ACLs are not supported on this platform",
            ))
        }
    }

    pub(super) fn set_acl_file(
        file: &File,
        acl: Option<&PosixAcl>,
        default: bool,
    ) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            linux_set(UnixXattrTarget::Fd(file.as_fd()), acl, default)
        }
        #[cfg(target_os = "freebsd")]
        {
            freebsd::set(None, file.as_fd().as_raw_fd(), acl, default, true)
        }
        #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
        {
            let _ = (file, acl, default);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX ACLs are not supported on this platform",
            ))
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn linux_acl_packet_round_trip() {
        let acl = PosixAcl::new(vec![
            PosixAce {
                qualifier: PosixAclQualifier::UserObj,
                permissions: PosixAclPermissions {
                    read: true,
                    write: true,
                    execute: false,
                },
            },
            PosixAce {
                qualifier: PosixAclQualifier::GroupObj,
                permissions: PosixAclPermissions {
                    read: true,
                    write: false,
                    execute: false,
                },
            },
            PosixAce {
                qualifier: PosixAclQualifier::Other,
                permissions: PosixAclPermissions::default(),
            },
        ])
        .unwrap();
        assert_eq!(decode_linux(&encode_linux(&acl)).unwrap(), acl);
    }
}
