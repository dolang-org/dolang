use super::{Child, Command, Direct, File};
#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
use crate::metadata::{AttrFlags, AttrsPatch, Metadata};
#[cfg(target_os = "linux")]
use crate::metadata::{MetadataFamily, UnixMetadata, UnixMetadataPlatform};
use crate::{
    error::{Error, ErrorKind, Result},
    file::FileId,
    file::StreamEntry,
    file::{XattrEntry, XattrNamespace},
    metadata::{
        FileType, FsMetadata, FsMetadataFamily, MetadataPatch, Mode, UnixFsMetadata,
        UnixFsMetadataPlatform,
    },
    process::ProcessControl,
    security::OwnershipIdentity,
};
use dolang_winterop::security::SecDesc;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;
use std::{
    collections::HashMap,
    ffi::{CStr, CString, OsString},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{self, OpenOptions},
    time::{Duration, timeout},
};

use nix::{
    dir::{Dir as NixDir, OwningIter, Type},
    fcntl::OFlag,
    sys::stat::Mode as NixMode,
};

#[derive(Debug)]
pub(crate) struct ReadDir {
    iter: Option<OwningIter>,
}

impl ReadDir {
    pub(super) async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let dir =
                NixDir::open(&path, OFlag::O_DIRECTORY, NixMode::empty()).map_err(Error::other)?;
            Ok(Self {
                iter: Some(dir.into_iter()),
            })
        })
        .await
        .map_err(Error::other)?
    }

    pub(crate) async fn next_entry(&mut self) -> Result<Option<crate::directory::DirEntry>> {
        let mut iter = match self.iter.take() {
            Some(iter) => iter,
            None => return Ok(None),
        };
        let (result, next_iter) = tokio::task::spawn_blocking(move || {
            loop {
                match iter.next() {
                    Some(Ok(entry)) => {
                        let name = entry.file_name().to_bytes();
                        if name == b"." || name == b".." {
                            continue;
                        }
                        let file_name = match String::from_utf8(name.to_vec()) {
                            Ok(name) => name,
                            Err(error) => {
                                return (
                                    Err(Error::new(ErrorKind::InvalidData, error)),
                                    Some(iter),
                                );
                            }
                        };
                        let file_type = entry
                            .file_type()
                            .map(|ty| match ty {
                                Type::File => FileType::File,
                                Type::Directory => FileType::Dir,
                                Type::Symlink => FileType::Symlink,
                                Type::Fifo => FileType::Fifo,
                                Type::CharacterDevice => FileType::CharacterDevice,
                                Type::BlockDevice => FileType::BlockDevice,
                                Type::Socket => FileType::Socket,
                            })
                            .unwrap_or(FileType::Unknown);
                        return (
                            Ok(Some(crate::directory::DirEntry::new(
                                file_name,
                                file_type,
                                crate::directory::DirEntryFamily::Unix { ino: entry.ino() },
                            ))),
                            Some(iter),
                        );
                    }
                    Some(Err(error)) => return (Err(Error::other(error)), Some(iter)),
                    None => return (Ok(None), None),
                }
            }
        })
        .await
        .map_err(Error::other)?;
        self.iter = next_iter;
        result
    }
}

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

#[derive(Clone, Copy)]
pub(super) enum UnixXattrTarget<'a> {
    Fd(BorrowedFd<'a>),
    Path(&'a CStr, bool),
}

impl File {
    pub(super) fn pread(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
        Ok(std::os::unix::fs::FileExt::read_at(file, buf, offset)?)
    }

    pub(super) fn pwrite(file: &std::fs::File, buf: &[u8], offset: u64) -> Result<usize> {
        Ok(std::os::unix::fs::FileExt::write_at(file, buf, offset)?)
    }

    /// Identifies the file behind an open descriptor, as `(dev, ino)`.
    pub(super) fn impl_id(file: &std::fs::File) -> Result<FileId> {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata()?;
        Ok(FileId {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }
}

impl Direct {
    pub(super) async fn impl_access(path: PathBuf, mode: crate::file::AccessFlags) -> Result<()> {
        let mode = nix::unistd::AccessFlags::from_bits(mode.bits())
            .unwrap_or(nix::unistd::AccessFlags::empty());
        tokio::task::spawn_blocking(move || nix::unistd::access(&path, mode))
            .await
            .map_err(Error::other)?
            .map_err(|error| Error::from_raw_os_error(error as i32))
    }

    pub(super) async fn impl_rename(from: PathBuf, to: PathBuf, replace: bool) -> Result<()> {
        if replace {
            return Ok(fs::rename(from, to).await?);
        }

        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        tokio::task::spawn_blocking(move || Self::rename_no_replace(&from, &to))
            .await
            .unwrap_or_else(|_| Err(Error::other("rename task failed")))
    }

    #[cfg(target_os = "linux")]
    fn rename_no_replace(from: &CStr, to: &CStr) -> Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
        ) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "atomic rename without replacement is not supported",
            ));
        }
        Err(error)
    }

    #[cfg(target_os = "macos")]
    fn rename_no_replace(from: &CStr, to: &CStr) -> Result<()> {
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
            return Ok(());
        }
        let error = Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "atomic rename without replacement is not supported",
            ));
        }
        Err(error)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn rename_no_replace(_from: &CStr, _to: &CStr) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "atomic rename without replacement is not supported",
        ))
    }

    pub(super) fn sec_desc_from_path(
        _path: &Path,
        _mask: dolang_winterop::security::SecInfo,
        _follow: bool,
    ) -> Result<SecDesc> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) fn update_sec_desc_path(
        _path: &Path,
        _descriptor: &SecDesc,
        _follow: bool,
    ) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) fn sec_desc_from_file(
        _file: &std::fs::File,
        _mask: dolang_winterop::security::SecInfo,
    ) -> Result<SecDesc> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) fn update_sec_desc_file(_file: &std::fs::File, _descriptor: &SecDesc) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "security descriptors are only supported on Windows",
        ))
    }

    pub(super) async fn impl_user_name(&self, uid: u32) -> Result<String> {
        tokio::task::spawn_blocking(move || {
            nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))?
                .map(|user| user.name)
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "user ID not found"))
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("user lookup task failed")))
    }

    pub(super) async fn impl_user_id(&self, name: &str) -> Result<u32> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            nix::unistd::User::from_name(&name)?
                .map(|user| user.uid.as_raw())
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "user name not found"))
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("user lookup task failed")))
    }

    pub(super) async fn impl_group_name(&self, gid: u32) -> Result<String> {
        tokio::task::spawn_blocking(move || {
            nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))?
                .map(|group| group.name)
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "group ID not found"))
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("group lookup task failed")))
    }

    pub(super) async fn impl_group_id(&self, name: &str) -> Result<u32> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            nix::unistd::Group::from_name(&name)?
                .map(|group| group.gid.as_raw())
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "group name not found"))
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("group lookup task failed")))
    }

    pub(super) fn program_not_found_error() -> Error {
        Error::from_raw_os_error(libc::ENOENT)
    }

    pub(super) fn directory_requires_all_error() -> Error {
        Error::from_raw_os_error(libc::EISDIR)
    }

    pub(super) fn directory_not_empty_error() -> Error {
        Error::from_raw_os_error(libc::ENOTEMPTY)
    }

    pub(super) fn not_a_directory_error() -> Error {
        Error::from_raw_os_error(libc::ENOTDIR)
    }

    fn statvfs_from_fd(fd: BorrowedFd<'_>) -> Result<libc::statvfs> {
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::fstatvfs(fd.as_raw_fd(), stat.as_mut_ptr()) };
        if rc == 0 {
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(Error::last_os_error())
        }
    }

    fn statvfs_from_path(path: &Path) -> Result<libc::statvfs> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
        if rc == 0 {
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(Error::last_os_error())
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
                #[cfg(target_os = "freebsd")]
                platform: UnixFsMetadataPlatform::FreeBsd {
                    flags: stat.f_flag.into(),
                },
            }),
        }
    }

    pub(super) fn fs_metadata_from_file(file: &std::fs::File) -> Result<FsMetadata> {
        Self::statvfs_from_fd(file.as_fd()).map(Self::fs_metadata_from_statvfs)
    }

    pub(super) fn fs_metadata_from_path(path: &Path, follow: bool) -> Result<FsMetadata> {
        if !follow {
            return Err(Error::new(
                ErrorKind::Unsupported,
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
    unsafe fn get_linux_flags(fd: RawFd) -> Result<libc::c_long> {
        nix::ioctl_read!(fs_ioc_getflags, b'f', 1, libc::c_long);

        let mut flags = 0;
        unsafe { fs_ioc_getflags(fd, &mut flags) }?;
        Ok(flags)
    }

    #[cfg(target_os = "linux")]
    unsafe fn set_linux_flags(fd: RawFd, flags: libc::c_long) -> Result<()> {
        nix::ioctl_write_ptr!(fs_ioc_setflags, b'f', 2, libc::c_long);

        unsafe { fs_ioc_setflags(fd, &flags) }?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn attrs_from_path(path: PathBuf, _follow: bool) -> Result<u32> {
        let file = std::fs::File::open(path)?;
        unsafe { Self::get_linux_flags(file.as_raw_fd()) }.map(Self::attrs_from_flags)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn metadata_with_attrs(
        metadata: std::fs::Metadata,
        file: &std::fs::File,
    ) -> Result<Metadata> {
        let mut metadata = crate::metadata::metadata_from_std(metadata);
        if !matches!(
            metadata.file_type,
            crate::metadata::FileType::File | crate::metadata::FileType::Dir
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
    pub(super) fn metadata_from_path(path: &Path, follow: bool) -> Result<Metadata> {
        let std_metadata = if follow {
            std::fs::metadata(path)?
        } else {
            std::fs::symlink_metadata(path)?
        };
        let mut metadata = crate::metadata::metadata_from_std(std_metadata);
        if !follow
            || !matches!(
                metadata.file_type,
                crate::metadata::FileType::File | crate::metadata::FileType::Dir
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
    fn validate_attrs_patch(patch: AttrsPatch) -> Result<()> {
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
            Err(Error::new(
                ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: AttrsPatch) -> Result<()> {
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

    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    pub(super) fn metadata_with_attrs(
        metadata: std::fs::Metadata,
        _file: &std::fs::File,
    ) -> Result<Metadata> {
        Ok(crate::metadata::metadata_from_std(metadata))
    }

    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    pub(super) fn metadata_from_path(path: &Path, follow: bool) -> Result<Metadata> {
        let metadata = if follow {
            std::fs::metadata(path)?
        } else {
            std::fs::symlink_metadata(path)?
        };
        Ok(crate::metadata::metadata_from_std(metadata))
    }

    #[cfg(target_os = "macos")]
    fn validate_attrs_patch(patch: AttrsPatch) -> Result<()> {
        let supported = AttrFlags::HIDDEN
            .union(AttrFlags::IMMUTABLE)
            .union(AttrFlags::APPEND_ONLY)
            .union(AttrFlags::NO_DUMP)
            .union(AttrFlags::OPAQUE);
        if !patch.requested().difference(supported).is_empty() {
            Err(Error::new(
                ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: AttrsPatch) -> Result<()> {
        use nix::sys::stat::{self, FileFlag};

        Self::validate_attrs_patch(patch)?;

        if patch.is_empty() {
            return Ok(());
        }

        let stat = stat::stat(&path)?;
        let mut flags = FileFlag::from_bits_retain(stat.st_flags);
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
        Ok(nix::unistd::chflags(&path, flags)?)
    }

    #[cfg(target_os = "freebsd")]
    fn validate_attrs_patch(patch: AttrsPatch) -> Result<()> {
        let supported = AttrFlags::READONLY
            .union(AttrFlags::HIDDEN)
            .union(AttrFlags::SYSTEM)
            .union(AttrFlags::ARCHIVE)
            .union(AttrFlags::COMPRESSED)
            .union(AttrFlags::OFFLINE)
            .union(AttrFlags::IMMUTABLE)
            .union(AttrFlags::APPEND_ONLY)
            .union(AttrFlags::NO_DUMP)
            .union(AttrFlags::OPAQUE);
        if !patch.requested().difference(supported).is_empty() {
            Err(Error::new(
                ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "freebsd")]
    pub(super) fn set_attrs_path(path: PathBuf, patch: AttrsPatch) -> Result<()> {
        use nix::sys::stat::FileFlag;

        const UF_COMPRESSED: libc::c_ulong = 0x0000_0020;

        Self::validate_attrs_patch(patch)?;

        if patch.is_empty() {
            return Ok(());
        }

        let stat = nix::sys::stat::stat(&path)?;
        let mut flags = FileFlag::from_bits_retain(stat.st_flags.into());
        for (semantic, native) in [
            (
                AttrFlags::READONLY,
                FileFlag::from_bits_retain(libc::UF_READONLY),
            ),
            (
                AttrFlags::HIDDEN,
                FileFlag::from_bits_retain(libc::UF_HIDDEN),
            ),
            (
                AttrFlags::SYSTEM,
                FileFlag::from_bits_retain(libc::UF_SYSTEM),
            ),
            (
                AttrFlags::ARCHIVE,
                FileFlag::from_bits_retain(libc::UF_ARCHIVE),
            ),
            (
                AttrFlags::COMPRESSED,
                FileFlag::from_bits_retain(UF_COMPRESSED),
            ),
            (
                AttrFlags::OFFLINE,
                FileFlag::from_bits_retain(libc::UF_OFFLINE),
            ),
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
        Ok(nix::unistd::chflags(&path, flags)?)
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
    ) -> Result<Option<PathBuf>> {
        match Self::override_or_env(env, key) {
            Some(value) => {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    Ok(Some(path))
                } else {
                    Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("{key} must be an absolute path"),
                    ))
                }
            }
            None => Ok(None),
        }
    }

    pub(super) fn home_dir_platform(env: &HashMap<String, Option<String>>) -> Result<PathBuf> {
        if let Some(home) = Self::absolute_env_path(env, "HOME")? {
            return Ok(home);
        }

        let uid = nix::unistd::getuid();
        let user = nix::unistd::User::from_uid(uid)
            .map_err(Error::other)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "could not resolve home directory"))?;
        let home = user.dir;
        if home.is_absolute() {
            Ok(home)
        } else {
            Err(Error::new(
                ErrorKind::InvalidData,
                "resolved home directory is not absolute",
            ))
        }
    }

    pub(super) fn cache_dir_platform(
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf> {
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

    pub(super) fn data_dir_platform(
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
        _local: bool,
    ) -> Result<PathBuf> {
        #[cfg(target_os = "macos")]
        let base = Self::home_dir_platform(env)?.join("Library/Application Support");
        #[cfg(not(target_os = "macos"))]
        let base = match Self::absolute_env_path(env, "XDG_DATA_HOME")? {
            Some(path) => path,
            None => Self::home_dir_platform(env)?.join(".local/share"),
        };
        Ok(match app {
            Some(app) => base.join(app),
            None => base,
        })
    }

    pub(super) fn state_dir_platform(
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> Result<Option<PathBuf>> {
        #[cfg(target_os = "macos")]
        {
            let _ = (app, env);
            Ok(None)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let base = match Self::absolute_env_path(env, "XDG_STATE_HOME")? {
                Some(path) => path,
                None => Self::home_dir_platform(env)?.join(".local/state"),
            };
            Ok(Some(match app {
                Some(app) => base.join(app),
                None => base,
            }))
        }
    }

    pub(super) fn temp_dir_platform(env: &HashMap<String, Option<String>>) -> Result<PathBuf> {
        Ok(Self::absolute_env_path(env, "TMPDIR")?.unwrap_or_else(|| PathBuf::from("/tmp")))
    }

    pub(super) fn unix_xattr_namespace(namespace: XattrNamespace<'_>) -> Result<Option<Vec<u8>>> {
        #[cfg(target_os = "macos")]
        {
            if !matches!(namespace, XattrNamespace::Default) {
                return Err(Error::new(
                    ErrorKind::InvalidData,
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
        #[cfg(target_os = "freebsd")]
        {
            Ok(match namespace {
                XattrNamespace::Default => Some(b"user".to_vec()),
                XattrNamespace::Named(namespace) => {
                    Self::freebsd_xattr_namespace(namespace)?;
                    Some(namespace.as_bytes().to_vec())
                }
                XattrNamespace::Any => None,
            })
        }
    }

    pub(super) fn xattr_path(path: &Path) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "path contains NUL"))
    }

    pub(super) fn xattr_name(name: &str, namespace: Option<&str>) -> Result<CString> {
        #[cfg(target_os = "linux")]
        let full_name = match namespace {
            Some(namespace) => format!("{namespace}.{name}"),
            None => format!("user.{name}"),
        };
        #[cfg(target_os = "macos")]
        let full_name = match namespace {
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "xattr namespaces are not supported on this platform",
                ));
            }
            None => name.to_owned(),
        };
        #[cfg(target_os = "freebsd")]
        let full_name = {
            let namespace = namespace.unwrap_or("user");
            Self::freebsd_xattr_namespace(namespace)?;
            format!("{namespace}.{name}")
        };
        CString::new(full_name)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "xattr name contains NUL"))
    }

    #[cfg(not(target_os = "freebsd"))]
    fn xattr_entry(raw_name: Vec<u8>) -> Result<XattrEntry> {
        let name = String::from_utf8(raw_name)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "xattr name is not UTF-8"))?;
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

    #[cfg(target_os = "freebsd")]
    fn freebsd_xattr_namespace(namespace: &str) -> Result<libc::c_int> {
        match namespace {
            "user" => Ok(libc::EXTATTR_NAMESPACE_USER),
            "system" => Ok(libc::EXTATTR_NAMESPACE_SYSTEM),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                "FreeBSD xattr namespace must be user or system",
            )),
        }
    }

    #[cfg(target_os = "freebsd")]
    fn freebsd_xattr_name(name: &CStr) -> Result<(libc::c_int, &CStr)> {
        let bytes = name.to_bytes_with_nul();
        let separator = bytes
            .iter()
            .position(|byte| *byte == b'.')
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "missing namespace"))?;
        let namespace = std::str::from_utf8(&bytes[..separator])
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid namespace"))?;
        let name = CStr::from_bytes_with_nul(&bytes[separator + 1..])
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid xattr name"))?;
        Ok((Self::freebsd_xattr_namespace(namespace)?, name))
    }

    #[cfg(not(target_os = "freebsd"))]
    pub(super) fn unix_list_xattrs(
        target: UnixXattrTarget<'_>,
        namespace: Option<Vec<u8>>,
    ) -> Result<Vec<XattrEntry>> {
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
            return Err(Error::last_os_error());
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
                let err = Error::last_os_error();
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
                        return Err(Error::last_os_error());
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

    #[cfg(target_os = "freebsd")]
    pub(super) fn unix_list_xattrs(
        target: UnixXattrTarget<'_>,
        namespace: Option<Vec<u8>>,
    ) -> Result<Vec<XattrEntry>> {
        let mut entries = Vec::new();
        if let Some(namespace) = namespace {
            let namespace = std::str::from_utf8(&namespace)
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid xattr namespace"))?;
            Self::freebsd_list_xattrs_in_namespace(
                target,
                namespace,
                Self::freebsd_xattr_namespace(namespace)?,
                &mut entries,
            )?;
        } else {
            for (namespace, native) in [
                ("user", libc::EXTATTR_NAMESPACE_USER),
                ("system", libc::EXTATTR_NAMESPACE_SYSTEM),
            ] {
                if let Err(error) =
                    Self::freebsd_list_xattrs_in_namespace(target, namespace, native, &mut entries)
                {
                    if native == libc::EXTATTR_NAMESPACE_SYSTEM
                        && matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM))
                    {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Ok(entries)
    }

    #[cfg(target_os = "freebsd")]
    fn freebsd_list_xattrs_in_namespace(
        target: UnixXattrTarget<'_>,
        namespace: &str,
        native: libc::c_int,
        entries: &mut Vec<XattrEntry>,
    ) -> Result<()> {
        fn list(
            target: UnixXattrTarget<'_>,
            namespace: libc::c_int,
            data: *mut libc::c_void,
            len: usize,
        ) -> libc::ssize_t {
            unsafe {
                match target {
                    UnixXattrTarget::Fd(fd) => {
                        libc::extattr_list_fd(fd.as_raw_fd(), namespace, data, len)
                    }
                    UnixXattrTarget::Path(path, true) => {
                        libc::extattr_list_file(path.as_ptr(), namespace, data, len)
                    }
                    UnixXattrTarget::Path(path, false) => {
                        libc::extattr_list_link(path.as_ptr(), namespace, data, len)
                    }
                }
            }
        }

        let mut size = list(target, native, std::ptr::null_mut(), 0);
        if size < 0 {
            return Err(Error::last_os_error());
        }
        loop {
            let mut buf = vec![0u8; size as usize];
            let read = list(target, native, buf.as_mut_ptr().cast(), buf.len());
            if read < 0 {
                let error = Error::last_os_error();
                if error.raw_os_error() == Some(libc::ERANGE) {
                    size = list(target, native, std::ptr::null_mut(), 0);
                    if size < 0 {
                        return Err(Error::last_os_error());
                    }
                    continue;
                }
                return Err(error);
            }
            buf.truncate(read as usize);
            let mut remaining = buf.as_slice();
            while let Some((&name_len, rest)) = remaining.split_first() {
                let name_len = usize::from(name_len);
                if rest.len() < name_len {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "malformed FreeBSD xattr list",
                    ));
                }
                let (name, tail) = rest.split_at(name_len);
                let name = std::str::from_utf8(name)
                    .map_err(|_| Error::new(ErrorKind::InvalidData, "xattr name is not UTF-8"))?;
                entries.push(XattrEntry {
                    name: name.to_owned(),
                    namespace: Some(namespace.to_owned()),
                    size: None,
                    flags: None,
                });
                remaining = tail;
            }
            return Ok(());
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    pub(super) fn unix_get_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> Result<Vec<u8>> {
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
            return Err(Error::last_os_error());
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
                let err = Error::last_os_error();
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
                        return Err(Error::last_os_error());
                    }
                    continue;
                }
                return Err(err);
            }
            buf.truncate(read as usize);
            return Ok(buf);
        }
    }

    #[cfg(target_os = "freebsd")]
    pub(super) fn unix_get_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> Result<Vec<u8>> {
        fn get(
            target: UnixXattrTarget<'_>,
            namespace: libc::c_int,
            name: &CStr,
            data: *mut libc::c_void,
            len: usize,
        ) -> libc::ssize_t {
            unsafe {
                match target {
                    UnixXattrTarget::Fd(fd) => {
                        libc::extattr_get_fd(fd.as_raw_fd(), namespace, name.as_ptr(), data, len)
                    }
                    UnixXattrTarget::Path(path, true) => {
                        libc::extattr_get_file(path.as_ptr(), namespace, name.as_ptr(), data, len)
                    }
                    UnixXattrTarget::Path(path, false) => {
                        libc::extattr_get_link(path.as_ptr(), namespace, name.as_ptr(), data, len)
                    }
                }
            }
        }

        let (namespace, name) = Self::freebsd_xattr_name(name)?;
        let mut size = get(target, namespace, name, std::ptr::null_mut(), 0);
        if size < 0 {
            return Err(Error::last_os_error());
        }
        loop {
            let mut buf = vec![0u8; size as usize];
            let read = get(target, namespace, name, buf.as_mut_ptr().cast(), buf.len());
            if read < 0 {
                let error = Error::last_os_error();
                if error.raw_os_error() == Some(libc::ERANGE) {
                    size = get(target, namespace, name, std::ptr::null_mut(), 0);
                    if size < 0 {
                        return Err(Error::last_os_error());
                    }
                    continue;
                }
                return Err(error);
            }
            buf.truncate(read as usize);
            return Ok(buf);
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    pub(super) fn unix_set_xattr(
        target: UnixXattrTarget<'_>,
        name: &CStr,
        value: &[u8],
    ) -> Result<()> {
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
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "freebsd")]
    pub(super) fn unix_set_xattr(
        target: UnixXattrTarget<'_>,
        name: &CStr,
        value: &[u8],
    ) -> Result<()> {
        let (namespace, name) = Self::freebsd_xattr_name(name)?;
        let result = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => libc::extattr_set_fd(
                    fd.as_raw_fd(),
                    namespace,
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                ),
                UnixXattrTarget::Path(path, true) => libc::extattr_set_file(
                    path.as_ptr(),
                    namespace,
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                ),
                UnixXattrTarget::Path(path, false) => libc::extattr_set_link(
                    path.as_ptr(),
                    namespace,
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                ),
            }
        };
        if result < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    pub(super) fn unix_remove_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> Result<()> {
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
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "freebsd")]
    pub(super) fn unix_remove_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> Result<()> {
        let (namespace, name) = Self::freebsd_xattr_name(name)?;
        let result = unsafe {
            match target {
                UnixXattrTarget::Fd(fd) => {
                    libc::extattr_delete_fd(fd.as_raw_fd(), namespace, name.as_ptr())
                }
                UnixXattrTarget::Path(path, true) => {
                    libc::extattr_delete_file(path.as_ptr(), namespace, name.as_ptr())
                }
                UnixXattrTarget::Path(path, false) => {
                    libc::extattr_delete_link(path.as_ptr(), namespace, name.as_ptr())
                }
            }
        };
        if result < 0 {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) async fn chown_local(
        path: PathBuf,
        user: Option<OwnershipIdentity>,
        group: Option<OwnershipIdentity>,
        follow: bool,
    ) -> Result<()> {
        use nix::{
            errno::Errno,
            unistd::{Gid, Group, Uid, User, chown},
        };
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        fn resolve_user(user: Option<OwnershipIdentity>) -> Result<Option<Uid>> {
            match user {
                None => Ok(None),
                Some(OwnershipIdentity::Id(id)) => Ok(Some(Uid::from_raw(id))),
                Some(OwnershipIdentity::Name(name)) => match User::from_name(&name)? {
                    Some(user) => Ok(Some(user.uid)),
                    None => Err(Error::new(
                        ErrorKind::NotFound,
                        format!("user not found: {name}"),
                    )),
                },
                Some(OwnershipIdentity::Sid(_)) => Err(Error::new(
                    ErrorKind::InvalidInput,
                    "SID ownership identities are not supported on Unix",
                )),
            }
        }

        fn resolve_group(group: Option<OwnershipIdentity>) -> Result<Option<Gid>> {
            match group {
                None => Ok(None),
                Some(OwnershipIdentity::Id(id)) => Ok(Some(Gid::from_raw(id))),
                Some(OwnershipIdentity::Name(name)) => match Group::from_name(&name)? {
                    Some(group) => Ok(Some(group.gid)),
                    None => Err(Error::new(
                        ErrorKind::NotFound,
                        format!("group not found: {name}"),
                    )),
                },
                Some(OwnershipIdentity::Sid(_)) => Err(Error::new(
                    ErrorKind::InvalidInput,
                    "SID ownership identities are not supported on Unix",
                )),
            }
        }

        fn lchown_path(
            path: &Path,
            user: Option<Uid>,
            group: Option<Gid>,
        ) -> std::result::Result<(), Errno> {
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
            result.map_err(|err| Error::from_raw_os_error(err as i32))
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("failed to join ownership update task")))
    }

    pub(super) async fn impl_copy_symlink(src: &Path, dst: &Path) -> Result<()> {
        let target = fs::read_link(src).await?;
        Self::impl_symlink(Path::new(""), &target, dst).await
    }

    pub(super) async fn impl_symlink(_cwd: &Path, src: &Path, dst: &Path) -> Result<()> {
        Ok(fs::symlink(src, dst).await?)
    }

    pub(super) async fn impl_symlink_dir(src: &Path, dst: &Path) -> Result<()> {
        Ok(fs::symlink(src, dst).await?)
    }

    pub(super) async fn impl_symlink_file(src: &Path, dst: &Path) -> Result<()> {
        Ok(fs::symlink(src, dst).await?)
    }

    pub(super) async fn impl_xattrs(
        &self,
        path: &Path,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>> {
        let path = Self::xattr_path(path)?;
        let namespace = Self::unix_xattr_namespace(namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_list_xattrs(UnixXattrTarget::Path(&path, follow), namespace)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_streams(
        &self,
        _path: &Path,
        _follow: bool,
    ) -> Result<Vec<StreamEntry>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    pub(super) async fn impl_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>> {
        let path = Self::xattr_path(path)?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_get_xattr(UnixXattrTarget::Path(&path, follow), &name)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_set_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<()> {
        let path = Self::xattr_path(path)?;
        let name = Self::xattr_name(name, namespace)?;
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || {
            Self::unix_set_xattr(UnixXattrTarget::Path(&path, follow), &name, &value)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_remove_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<()> {
        let path = Self::xattr_path(path)?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_remove_xattr(UnixXattrTarget::Path(&path, follow), &name)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_xattrs(
        file: &Arc<std::fs::File>,
        namespace: XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>> {
        let file = Arc::clone(file);
        let namespace = Self::unix_xattr_namespace(namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_list_xattrs(UnixXattrTarget::Fd(file.as_fd()), namespace)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_xattr(
        file: &Arc<std::fs::File>,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<u8>> {
        let file = Arc::clone(file);
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_get_xattr(UnixXattrTarget::Fd(file.as_fd()), &name)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_streams(_file: &Arc<std::fs::File>) -> Result<Vec<StreamEntry>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    pub(super) async fn impl_file_set_xattr(
        file: &Arc<std::fs::File>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<()> {
        let file = Arc::clone(file);
        let name = Self::xattr_name(name, namespace)?;
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || {
            Self::unix_set_xattr(UnixXattrTarget::Fd(file.as_fd()), &name, &value)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_file_remove_xattr(
        file: &Arc<std::fs::File>,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<()> {
        let file = Arc::clone(file);
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_remove_xattr(UnixXattrTarget::Fd(file.as_fd()), &name)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_set_attrs(&self, path: &Path, attrs: AttrsPatch) -> Result<()> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::set_attrs_path(path, attrs))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join attrs update task")))
    }

    pub(super) async fn impl_update_metadata(
        &self,
        paths: &[PathBuf],
        mut patch: MetadataPatch,
    ) -> Result<()> {
        if patch.is_empty() {
            return Ok(());
        }
        if !patch.follow && (patch.mode.is_some() || !patch.attrs.is_empty()) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "mode and attributes cannot be set without following symlinks on this platform",
            ));
        }
        if patch.created.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
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
                    Some(OwnershipIdentity::Name(name)) => User::from_name(&name)?
                        .map(|user| Some(OwnershipIdentity::Id(user.uid.as_raw())))
                        .ok_or_else(|| {
                            Error::new(ErrorKind::NotFound, format!("user not found: {name}"))
                        })?,
                    Some(OwnershipIdentity::Sid(_)) => {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "SID ownership identities are not supported on Unix",
                        ));
                    }
                    value => value,
                };
                let group = match group {
                    Some(OwnershipIdentity::Name(name)) => Group::from_name(&name)?
                        .map(|group| Some(OwnershipIdentity::Id(group.gid.as_raw())))
                        .ok_or_else(|| {
                            Error::new(ErrorKind::NotFound, format!("group not found: {name}"))
                        })?,
                    Some(OwnershipIdentity::Sid(_)) => {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            "SID ownership identities are not supported on Unix",
                        ));
                    }
                    value => value,
                };
                Ok((user, group))
            })
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join ownership lookup task")))?;
        }

        for path in paths {
            if patch.user.is_some() || patch.group.is_some() {
                self.impl_chown(path, patch.user.clone(), patch.group.clone(), patch.follow)
                    .await?;
            }
            if let Some(mode) = patch.mode {
                self.impl_set_permissions(path, mode).await?;
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

    pub(super) async fn impl_canonicalize(&self, path: &Path) -> Result<PathBuf> {
        Ok(fs::canonicalize(path).await?)
    }

    pub(super) async fn impl_set_permissions(&self, path: &Path, mode: Mode) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        Ok(fs::set_permissions(path, std::fs::Permissions::from_mode(mode.bits())).await?)
    }

    async fn impl_set_file_times(
        &self,
        path: &Path,
        accessed: Option<i128>,
        modified: Option<i128>,
        created: Option<i128>,
        follow: bool,
    ) -> Result<()> {
        use nix::{
            fcntl::AT_FDCWD,
            sys::{
                stat::{UtimensatFlags, utimensat},
                time::TimeSpec,
            },
        };

        fn unix_timespec(time: Option<i128>) -> Result<TimeSpec> {
            let Some(time) = time else {
                return Ok(TimeSpec::UTIME_OMIT);
            };
            let secs = i64::try_from(time.div_euclid(1_000_000_000))
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid timestamp"))?;
            let nanos = i64::try_from(time.rem_euclid(1_000_000_000))
                .expect("nanosecond remainder is in i64 range");
            Ok(TimeSpec::new(secs, nanos))
        }

        if created.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "created timestamp is not supported on this platform",
            ));
        }

        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let atime = unix_timespec(accessed)?;
            let mtime = unix_timespec(modified)?;
            let flags = if follow {
                UtimensatFlags::FollowSymlink
            } else {
                UtimensatFlags::NoFollowSymlink
            };
            Ok(utimensat(AT_FDCWD, &path, &atime, &mtime, flags)?)
        })
        .await
        .unwrap_or_else(|_| Err(Error::from_raw_os_error(libc::EIO)))
    }

    pub(super) async fn impl_chown(
        &self,
        path: &Path,
        user: Option<OwnershipIdentity>,
        group: Option<OwnershipIdentity>,
        follow: bool,
    ) -> Result<()> {
        Self::chown_local(path.to_path_buf(), user, group, follow).await
    }
}

impl Child {
    pub(super) async fn impl_terminate(self) -> Result<Option<std::process::ExitStatus>> {
        let mut child = self.inner;
        let Some(pid) = child.id() else {
            return Ok(child.wait().await.map(Some)?);
        };
        let target = match self.process_control {
            ProcessControl::Foreground => pid as libc::pid_t,
            ProcessControl::Background => -(pid as libc::pid_t),
        };
        let signal = signal_to_raw(self.termination_policy.signal)?;

        let send = |signal| {
            let result = unsafe { libc::kill(target, signal) };
            if result == 0 {
                Ok(())
            } else {
                let error = Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        };
        send(signal)?;

        let wait = async {
            Ok(match self.process_control {
                ProcessControl::Foreground => child.wait().await,
                ProcessControl::Background => {
                    let mut root_status = None;
                    loop {
                        if root_status.is_none() {
                            root_status = child.try_wait()?;
                        }
                        if unsafe { libc::kill(target, 0) } != 0 {
                            let error = Error::last_os_error();
                            if error.raw_os_error() == Some(libc::ESRCH) {
                                break match root_status {
                                    Some(status) => Ok(status),
                                    None => child.wait().await,
                                };
                            }
                            if error.raw_os_error() != Some(libc::EPERM) {
                                return Err(error);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }?)
        };
        if let Ok(status) = timeout(self.termination_policy.grace, wait).await {
            return status.map(Some);
        }
        if !self.termination_policy.force {
            return Ok(None);
        }

        send(libc::SIGKILL)?;
        // Remaining group members are not necessarily our children. In
        // particular, an orphaned zombie can keep kill(-pgid, 0) succeeding
        // indefinitely when PID 1 does not reap it.
        Ok(child.wait().await.map(Some)?)
    }
}

impl Command<'_> {
    pub(super) fn configure_process(&self, command: &mut tokio::process::Command) -> Result<()> {
        signal_to_raw(self.termination_policy.signal)?;
        if self.process_control == ProcessControl::Background {
            command.as_std_mut().process_group(0);
        }
        Ok(())
    }

    pub(super) fn finish_spawn(&self, child: tokio::process::Child) -> Result<Child> {
        Ok(Child::new(
            child,
            self.process_control,
            self.termination_policy,
        ))
    }

    pub(super) fn impl_stdout_inherit_stderr(&mut self) -> Result<&mut Self> {
        self.stdout = Some(std::process::Stdio::from(
            std::io::stderr().as_fd().try_clone_to_owned()?,
        ));
        Ok(self)
    }

    pub(super) fn impl_stderr_inherit_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = Some(std::process::Stdio::from(
            std::io::stdout().as_fd().try_clone_to_owned()?,
        ));
        Ok(self)
    }
}

pub(super) fn signal_to_raw(signal: crate::process::Signal) -> Result<libc::c_int> {
    use crate::process::Signal;
    let signal = match signal {
        Signal::Hup => libc::SIGHUP,
        Signal::Int => libc::SIGINT,
        Signal::Quit => libc::SIGQUIT,
        Signal::Ill => libc::SIGILL,
        Signal::Trap => libc::SIGTRAP,
        Signal::Abrt => libc::SIGABRT,
        Signal::Fpe => libc::SIGFPE,
        Signal::Kill => libc::SIGKILL,
        Signal::Bus => libc::SIGBUS,
        Signal::Segv => libc::SIGSEGV,
        Signal::Sys => libc::SIGSYS,
        Signal::Pipe => libc::SIGPIPE,
        Signal::Alrm => libc::SIGALRM,
        Signal::Term => libc::SIGTERM,
        Signal::Urg => libc::SIGURG,
        Signal::Stop => libc::SIGSTOP,
        Signal::Tstp => libc::SIGTSTP,
        Signal::Cont => libc::SIGCONT,
        Signal::Chld => libc::SIGCHLD,
        Signal::Ttin => libc::SIGTTIN,
        Signal::Ttou => libc::SIGTTOU,
        Signal::Io => libc::SIGIO,
        Signal::Xcpu => libc::SIGXCPU,
        Signal::Xfsz => libc::SIGXFSZ,
        Signal::Vtalrm => libc::SIGVTALRM,
        Signal::Prof => libc::SIGPROF,
        Signal::Winch => libc::SIGWINCH,
        Signal::Usr1 => libc::SIGUSR1,
        Signal::Usr2 => libc::SIGUSR2,
        #[cfg(any(target_os = "freebsd", target_os = "macos"))]
        Signal::Emt => libc::SIGEMT,
        #[cfg(any(target_os = "freebsd", target_os = "macos"))]
        Signal::Info => libc::SIGINFO,
        #[cfg(target_os = "linux")]
        Signal::Stkflt => libc::SIGSTKFLT,
        #[cfg(target_os = "linux")]
        Signal::Pwr => libc::SIGPWR,
        #[cfg(target_os = "freebsd")]
        Signal::Thr => libc::SIGTHR,
        #[cfg(target_os = "freebsd")]
        Signal::Librt => libc::SIGLIBRT,
        Signal::Number(signal) if signal > 0 => signal,
        Signal::Number(_) => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "termination signal must be positive",
            ));
        }
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("{signal:?} is not supported on this platform"),
            ));
        }
    };
    Ok(signal)
}

impl super::OpenOptions {
    pub(super) fn apply_no_follow_flags(&self, opts: &mut OpenOptions) {
        if self.no_follow {
            opts.custom_flags(libc::O_NOFOLLOW);
        }
    }
}
