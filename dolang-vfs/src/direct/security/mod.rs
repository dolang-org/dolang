use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

use std::fs::File;

use super::Direct;
use crate::error::{Error, ErrorKind, Result};
#[cfg(target_os = "macos")]
use crate::security::MacosAcl;
#[cfg(target_os = "freebsd")]
use crate::security::Nfs4Acl;
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
use crate::security::PosixAcl;
use crate::{
    metadata::Resolve,
    security::{Acl, AclKind},
};

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn canonical_entries(acl: &PosixAcl) -> Vec<crate::security::PosixAce> {
    use crate::security::PosixAclQualifier;

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

fn cpath(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

fn unsupported_kind(kind: AclKind) -> Error {
    let what = match kind {
        AclKind::Posix => "POSIX ACLs",
        AclKind::Nfs4 => "NFSv4 ACLs",
        AclKind::Macos => "macOS ACLs",
    };
    Error::new(
        ErrorKind::Unsupported,
        format!("{what} are not supported on this platform"),
    )
}

fn check_default(kind: AclKind, default: bool) -> Result<()> {
    if default && kind != AclKind::Posix {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "default: is only meaningful for POSIX ACLs",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn split_posix(kind: AclKind, acl: Option<&Acl>) -> Result<Option<&PosixAcl>> {
    match (kind, acl) {
        (AclKind::Posix, None) => Ok(None),
        (AclKind::Posix, Some(Acl::Posix(posix))) => Ok(Some(posix)),
        (AclKind::Posix, Some(_)) => Err(Error::new(
            ErrorKind::InvalidInput,
            "kind: POSIX but value is not a POSIX ACL",
        )),
        (AclKind::Nfs4 | AclKind::Macos, _) => {
            unreachable!("caller must route AclKind::Nfs4/Macos separately")
        }
    }
}

#[cfg(target_os = "freebsd")]
fn split_nfs4(kind: AclKind, acl: Option<&Acl>) -> Result<Option<&Nfs4Acl>> {
    match (kind, acl) {
        (AclKind::Nfs4, None) => Ok(None),
        (AclKind::Nfs4, Some(Acl::Nfs4(nfs4))) => Ok(Some(nfs4)),
        (AclKind::Nfs4, Some(_)) => Err(Error::new(
            ErrorKind::InvalidInput,
            "kind: NFS4 but value is not an NFSv4 ACL",
        )),
        (AclKind::Posix | AclKind::Macos, _) => {
            unreachable!("caller must route AclKind::Posix/Macos separately")
        }
    }
}

#[cfg(target_os = "macos")]
fn split_macos(kind: AclKind, acl: Option<&Acl>) -> Result<Option<&MacosAcl>> {
    match (kind, acl) {
        (AclKind::Macos, None) => Ok(None),
        (AclKind::Macos, Some(Acl::Macos(macos))) => Ok(Some(macos)),
        (AclKind::Macos, Some(_)) => Err(Error::new(
            ErrorKind::InvalidInput,
            "kind: MACOS but value is not a macOS ACL",
        )),
        (AclKind::Posix | AclKind::Nfs4, _) => {
            unreachable!("caller must route AclKind::Posix/Nfs4 separately")
        }
    }
}

impl Direct {
    pub(super) fn acl_from_path(
        path: &Path,
        kind: AclKind,
        default: bool,
        resolve: Resolve,
    ) -> Result<Option<Acl>> {
        let follow = resolve.follows();
        check_default(kind, default)?;
        #[cfg(target_os = "linux")]
        return linux::get(path, kind, default, follow);
        #[cfg(target_os = "freebsd")]
        return freebsd::get(path, kind, default, follow);
        #[cfg(target_os = "macos")]
        return macos::get(path, kind, follow);
    }

    pub(super) fn set_acl_path(
        path: &Path,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
        resolve: Resolve,
    ) -> Result<()> {
        let follow = resolve.follows();
        check_default(kind, default)?;
        #[cfg(target_os = "linux")]
        return linux::set(path, kind, acl, default, follow);
        #[cfg(target_os = "freebsd")]
        return freebsd::set(path, kind, acl, default, follow);
        #[cfg(target_os = "macos")]
        return macos::set(path, kind, acl, follow);
    }

    pub(super) fn acl_from_file(file: &File, kind: AclKind, default: bool) -> Result<Option<Acl>> {
        check_default(kind, default)?;
        #[cfg(target_os = "linux")]
        return linux::get_fd(file, kind, default);
        #[cfg(target_os = "freebsd")]
        return freebsd::get_fd(file, kind, default);
        #[cfg(target_os = "macos")]
        return macos::get_fd(file, kind);
    }

    pub(super) fn set_acl_file(
        file: &File,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
    ) -> Result<()> {
        check_default(kind, default)?;
        #[cfg(target_os = "linux")]
        return linux::set_fd(file, kind, acl, default);
        #[cfg(target_os = "freebsd")]
        return freebsd::set_fd(file, kind, acl, default);
        #[cfg(target_os = "macos")]
        return macos::set_fd(file, kind, acl);
    }

    pub(super) fn impl_resolve_principal_id(
        input: crate::security::PrincipalId,
        want: crate::security::PrincipalIdKind,
    ) -> Result<crate::security::PrincipalId> {
        #[cfg(target_os = "macos")]
        return macos::resolve_principal_id(input, want);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (input, want);
            Err(Error::new(
                ErrorKind::Unsupported,
                "principal ID resolution is not supported on this platform",
            ))
        }
    }
}
