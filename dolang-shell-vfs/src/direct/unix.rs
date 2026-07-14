use super::{Direct, DirectChild, DirectCommand};
use crate::{
    Attrs, ChownIdentity, FsMetadata, FsMetadataFamily, StreamEntry, UnixFsMetadata,
    UnixFsMetadataPlatform, XattrEntry, XattrNamespace,
};
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
    fn attrs_from_flags(flags: libc::c_long) -> Attrs {
        Attrs {
            compressed: Some(flags & linux_attrs::COMPR != 0),
            immutable: Some(flags & linux_attrs::IMMUTABLE != 0),
            append_only: Some(flags & linux_attrs::APPEND != 0),
            no_dump: Some(flags & linux_attrs::NODUMP != 0),
            no_atime: Some(flags & linux_attrs::NOATIME != 0),
            no_copy_on_write: Some(flags & linux_attrs::NOCOW != 0),
            dir_sync: Some(flags & linux_attrs::DIRSYNC != 0),
            casefold: Some(flags & linux_attrs::CASEFOLD != 0),
            data_journaling: Some(flags & linux_attrs::JOURNAL_DATA != 0),
            no_compress: Some(flags & linux_attrs::NOCOMP != 0),
            project_inherit: Some(flags & linux_attrs::PROJINHERIT != 0),
            secure_delete: Some(flags & linux_attrs::SECRM != 0),
            sync: Some(flags & linux_attrs::SYNC != 0),
            no_tail_merge: Some(flags & linux_attrs::NOTAIL != 0),
            top_dir: Some(flags & linux_attrs::TOPDIR != 0),
            undelete: Some(flags & linux_attrs::UNRM != 0),
            direct_access: Some(flags & linux_attrs::DAX != 0),
            extent_format: Some(flags & linux_attrs::EXTENT != 0),
            unix_flags: u32::try_from(flags).ok(),
            ..Attrs::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_linux_flag(flags: &mut libc::c_long, flag: libc::c_long, value: Option<bool>) {
        match value {
            Some(true) => *flags |= flag,
            Some(false) => *flags &= !flag,
            None => {}
        }
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
    pub(super) fn attrs_from_path(path: PathBuf, _follow: bool) -> io::Result<Attrs> {
        let file = std::fs::File::open(path)?;
        unsafe { Self::get_linux_flags(file.as_raw_fd()) }.map(Self::attrs_from_flags)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: Attrs) -> io::Result<()> {
        if patch.readonly.is_some()
            || patch.hidden.is_some()
            || patch.system.is_some()
            || patch.archive.is_some()
            || patch.reparse_point.is_some()
            || patch.encrypted.is_some()
            || patch.temporary.is_some()
            || patch.offline.is_some()
            || patch.not_content_indexed.is_some()
            || patch.opaque.is_some()
            || patch.win_attrs.is_some()
            || patch.unix_flags.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ));
        }

        if patch.is_empty_patch() {
            return Ok(());
        }

        let file = std::fs::OpenOptions::new().read(true).open(path)?;
        let mut flags = unsafe { Self::get_linux_flags(file.as_raw_fd()) }?;
        Self::apply_linux_flag(&mut flags, linux_attrs::COMPR, patch.compressed);
        Self::apply_linux_flag(&mut flags, linux_attrs::IMMUTABLE, patch.immutable);
        Self::apply_linux_flag(&mut flags, linux_attrs::APPEND, patch.append_only);
        Self::apply_linux_flag(&mut flags, linux_attrs::NODUMP, patch.no_dump);
        Self::apply_linux_flag(&mut flags, linux_attrs::NOATIME, patch.no_atime);
        Self::apply_linux_flag(&mut flags, linux_attrs::NOCOW, patch.no_copy_on_write);
        Self::apply_linux_flag(&mut flags, linux_attrs::DIRSYNC, patch.dir_sync);
        Self::apply_linux_flag(&mut flags, linux_attrs::CASEFOLD, patch.casefold);
        Self::apply_linux_flag(&mut flags, linux_attrs::JOURNAL_DATA, patch.data_journaling);
        Self::apply_linux_flag(&mut flags, linux_attrs::NOCOMP, patch.no_compress);
        Self::apply_linux_flag(&mut flags, linux_attrs::PROJINHERIT, patch.project_inherit);
        Self::apply_linux_flag(&mut flags, linux_attrs::SECRM, patch.secure_delete);
        Self::apply_linux_flag(&mut flags, linux_attrs::SYNC, patch.sync);
        Self::apply_linux_flag(&mut flags, linux_attrs::NOTAIL, patch.no_tail_merge);
        Self::apply_linux_flag(&mut flags, linux_attrs::TOPDIR, patch.top_dir);
        Self::apply_linux_flag(&mut flags, linux_attrs::UNRM, patch.undelete);
        Self::apply_linux_flag(&mut flags, linux_attrs::DAX, patch.direct_access);
        Self::apply_linux_flag(&mut flags, linux_attrs::EXTENT, patch.extent_format);
        unsafe { Self::set_linux_flags(file.as_raw_fd(), flags) }
    }

    #[cfg(target_os = "macos")]
    fn attrs_from_flags(flags: libc::c_uint) -> Attrs {
        use nix::sys::stat::FileFlag;

        let flags = FileFlag::from_bits_truncate(flags);
        Attrs {
            hidden: Some(flags.contains(FileFlag::UF_HIDDEN)),
            compressed: Some(flags.contains(FileFlag::UF_COMPRESSED)),
            immutable: Some(flags.contains(FileFlag::UF_IMMUTABLE)),
            append_only: Some(flags.contains(FileFlag::UF_APPEND)),
            no_dump: Some(flags.contains(FileFlag::UF_NODUMP)),
            opaque: Some(flags.contains(FileFlag::UF_OPAQUE)),
            unix_flags: Some(flags.bits()),
            ..Attrs::default()
        }
    }

    #[cfg(target_os = "macos")]
    fn apply_macos_flag(
        flags: &mut nix::sys::stat::FileFlag,
        flag: nix::sys::stat::FileFlag,
        value: Option<bool>,
    ) {
        match value {
            Some(true) => flags.insert(flag),
            Some(false) => flags.remove(flag),
            None => {}
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn attrs_from_path(path: PathBuf, follow: bool) -> io::Result<Attrs> {
        let stat = if follow {
            nix::sys::stat::stat(&path)
        } else {
            nix::sys::stat::lstat(&path)
        }
        .map_err(io::Error::from)?;
        Ok(Self::attrs_from_flags(stat.st_flags))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: Attrs) -> io::Result<()> {
        use nix::sys::stat::FileFlag;

        if patch.readonly.is_some()
            || patch.system.is_some()
            || patch.archive.is_some()
            || patch.reparse_point.is_some()
            || patch.compressed.is_some()
            || patch.encrypted.is_some()
            || patch.temporary.is_some()
            || patch.offline.is_some()
            || patch.not_content_indexed.is_some()
            || patch.no_atime.is_some()
            || patch.no_copy_on_write.is_some()
            || patch.dir_sync.is_some()
            || patch.casefold.is_some()
            || patch.data_journaling.is_some()
            || patch.no_compress.is_some()
            || patch.project_inherit.is_some()
            || patch.secure_delete.is_some()
            || patch.sync.is_some()
            || patch.no_tail_merge.is_some()
            || patch.top_dir.is_some()
            || patch.undelete.is_some()
            || patch.direct_access.is_some()
            || patch.extent_format.is_some()
            || patch.win_attrs.is_some()
            || patch.unix_flags.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ));
        }

        if patch.is_empty_patch() {
            return Ok(());
        }

        let stat = nix::sys::stat::stat(&path).map_err(io::Error::from)?;
        let mut flags = FileFlag::from_bits_truncate(stat.st_flags);
        Self::apply_macos_flag(&mut flags, FileFlag::UF_HIDDEN, patch.hidden);
        Self::apply_macos_flag(&mut flags, FileFlag::UF_IMMUTABLE, patch.immutable);
        Self::apply_macos_flag(&mut flags, FileFlag::UF_APPEND, patch.append_only);
        Self::apply_macos_flag(&mut flags, FileFlag::UF_NODUMP, patch.no_dump);
        Self::apply_macos_flag(&mut flags, FileFlag::UF_OPAQUE, patch.opaque);
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
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self::home_dir_platform(env)?.join("Library").join("Caches"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            if let Some(cache) = Self::absolute_env_path(env, "XDG_CACHE_HOME")? {
                Ok(cache)
            } else {
                Ok(Self::home_dir_platform(env)?.join(".cache"))
            }
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
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> io::Result<()> {
        use nix::{
            errno::Errno,
            unistd::{Gid, Group, Uid, User, chown},
        };
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        fn resolve_user(user: Option<ChownIdentity>) -> Result<Option<Uid>, Errno> {
            match user {
                None => Ok(None),
                Some(ChownIdentity::Id(id)) => Ok(Some(Uid::from_raw(id))),
                Some(ChownIdentity::Name(name)) => match User::from_name(&name)? {
                    Some(user) => Ok(Some(user.uid)),
                    None => Err(Errno::ENOENT),
                },
            }
        }

        fn resolve_group(group: Option<ChownIdentity>) -> Result<Option<Gid>, Errno> {
            match group {
                None => Ok(None),
                Some(ChownIdentity::Id(id)) => Ok(Some(Gid::from_raw(id))),
                Some(ChownIdentity::Name(name)) => match Group::from_name(&name)? {
                    Some(group) => Ok(Some(group.gid)),
                    None => Err(Errno::ENOENT),
                },
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
            let user =
                resolve_user(user).map_err(|err| io::Error::from_raw_os_error(err as i32))?;
            let group =
                resolve_group(group).map_err(|err| io::Error::from_raw_os_error(err as i32))?;
            let result = if follow {
                chown(&path, user, group)
            } else {
                lchown_path(&path, user, group)
            };
            result.map_err(|err| io::Error::from_raw_os_error(err as i32))
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
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

    pub(super) async fn impl_attrs(&self, path: &Path, follow: bool) -> Result<Attrs, io::Error> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::attrs_from_path(path, follow))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join attrs query task")))
    }

    pub(super) async fn impl_set_attrs(&self, path: &Path, attrs: Attrs) -> Result<(), io::Error> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::set_attrs_path(path, attrs))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join attrs update task")))
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

    pub(super) async fn impl_set_times(
        &self,
        path: &Path,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        use nix::{
            fcntl::AT_FDCWD,
            sys::{
                stat::{UtimensatFlags, utimensat},
                time::{TimeSpec, TimeValLike},
            },
        };

        fn unix_timespec(time: Option<(i64, u32)>) -> io::Result<TimeSpec> {
            match time {
                Some((secs, nanos)) => secs
                    .checked_mul(1_000_000_000)
                    .and_then(|secs| secs.checked_add(i64::from(nanos)))
                    .map(TimeSpec::nanoseconds)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid timestamp")
                    }),
                None => Ok(TimeSpec::UTIME_OMIT),
            }
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
            utimensat(
                AT_FDCWD,
                &path,
                &atime,
                &mtime,
                UtimensatFlags::FollowSymlink,
            )
            .map_err(io::Error::from)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_chown(
        &self,
        path: &Path,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
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
