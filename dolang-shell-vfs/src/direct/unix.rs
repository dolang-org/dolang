use super::{Direct, DirectChild, DirectCommand};
use crate::{
    AttrFlags, AttrsPatch, FsMetadata, FsMetadataFamily, Metadata, MetadataPatch,
    OwnershipIdentity, SecDesc, StreamEntry, UnixFsMetadata, UnixFsMetadataPlatform, XattrEntry,
    XattrNamespace,
};
#[cfg(target_os = "linux")]
use crate::{MetadataFamily, UnixMetadata, UnixMetadataPlatform};
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;
use std::{
    collections::HashMap,
    ffi::{CStr, CString, OsString},
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};
use tokio::fs::{self, File, OpenOptions};
use tokio::time::{Duration, timeout};

#[cfg(target_os = "linux")]
mod linux_attrs {
    pub(super) const SECRM: libc::c_long = 0x0000_0001;
    pub(super) const UNRM: libc::c_long = 0x0000_0002;
    pub(super) const COMPR: libc::c_long = 0x0000_0004;
    pub(super) const SYNC: libc::c_long = 0x0000_0008;
    pub(super) const IMMUTABLE: libc::c_long = 0x0000_0010;
    pub(super) const APPEND: libc::c_long = 0x0000_0020;
    pub(super) const NODUMP: libc::c_long = 0x0000_0040;
    pub(super) const NOATIME: libc::c_long = 0x0000_0080;
    pub(super) const NOCOMP: libc::c_long = 0x0000_0400;
    pub(super) const JOURNAL_DATA: libc::c_long = 0x0000_4000;
    pub(super) const NOTAIL: libc::c_long = 0x0000_8000;
    pub(super) const DIRSYNC: libc::c_long = 0x0001_0000;
    pub(super) const TOPDIR: libc::c_long = 0x0002_0000;
    pub(super) const EXTENT: libc::c_long = 0x0008_0000;
    pub(super) const NOCOW: libc::c_long = 0x0080_0000;
    pub(super) const DAX: libc::c_long = 0x0200_0000;
    pub(super) const PROJINHERIT: libc::c_long = 0x2000_0000;
    pub(super) const CASEFOLD: libc::c_long = 0x4000_0000;
}

pub(super) enum UnixXattrTarget<'a> {
    Fd(BorrowedFd<'a>),
    Path(&'a CStr, bool),
}

impl Direct {
    pub(super) fn sec_desc_from_path(
        _path: &Path,
        _mask: u32,
        _follow: bool,
    ) -> io::Result<SecDesc> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) fn set_sec_desc_path(
        _path: &Path,
        _descriptor: &SecDesc,
        _follow: bool,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) fn sec_desc_from_file(_file: &File, _mask: u32) -> io::Result<SecDesc> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) fn set_sec_desc_file(_file: &File, _descriptor: &SecDesc) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) async fn impl_user_name(&self, uid: u32) -> crate::Result<String> {
        tokio::task::spawn_blocking(move || {
            nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                .map_err(io::Error::from)?
                .map(|user| user.name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user ID not found"))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("user lookup task failed")))
        .map_err(Into::into)
    }

    pub(super) async fn impl_user_id(&self, name: &str) -> crate::Result<u32> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            nix::unistd::User::from_name(&name)
                .map_err(io::Error::from)?
                .map(|user| user.uid.as_raw())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user name not found"))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("user lookup task failed")))
        .map_err(Into::into)
    }

    pub(super) async fn impl_group_name(&self, gid: u32) -> crate::Result<String> {
        tokio::task::spawn_blocking(move || {
            nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
                .map_err(io::Error::from)?
                .map(|group| group.name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "group ID not found"))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("group lookup task failed")))
        .map_err(Into::into)
    }

    pub(super) async fn impl_group_id(&self, name: &str) -> crate::Result<u32> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            nix::unistd::Group::from_name(&name)
                .map_err(io::Error::from)?
                .map(|group| group.gid.as_raw())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "group name not found"))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("group lookup task failed")))
        .map_err(Into::into)
    }

    pub(super) fn program_not_found_error() -> io::Error {
        io::Error::from_raw_os_error(libc::ENOENT)
    }

    pub(super) fn directory_requires_all_error() -> io::Error {
        io::Error::from_raw_os_error(libc::EISDIR)
    }

    pub(super) fn directory_not_empty_error() -> io::Error {
        io::Error::from_raw_os_error(libc::ENOTEMPTY)
    }

    pub(super) fn not_a_directory_error() -> io::Error {
        io::Error::from_raw_os_error(libc::ENOTDIR)
    }

    fn statvfs_from_fd(fd: BorrowedFd<'_>) -> io::Result<libc::statvfs> {
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::fstatvfs(fd.as_raw_fd(), stat.as_mut_ptr()) };
        if rc == 0 {
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn statvfs_from_path(path: &Path) -> io::Result<libc::statvfs> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
        if rc == 0 {
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[allow(clippy::useless_conversion)]
    fn fs_metadata_from_statvfs(stat: libc::statvfs) -> FsMetadata {
        let unit_size = if stat.f_frsize != 0 {
            u64::from(stat.f_frsize)
        } else {
            u64::from(stat.f_bsize)
        };
        FsMetadata {
            capacity: u64::from(stat.f_blocks).saturating_mul(unit_size),
            free: u64::from(stat.f_bfree).saturating_mul(unit_size),
            available: u64::from(stat.f_bavail).saturating_mul(unit_size),
            block_size: u32::try_from(stat.f_bsize).unwrap_or(u32::MAX),
            family: FsMetadataFamily::Unix(UnixFsMetadata {
                blocks: stat.f_blocks.into(),
                blocks_free: stat.f_bfree.into(),
                blocks_available: stat.f_bavail.into(),
                files: stat.f_files.into(),
                files_free: stat.f_ffree.into(),
                files_available: stat.f_favail.into(),
                fragment_size: u32::try_from(stat.f_frsize).unwrap_or(u32::MAX),
                #[cfg(target_os = "linux")]
                fsid: Some(stat.f_fsid),
                #[cfg(not(target_os = "linux"))]
                fsid: None,
                name_max: u32::try_from(stat.f_namemax).unwrap_or(u32::MAX),
                #[cfg(target_os = "linux")]
                platform: UnixFsMetadataPlatform::Linux {
                    flags: stat.f_flag.into(),
                },
                #[cfg(target_os = "macos")]
                platform: UnixFsMetadataPlatform::Macos {
                    flags: stat.f_flag.into(),
                },
            }),
        }
    }

    pub(super) fn fs_metadata_from_file(file: &File) -> io::Result<FsMetadata> {
        Self::statvfs_from_fd(file.as_fd()).map(Self::fs_metadata_from_statvfs)
    }

    pub(super) fn fs_metadata_from_path(path: &Path, follow: bool) -> io::Result<FsMetadata> {
        if !follow {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fs_metadata follow: false is not implemented on this platform",
            ));
        }
        Self::statvfs_from_path(path).map(Self::fs_metadata_from_statvfs)
    }

    #[cfg(target_os = "linux")]
    fn attrs_from_flags(flags: libc::c_long) -> u32 {
        u32::try_from(flags).unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    unsafe fn get_linux_flags(fd: RawFd) -> io::Result<libc::c_long> {
        nix::ioctl_read!(fs_ioc_getflags, b'f', 1, libc::c_long);

        let mut flags = 0;
        unsafe { fs_ioc_getflags(fd, &mut flags) }.map_err(io::Error::from)?;
        Ok(flags)
    }

    #[cfg(target_os = "linux")]
    unsafe fn set_linux_flags(fd: RawFd, flags: libc::c_long) -> io::Result<()> {
        nix::ioctl_write_ptr!(fs_ioc_setflags, b'f', 2, libc::c_long);

        unsafe { fs_ioc_setflags(fd, &flags) }.map_err(io::Error::from)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn attrs_from_path(path: PathBuf, _follow: bool) -> io::Result<u32> {
        let file = std::fs::File::open(path)?;
        unsafe { Self::get_linux_flags(file.as_raw_fd()) }.map(Self::attrs_from_flags)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn metadata_with_attrs(
        metadata: std::fs::Metadata,
        file: &File,
    ) -> io::Result<Metadata> {
        let mut metadata = crate::metadata_from_std(metadata);
        if !matches!(
            metadata.file_type,
            crate::FileType::File | crate::FileType::Dir
        ) {
            return Ok(metadata);
        }
        let attrs = match unsafe { Self::get_linux_flags(file.as_raw_fd()) } {
            Ok(flags) => Some(Self::attrs_from_flags(flags)),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL | libc::ENXIO)
                ) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let MetadataFamily::Unix(UnixMetadata {
            platform: UnixMetadataPlatform::Linux { attrs: value },
            ..
        }) = &mut metadata.family
        else {
            unreachable!();
        };
        *value = attrs;
        Ok(metadata)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn metadata_from_path(path: &Path, follow: bool) -> io::Result<Metadata> {
        let std_metadata = if follow {
            std::fs::metadata(path)?
        } else {
            std::fs::symlink_metadata(path)?
        };
        let mut metadata = crate::metadata_from_std(std_metadata);
        if !follow
            || !matches!(
                metadata.file_type,
                crate::FileType::File | crate::FileType::Dir
            )
        {
            return Ok(metadata);
        }
        let attrs = match Self::attrs_from_path(path.to_path_buf(), true) {
            Ok(attrs) => Some(attrs),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL | libc::ENXIO)
                ) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let MetadataFamily::Unix(UnixMetadata {
            platform: UnixMetadataPlatform::Linux { attrs: value },
            ..
        }) = &mut metadata.family
        else {
            unreachable!();
        };
        *value = attrs;
        Ok(metadata)
    }

    #[cfg(target_os = "linux")]
    fn validate_attrs_patch(patch: AttrsPatch) -> io::Result<()> {
        let supported = AttrFlags::COMPRESSED
            .union(AttrFlags::IMMUTABLE)
            .union(AttrFlags::APPEND_ONLY)
            .union(AttrFlags::NO_DUMP)
            .union(AttrFlags::NO_ATIME)
            .union(AttrFlags::NO_COPY_ON_WRITE)
            .union(AttrFlags::DIR_SYNC)
            .union(AttrFlags::CASEFOLD)
            .union(AttrFlags::DATA_JOURNALING)
            .union(AttrFlags::NO_COMPRESS)
            .union(AttrFlags::PROJECT_INHERIT)
            .union(AttrFlags::SECURE_DELETE)
            .union(AttrFlags::SYNC)
            .union(AttrFlags::NO_TAIL_MERGE)
            .union(AttrFlags::TOP_DIR)
            .union(AttrFlags::UNDELETE)
            .union(AttrFlags::DIRECT_ACCESS)
            .union(AttrFlags::EXTENT_FORMAT);
        if !patch.requested().difference(supported).is_empty() {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: AttrsPatch) -> io::Result<()> {
        Self::validate_attrs_patch(patch)?;

        if patch.is_empty() {
            return Ok(());
        }

        let file = std::fs::OpenOptions::new().read(true).open(path)?;
        let mut flags = unsafe { Self::get_linux_flags(file.as_raw_fd()) }?;
        for (semantic, native) in [
            (AttrFlags::COMPRESSED, linux_attrs::COMPR),
            (AttrFlags::IMMUTABLE, linux_attrs::IMMUTABLE),
            (AttrFlags::APPEND_ONLY, linux_attrs::APPEND),
            (AttrFlags::NO_DUMP, linux_attrs::NODUMP),
            (AttrFlags::NO_ATIME, linux_attrs::NOATIME),
            (AttrFlags::NO_COPY_ON_WRITE, linux_attrs::NOCOW),
            (AttrFlags::DIR_SYNC, linux_attrs::DIRSYNC),
            (AttrFlags::CASEFOLD, linux_attrs::CASEFOLD),
            (AttrFlags::DATA_JOURNALING, linux_attrs::JOURNAL_DATA),
            (AttrFlags::NO_COMPRESS, linux_attrs::NOCOMP),
            (AttrFlags::PROJECT_INHERIT, linux_attrs::PROJINHERIT),
            (AttrFlags::SECURE_DELETE, linux_attrs::SECRM),
            (AttrFlags::SYNC, linux_attrs::SYNC),
            (AttrFlags::NO_TAIL_MERGE, linux_attrs::NOTAIL),
            (AttrFlags::TOP_DIR, linux_attrs::TOPDIR),
            (AttrFlags::UNDELETE, linux_attrs::UNRM),
            (AttrFlags::DIRECT_ACCESS, linux_attrs::DAX),
            (AttrFlags::EXTENT_FORMAT, linux_attrs::EXTENT),
        ] {
            if patch.set.contains(semantic) {
                flags |= native;
            } else if patch.clear.contains(semantic) {
                flags &= !native;
            }
        }
        unsafe { Self::set_linux_flags(file.as_raw_fd(), flags) }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn metadata_with_attrs(
        metadata: std::fs::Metadata,
        _file: &File,
    ) -> io::Result<Metadata> {
        Ok(crate::metadata_from_std(metadata))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn metadata_from_path(path: &Path, follow: bool) -> io::Result<Metadata> {
        let metadata = if follow {
            std::fs::metadata(path)?
        } else {
            std::fs::symlink_metadata(path)?
        };
        Ok(crate::metadata_from_std(metadata))
    }

    #[cfg(target_os = "macos")]
    fn validate_attrs_patch(patch: AttrsPatch) -> io::Result<()> {
        let supported = AttrFlags::HIDDEN
            .union(AttrFlags::IMMUTABLE)
            .union(AttrFlags::APPEND_ONLY)
            .union(AttrFlags::NO_DUMP)
            .union(AttrFlags::OPAQUE);
        if !patch.requested().difference(supported).is_empty() {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: AttrsPatch) -> io::Result<()> {
        use nix::sys::stat::FileFlag;

        Self::validate_attrs_patch(patch)?;

        if patch.is_empty() {
            return Ok(());
        }

        let stat = nix::sys::stat::stat(&path).map_err(io::Error::from)?;
        let mut flags = FileFlag::from_bits_truncate(stat.st_flags);
        for (semantic, native) in [
            (AttrFlags::HIDDEN, FileFlag::UF_HIDDEN),
            (AttrFlags::IMMUTABLE, FileFlag::UF_IMMUTABLE),
            (AttrFlags::APPEND_ONLY, FileFlag::UF_APPEND),
            (AttrFlags::NO_DUMP, FileFlag::UF_NODUMP),
            (AttrFlags::OPAQUE, FileFlag::UF_OPAQUE),
        ] {
            if patch.set.contains(semantic) {
                flags.insert(native);
            } else if patch.clear.contains(semantic) {
                flags.remove(native);
            }
        }
        nix::unistd::chflags(&path, flags).map_err(io::Error::from)
    }

    fn override_or_env(env: &HashMap<String, Option<String>>, key: &str) -> Option<OsString> {
        match env.get(key) {
            Some(Some(value)) => Some(OsString::from(value)),
            Some(None) => None,
            None => std::env::var_os(key),
        }
    }

    pub(super) fn absolute_env_path(
        env: &HashMap<String, Option<String>>,
        key: &str,
    ) -> Result<Option<PathBuf>, io::Error> {
        match Self::override_or_env(env, key) {
            Some(value) => {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    Ok(Some(path))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{key} must be an absolute path"),
                    ))
                }
            }
            None => Ok(None),
        }
    }

    pub(super) fn home_dir_platform(
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        if let Some(home) = Self::absolute_env_path(env, "HOME")? {
            return Ok(home);
        }

        let uid = nix::unistd::getuid();
        let user = nix::unistd::User::from_uid(uid)
            .map_err(io::Error::other)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "could not resolve home directory")
            })?;
        let home = user.dir;
        if home.is_absolute() {
            Ok(home)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resolved home directory is not absolute",
            ))
        }
    }

    pub(super) fn cache_dir_platform(
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        #[cfg(target_os = "macos")]
        {
            let path = Self::home_dir_platform(env)?.join("Library").join("Caches");
            Ok(match app {
                Some(app) => path.join(app),
                None => path,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let base = if let Some(cache) = Self::absolute_env_path(env, "XDG_CACHE_HOME")? {
                cache
            } else {
                Self::home_dir_platform(env)?.join(".cache")
            };
            Ok(match app {
                Some(app) => base.join(app),
                None => base,
            })
        }
    }

    pub(super) fn temp_dir_platform(
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        Ok(Self::absolute_env_path(env, "TMPDIR")?.unwrap_or_else(|| PathBuf::from("/tmp")))
    }

    pub(super) fn unix_xattr_namespace(
        namespace: XattrNamespace<'_>,
    ) -> io::Result<Option<Vec<u8>>> {
        #[cfg(not(target_os = "linux"))]
        {
            if !matches!(namespace, XattrNamespace::Default) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "xattr namespaces not supported on this platform",
                ));
            }
            Ok(None)
        }
        #[cfg(target_os = "linux")]
        {
            Ok(match namespace {
                XattrNamespace::Default => Some(b"user.".to_vec()),
                XattrNamespace::Named(namespace) => Some(format!("{namespace}.").into_bytes()),
                XattrNamespace::Any => None,
            })
        }
    }

    pub(super) fn xattr_path(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    }

    pub(super) fn xattr_name(name: &str, namespace: Option<&str>) -> io::Result<CString> {
        #[cfg(target_os = "linux")]
        let full_name = match namespace {
            Some(namespace) => format!("{namespace}.{name}"),
            None => format!("user.{name}"),
        };
        #[cfg(not(target_os = "linux"))]
        let full_name = match namespace {
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "xattr namespaces are not supported on this platform",
                ));
            }
            None => name.to_owned(),
        };
        CString::new(full_name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "xattr name contains NUL"))
    }

    fn xattr_entry(raw_name: Vec<u8>) -> io::Result<XattrEntry> {
        let name = String::from_utf8(raw_name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "xattr name is not UTF-8"))?;
        #[cfg(target_os = "linux")]
        {
            let (namespace, name) = name
                .split_once('.')
                .map(|(namespace, name)| (Some(namespace.to_owned()), name.to_owned()))
                .unwrap_or_else(|| (None, name.clone()));
            Ok(XattrEntry {
                name,
                namespace,
                size: None,
                flags: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(XattrEntry {
                name,
                namespace: None,
                size: None,
                flags: None,
            })
        }
    }

    pub(super) fn unix_list_xattrs(
        target: UnixXattrTarget<'_>,
        namespace: Option<Vec<u8>>,
    ) -> io::Result<Vec<XattrEntry>> {
        #[cfg(not(target_os = "macos"))]
        let mut size = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => {
                    libc::flistxattr(fd.as_raw_fd(), std::ptr::null_mut(), 0)
                }
                UnixXattrTarget::Path(path, true) => {
                    libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0)
                }
                UnixXattrTarget::Path(path, false) => {
                    libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0)
                }
            }
        };
        #[cfg(target_os = "macos")]
        let mut size = unsafe {
            debug_assert!(namespace.is_none());
            let _ = namespace;
            match target {
                UnixXattrTarget::Fd(fd) => {
                    libc::flistxattr(fd.as_raw_fd(), std::ptr::null_mut(), 0, 0)
                }
                UnixXattrTarget::Path(path, follow) => libc::listxattr(
                    path.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    if follow { 0 } else { libc::XATTR_NOFOLLOW },
                ),
            }
        };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }

        loop {
            let mut buf = vec![0u8; size as usize];
            #[cfg(not(target_os = "macos"))]
            let read = unsafe {
                match target {
                    UnixXattrTarget::Fd(fd) => {
                        libc::flistxattr(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                    }
                    UnixXattrTarget::Path(path, true) => {
                        libc::listxattr(path.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
                    }
                    UnixXattrTarget::Path(path, false) => {
                        libc::llistxattr(path.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
                    }
                }
            };
            #[cfg(target_os = "macos")]
            let read = unsafe {
                match target {
                    UnixXattrTarget::Fd(fd) => {
                        libc::flistxattr(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0)
                    }
                    UnixXattrTarget::Path(path, follow) => libc::listxattr(
                        path.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        if follow { 0 } else { libc::XATTR_NOFOLLOW },
                    ),
                }
            };
            if read < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ERANGE) {
                    #[cfg(not(target_os = "macos"))]
                    {
                        size = unsafe {
                            match target {
                                UnixXattrTarget::Fd(fd) => {
                                    libc::flistxattr(fd.as_raw_fd(), std::ptr::null_mut(), 0)
                                }
                                UnixXattrTarget::Path(path, true) => {
                                    libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0)
                                }
                                UnixXattrTarget::Path(path, false) => {
                                    libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0)
                                }
                            }
                        };
                    }
                    #[cfg(target_os = "macos")]
                    {
                        size = unsafe {
                            match target {
                                UnixXattrTarget::Fd(fd) => {
                                    libc::flistxattr(fd.as_raw_fd(), std::ptr::null_mut(), 0, 0)
                                }
                                UnixXattrTarget::Path(path, follow) => libc::listxattr(
                                    path.as_ptr(),
                                    std::ptr::null_mut(),
                                    0,
                                    if follow { 0 } else { libc::XATTR_NOFOLLOW },
                                ),
                            }
                        };
                    }
                    if size < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    continue;
                }
                return Err(err);
            }
            buf.truncate(read as usize);
            return buf
                .split(|byte| *byte == 0)
                .filter(|name| {
                    if name.is_empty() {
                        return false;
                    }
                    #[cfg(target_os = "linux")]
                    {
                        namespace.as_ref().is_none_or(|ns| name.starts_with(ns))
                    }
                    #[cfg(not(target_os = "linux"))]
                    true
                })
                .map(|name| Direct::xattr_entry(name.to_vec()))
                .collect();
        }
    }

    pub(super) fn unix_get_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> io::Result<Vec<u8>> {
        #[cfg(not(target_os = "macos"))]
        let mut size = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => {
                    libc::fgetxattr(fd.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0)
                }
                UnixXattrTarget::Path(path, true) => {
                    libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
                }
                UnixXattrTarget::Path(path, false) => {
                    libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
                }
            }
        };
        #[cfg(target_os = "macos")]
        let mut size = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => {
                    libc::fgetxattr(fd.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0)
                }
                UnixXattrTarget::Path(path, follow) => libc::getxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    if follow { 0 } else { libc::XATTR_NOFOLLOW },
                ),
            }
        };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }

        loop {
            let mut buf = vec![0u8; size as usize];
            #[cfg(not(target_os = "macos"))]
            let read = unsafe {
                match target {
                    UnixXattrTarget::Fd(fd) => libc::fgetxattr(
                        fd.as_raw_fd(),
                        name.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    ),
                    UnixXattrTarget::Path(path, true) => libc::getxattr(
                        path.as_ptr(),
                        name.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    ),
                    UnixXattrTarget::Path(path, false) => libc::lgetxattr(
                        path.as_ptr(),
                        name.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    ),
                }
            };
            #[cfg(target_os = "macos")]
            let read = unsafe {
                match target {
                    UnixXattrTarget::Fd(fd) => libc::fgetxattr(
                        fd.as_raw_fd(),
                        name.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        0,
                        0,
                    ),
                    UnixXattrTarget::Path(path, follow) => libc::getxattr(
                        path.as_ptr(),
                        name.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        0,
                        if follow { 0 } else { libc::XATTR_NOFOLLOW },
                    ),
                }
            };
            if read < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ERANGE) {
                    #[cfg(not(target_os = "macos"))]
                    {
                        size = unsafe {
                            match target {
                                UnixXattrTarget::Fd(fd) => libc::fgetxattr(
                                    fd.as_raw_fd(),
                                    name.as_ptr(),
                                    std::ptr::null_mut(),
                                    0,
                                ),
                                UnixXattrTarget::Path(path, true) => libc::getxattr(
                                    path.as_ptr(),
                                    name.as_ptr(),
                                    std::ptr::null_mut(),
                                    0,
                                ),
                                UnixXattrTarget::Path(path, false) => libc::lgetxattr(
                                    path.as_ptr(),
                                    name.as_ptr(),
                                    std::ptr::null_mut(),
                                    0,
                                ),
                            }
                        };
                    }
                    #[cfg(target_os = "macos")]
                    {
                        size = unsafe {
                            match target {
                                UnixXattrTarget::Fd(fd) => libc::fgetxattr(
                                    fd.as_raw_fd(),
                                    name.as_ptr(),
                                    std::ptr::null_mut(),
                                    0,
                                    0,
                                    0,
                                ),
                                UnixXattrTarget::Path(path, follow) => libc::getxattr(
                                    path.as_ptr(),
                                    name.as_ptr(),
                                    std::ptr::null_mut(),
                                    0,
                                    0,
                                    if follow { 0 } else { libc::XATTR_NOFOLLOW },
                                ),
                            }
                        };
                    }
                    if size < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    continue;
                }
                return Err(err);
            }
            buf.truncate(read as usize);
            return Ok(buf);
        }
    }

    pub(super) fn unix_set_xattr(
        target: UnixXattrTarget<'_>,
        name: &CStr,
        value: &[u8],
    ) -> io::Result<()> {
        #[cfg(not(target_os = "macos"))]
        let res = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => libc::fsetxattr(
                    fd.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
                UnixXattrTarget::Path(path, true) => libc::setxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
                UnixXattrTarget::Path(path, false) => libc::lsetxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                ),
            }
        };
        #[cfg(target_os = "macos")]
        let res = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => libc::fsetxattr(
                    fd.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                ),
                UnixXattrTarget::Path(path, follow) => libc::setxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    if follow { 0 } else { libc::XATTR_NOFOLLOW },
                ),
            }
        };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn unix_remove_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> io::Result<()> {
        #[cfg(not(target_os = "macos"))]
        let res = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => libc::fremovexattr(fd.as_raw_fd(), name.as_ptr()),
                UnixXattrTarget::Path(path, true) => {
                    libc::removexattr(path.as_ptr(), name.as_ptr())
                }
                UnixXattrTarget::Path(path, false) => {
                    libc::lremovexattr(path.as_ptr(), name.as_ptr())
                }
            }
        };
        #[cfg(target_os = "macos")]
        let res = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => libc::fremovexattr(fd.as_raw_fd(), name.as_ptr(), 0),
                UnixXattrTarget::Path(path, follow) => libc::removexattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    if follow { 0 } else { libc::XATTR_NOFOLLOW },
                ),
            }
        };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) async fn chown_local(
        path: PathBuf,
        user: Option<OwnershipIdentity>,
        group: Option<OwnershipIdentity>,
        follow: bool,
    ) -> io::Result<()> {
        use nix::{
            errno::Errno,
            unistd::{Gid, Group, Uid, User, chown},
        };
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        fn resolve_user(user: Option<OwnershipIdentity>) -> io::Result<Option<Uid>> {
            match user {
                None => Ok(None),
                Some(OwnershipIdentity::Id(id)) => Ok(Some(Uid::from_raw(id))),
                Some(OwnershipIdentity::Name(name)) => {
                    match User::from_name(&name).map_err(io::Error::from)? {
                        Some(user) => Ok(Some(user.uid)),
                        None => Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("user not found: {name}"),
                        )),
                    }
                }
                Some(OwnershipIdentity::Sid(_)) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SID ownership identities are not supported on Unix",
                )),
            }
        }

        fn resolve_group(group: Option<OwnershipIdentity>) -> io::Result<Option<Gid>> {
            match group {
                None => Ok(None),
                Some(OwnershipIdentity::Id(id)) => Ok(Some(Gid::from_raw(id))),
                Some(OwnershipIdentity::Name(name)) => {
                    match Group::from_name(&name).map_err(io::Error::from)? {
                        Some(group) => Ok(Some(group.gid)),
                        None => Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("group not found: {name}"),
                        )),
                    }
                }
                Some(OwnershipIdentity::Sid(_)) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SID ownership identities are not supported on Unix",
                )),
            }
        }

        fn lchown_path(path: &Path, user: Option<Uid>, group: Option<Gid>) -> Result<(), Errno> {
            let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
            Errno::result(unsafe {
                libc::lchown(
                    path.as_ptr(),
                    user.map_or(!0, |user| user.as_raw()) as libc::uid_t,
                    group.map_or(!0, |group| group.as_raw()) as libc::gid_t,
                )
            })
            .map(drop)
        }

        tokio::task::spawn_blocking(move || {
            let user = resolve_user(user)?;
            let group = resolve_group(group)?;
            let result = if follow {
                chown(&path, user, group)
            } else {
                lchown_path(&path, user, group)
            };
            result.map_err(|err| io::Error::from_raw_os_error(err as i32))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("failed to join ownership update task")))
    }

    pub(super) async fn impl_symlink(_cwd: &Path, src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink(src, dst).await
    }

    pub(super) async fn impl_symlink_dir(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink(src, dst).await
    }

    pub(super) async fn impl_symlink_file(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink(src, dst).await
    }

    pub(super) async fn impl_xattrs(
        &self,
        path: &Path,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        let path = Self::xattr_path(path)?;
        let namespace = Self::unix_xattr_namespace(namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_list_xattrs(UnixXattrTarget::Path(&path, follow), namespace)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_streams(
        &self,
        _path: &Path,
        _follow: bool,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    pub(super) async fn impl_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>, io::Error> {
        let path = Self::xattr_path(path)?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_get_xattr(UnixXattrTarget::Path(&path, follow), &name)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_set_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<(), io::Error> {
        let path = Self::xattr_path(path)?;
        let name = Self::xattr_name(name, namespace)?;
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || {
            Self::unix_set_xattr(UnixXattrTarget::Path(&path, follow), &name, &value)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_remove_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<(), io::Error> {
        let path = Self::xattr_path(path)?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_remove_xattr(UnixXattrTarget::Path(&path, follow), &name)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_xattrs(
        &self,
        file: &File,
        namespace: XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        let file = file.try_clone().await?;
        let namespace = Self::unix_xattr_namespace(namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_list_xattrs(UnixXattrTarget::Fd(file.as_fd()), namespace)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<u8>, io::Error> {
        let file = file.try_clone().await?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_get_xattr(UnixXattrTarget::Fd(file.as_fd()), &name)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_streams(
        &self,
        _file: &File,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    pub(super) async fn impl_file_set_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<(), io::Error> {
        let file = file.try_clone().await?;
        let name = Self::xattr_name(name, namespace)?;
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || {
            Self::unix_set_xattr(UnixXattrTarget::Fd(file.as_fd()), &name, &value)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_remove_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<(), io::Error> {
        let file = file.try_clone().await?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_remove_xattr(UnixXattrTarget::Fd(file.as_fd()), &name)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_set_attrs(
        &self,
        path: &Path,
        attrs: AttrsPatch,
    ) -> Result<(), io::Error> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::set_attrs_path(path, attrs))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join attrs update task")))
    }

    pub(super) async fn impl_set_metadata(
        &self,
        paths: &[PathBuf],
        mut patch: MetadataPatch,
    ) -> Result<(), io::Error> {
        if patch.is_empty() {
            return Ok(());
        }
        if !patch.follow && (patch.mode.is_some() || !patch.attrs.is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "mode and attributes cannot be set without following symlinks on this platform",
            ));
        }
        if patch.created.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "created timestamp is not supported on this platform",
            ));
        }
        Self::validate_attrs_patch(patch.attrs)?;

        if patch.user.is_some() || patch.group.is_some() {
            let user = patch.user;
            let group = patch.group;
            (patch.user, patch.group) = tokio::task::spawn_blocking(move || {
                use nix::unistd::{Group, User};

                let user = match user {
                    Some(OwnershipIdentity::Name(name)) => User::from_name(&name)
                        .map_err(io::Error::from)?
                        .map(|user| Some(OwnershipIdentity::Id(user.uid.as_raw())))
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("user not found: {name}"),
                            )
                        })?,
                    Some(OwnershipIdentity::Sid(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "SID ownership identities are not supported on Unix",
                        ));
                    }
                    value => value,
                };
                let group = match group {
                    Some(OwnershipIdentity::Name(name)) => Group::from_name(&name)
                        .map_err(io::Error::from)?
                        .map(|group| Some(OwnershipIdentity::Id(group.gid.as_raw())))
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("group not found: {name}"),
                            )
                        })?,
                    Some(OwnershipIdentity::Sid(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "SID ownership identities are not supported on Unix",
                        ));
                    }
                    value => value,
                };
                Ok((user, group))
            })
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join ownership lookup task")))?;
        }

        for path in paths {
            if patch.user.is_some() || patch.group.is_some() {
                self.impl_chown(path, patch.user.clone(), patch.group.clone(), patch.follow)
                    .await?;
            }
            if let Some(mode) = patch.mode {
                self.impl_set_permissions(path, crate::Permissions::from_mode(mode))
                    .await?;
            }
            if !patch.attrs.is_empty() {
                self.impl_set_attrs(path, patch.attrs).await?;
            }
            if patch.accessed.is_some() || patch.modified.is_some() {
                self.impl_set_file_times(
                    path,
                    patch.accessed,
                    patch.modified,
                    patch.created,
                    patch.follow,
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn impl_canonicalize(&self, path: &Path) -> Result<PathBuf, io::Error> {
        fs::canonicalize(path).await
    }

    pub(super) async fn impl_set_permissions(
        &self,
        path: &Path,
        perm: crate::Permissions,
    ) -> Result<(), io::Error> {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, std::fs::Permissions::from_mode(perm.mode())).await
    }

    async fn impl_set_file_times(
        &self,
        path: &Path,
        accessed: Option<i128>,
        modified: Option<i128>,
        created: Option<i128>,
        follow: bool,
    ) -> Result<(), io::Error> {
        use nix::{
            fcntl::AT_FDCWD,
            sys::{
                stat::{UtimensatFlags, utimensat},
                time::TimeSpec,
            },
        };

        fn unix_timespec(time: Option<i128>) -> io::Result<TimeSpec> {
            let Some(time) = time else {
                return Ok(TimeSpec::UTIME_OMIT);
            };
            let secs = i64::try_from(time.div_euclid(1_000_000_000))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp"))?;
            let nanos = i64::try_from(time.rem_euclid(1_000_000_000))
                .expect("nanosecond remainder is in i64 range");
            Ok(TimeSpec::new(secs, nanos))
        }

        if created.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "created timestamp is not supported on this platform",
            ));
        }

        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let atime = unix_timespec(accessed)?;
            let mtime = unix_timespec(modified)?;
            let flags = if follow {
                UtimensatFlags::FollowSymlink
            } else {
                UtimensatFlags::NoFollowSymlink
            };
            utimensat(AT_FDCWD, &path, &atime, &mtime, flags).map_err(io::Error::from)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_chown(
        &self,
        path: &Path,
        user: Option<OwnershipIdentity>,
        group: Option<OwnershipIdentity>,
        follow: bool,
    ) -> Result<(), io::Error> {
        Self::chown_local(path.to_path_buf(), user, group, follow).await
    }
}

impl DirectChild {
    pub(super) async fn impl_terminate(self) -> io::Result<std::process::ExitStatus> {
        let mut child = self.inner;
        let Some(pid) = child.id() else {
            return child.wait().await;
        };
        let res = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if res != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(err);
            }
        }
        match timeout(Duration::from_millis(500), child.wait()).await {
            Ok(result) => result,
            Err(_) => {
                let _ = child.start_kill();
                child.wait().await
            }
        }
    }
}

impl DirectCommand<'_> {
    pub(super) fn impl_stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.stderr = Some(std::process::Stdio::from(
            std::io::stdout().as_fd().try_clone_to_owned()?,
        ));
        Ok(self)
    }
}

impl super::DirectOpenOptions {
    pub(super) fn apply_no_follow_flags(&self, opts: &mut OpenOptions) {
        if self.no_follow {
            opts.custom_flags(libc::O_NOFOLLOW);
        }
    }
}
