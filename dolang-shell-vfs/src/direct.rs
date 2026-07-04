use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
};

#[cfg(unix)]
use std::{
    ffi::{CStr, CString},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
};

#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

#[cfg(windows)]
use std::{
    fs::File as StdFile,
    mem,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Prefix},
    ptr, slice,
    time::SystemTime,
};
#[cfg(windows)]
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_FULL_EA_INFORMATION, FILE_GET_EA_INFORMATION, NtQueryEaFile, NtSetEaFile,
    },
    Win32::{
        Foundation::{
            ERROR_HANDLE_EOF, ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
            RtlNtStatusToDosError, S_OK, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
            STATUS_NO_EAS_ON_FILE, STATUS_NO_MORE_EAS, STATUS_SUCCESS,
        },
        Storage::FileSystem::{
            COMPRESSION_FORMAT_DEFAULT, COMPRESSION_FORMAT_NONE, CreateFileW,
            FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
            FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FILE_STREAM_INFO, FileStreamInfo, GetDiskFreeSpaceExW, GetFileAttributesW,
            GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
            INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, SetFileAttributesW, VOLUME_NAME_DOS,
        },
        System::{
            Com::CoTaskMemFree,
            IO::{DeviceIoControl, IO_STATUS_BLOCK},
            Ioctl::FSCTL_SET_COMPRESSION,
        },
        UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_Profile, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
        },
    },
    core::GUID,
};

use tokio::{
    fs::{self, File, OpenOptions},
    process::Command as TokioCommand,
    sync::Mutex,
    time::Duration,
};

#[cfg(unix)]
use tokio::time::timeout;

use wax::{
    Glob,
    walk::{DepthBehavior, DepthMax, Entry, LinkBehavior, WalkBehavior},
};

#[cfg(windows)]
use crate::OpenOptions as _;
use crate::{
    Attrs, Child, ChownIdentity, Command, FsMetadata, Metadata, Permissions, PipeRecv, PipeSend,
    ReadDir, StreamEntry, Vfs, WellKnownPath, XattrEntry, XattrNamespace,
};

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

#[derive(Debug, Clone)]
pub struct Direct {
    path_cache: Arc<PathCache>,
}

#[derive(Debug, Default)]
pub struct DirectOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    no_follow: bool,
}

pub struct DirectCommand<'a> {
    direct: &'a Direct,
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

pub struct DirectChild {
    inner: tokio::process::Child,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    program: PathBuf,
    path: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct PathCache {
    map: Mutex<HashMap<CacheKey, PathBuf>>,
}

#[cfg(unix)]
enum UnixXattrTarget<'a> {
    Fd(BorrowedFd<'a>),
    Path(&'a CStr, bool),
}

impl PathCache {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    async fn resolve(
        &self,
        program: &Path,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Option<PathBuf> {
        let key = CacheKey {
            program: program.to_path_buf(),
            path: path.map(|p| p.to_string()),
            cwd: cwd.map(|p| p.to_path_buf()),
        };

        let cached = {
            let map = self.map.lock().await;
            map.get(&key).cloned()
        };

        if let Some(cached) = cached {
            return Some(cached);
        }

        let path_env = path
            .map(|p| p.into())
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_else(|| "".into());

        let program = program.to_path_buf();
        let cwd = cwd.map(|p| p.to_path_buf());

        let resolved = tokio::task::spawn_blocking(move || {
            which::which_in(
                &program,
                Some(path_env),
                cwd.as_deref().unwrap_or(Path::new("")),
            )
            .ok()
        })
        .await
        .unwrap_or(None);

        if let Some(ref resolved_path) = resolved {
            let mut map = self.map.lock().await;
            map.insert(key, resolved_path.clone());
        }

        resolved
    }

    async fn clear(&self) {
        self.map.lock().await.clear();
    }
}

impl Default for Direct {
    fn default() -> Self {
        Self {
            path_cache: Arc::new(PathCache::new()),
        }
    }
}

impl Direct {
    #[cfg(unix)]
    fn statvfs_from_fd(fd: BorrowedFd<'_>) -> io::Result<libc::statvfs> {
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::fstatvfs(fd.as_raw_fd(), stat.as_mut_ptr()) };
        if rc == 0 {
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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
            blocks: Some(stat.f_blocks.into()),
            blocks_free: Some(stat.f_bfree.into()),
            blocks_available: Some(stat.f_bavail.into()),
            files: Some(stat.f_files.into()),
            files_free: Some(stat.f_ffree.into()),
            files_available: Some(stat.f_favail.into()),
            fragment_size: Some(u32::try_from(stat.f_frsize).unwrap_or(u32::MAX)),
            unix_flags: Some(stat.f_flag.into()),
            #[cfg(target_os = "linux")]
            fsid: Some(stat.f_fsid),
            #[cfg(not(target_os = "linux"))]
            fsid: None,
            name_max: Some(u32::try_from(stat.f_namemax).unwrap_or(u32::MAX)),
            win_flags: None,
            volume_serial_number: None,
            component_length_max: None,
        }
    }

    #[cfg(unix)]
    fn fs_metadata_from_file(file: &File) -> io::Result<FsMetadata> {
        Self::statvfs_from_fd(file.as_fd()).map(Self::fs_metadata_from_statvfs)
    }

    #[cfg(unix)]
    fn fs_metadata_from_path(path: &Path, follow: bool) -> io::Result<FsMetadata> {
        if !follow {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fs_metadata follow: false is not implemented on this platform",
            ));
        }
        Self::statvfs_from_path(path).map(Self::fs_metadata_from_statvfs)
    }

    #[cfg(windows)]
    fn final_path_from_handle(handle: BorrowedHandle<'_>) -> io::Result<PathBuf> {
        let mut path = vec![0u16; 32768];
        let len = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                path.as_mut_ptr(),
                32768,
                VOLUME_NAME_DOS,
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = usize::try_from(len).unwrap_or(path.len());
        if len >= path.len() {
            return Err(io::Error::other("path buffer too small"));
        }
        path.truncate(len);

        if path.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]) {
            if path[4..].starts_with(&[b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16]) {
                path = [&[b'\\' as u16, b'\\' as u16][..], &path[8..]].concat();
            } else {
                path.drain(..4);
            }
        }
        Ok(PathBuf::from(OsString::from_wide(&path)))
    }

    #[cfg(windows)]
    fn volume_root_path(path: &Path) -> io::Result<PathBuf> {
        match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                    Ok(PathBuf::from(format!("{}:\\", char::from(drive))))
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    Ok(PathBuf::from(format!(
                        r"\\{}\{}\",
                        server.to_string_lossy(),
                        share.to_string_lossy()
                    )))
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported Windows path prefix",
                )),
            },
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path has no Windows volume prefix",
            )),
        }
    }

    #[cfg(windows)]
    fn fs_query_root_metadata(root: &Path) -> io::Result<(u64, u64, u64, u32, u32, u32)> {
        let root_str = Self::path_wide(root);
        let mut available = 0u64;
        let mut capacity = 0u64;
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(root_str.as_ptr(), &mut available, &mut capacity, &mut free)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let root_handle = Self::open_for_metadata(root, true)?;

        let mut serial = 0u32;
        let mut max_component = 0u32;
        let mut flags = 0u32;
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                root_handle.as_raw_handle(),
                ptr::null_mut(),
                0,
                &mut serial,
                &mut max_component,
                &mut flags,
                ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok((available, capacity, free, serial, max_component, flags))
    }

    #[cfg(windows)]
    fn fs_metadata_from_handle(handle: BorrowedHandle<'_>) -> io::Result<FsMetadata> {
        let root = Self::volume_root_path(&Self::final_path_from_handle(handle)?)?;
        let (available, capacity, free, serial, max_component, flags) =
            Self::fs_query_root_metadata(&root)?;

        Ok(FsMetadata {
            capacity,
            free,
            available,
            block_size: 0,
            blocks: None,
            blocks_free: None,
            blocks_available: None,
            files: None,
            files_free: None,
            files_available: None,
            fragment_size: None,
            unix_flags: None,
            fsid: None,
            name_max: Some(max_component),
            win_flags: Some(flags),
            volume_serial_number: Some(serial),
            component_length_max: Some(max_component),
        })
    }

    #[cfg(windows)]
    fn fs_metadata_from_file(file: &File) -> io::Result<FsMetadata> {
        Self::fs_metadata_from_handle(file.as_handle())
    }

    #[cfg(windows)]
    fn fs_metadata_from_path(path: &Path, follow: bool) -> io::Result<FsMetadata> {
        let root = if follow {
            Self::volume_root_path(&std::fs::canonicalize(path)?)?
        } else {
            Self::volume_root_path(path)?
        };
        let (available, capacity, free, serial, max_component, flags) =
            Self::fs_query_root_metadata(&root)?;

        Ok(FsMetadata {
            capacity,
            free,
            available,
            block_size: 0,
            blocks: None,
            blocks_free: None,
            blocks_available: None,
            files: None,
            files_free: None,
            files_available: None,
            fragment_size: None,
            unix_flags: None,
            fsid: None,
            name_max: Some(max_component),
            win_flags: Some(flags),
            volume_serial_number: Some(serial),
            component_length_max: Some(max_component),
        })
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
    fn attrs_from_path(path: PathBuf, _follow: bool) -> io::Result<Attrs> {
        let file = std::fs::File::open(path)?;
        unsafe { Self::get_linux_flags(file.as_raw_fd()) }.map(Self::attrs_from_flags)
    }

    #[cfg(target_os = "linux")]
    fn set_attrs_path(path: PathBuf, patch: Attrs) -> io::Result<()> {
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
    fn attrs_from_path(path: PathBuf, follow: bool) -> io::Result<Attrs> {
        let stat = if follow {
            nix::sys::stat::stat(&path)
        } else {
            nix::sys::stat::lstat(&path)
        }
        .map_err(io::Error::from)?;
        Ok(Self::attrs_from_flags(stat.st_flags))
    }

    #[cfg(target_os = "macos")]
    fn set_attrs_path(path: PathBuf, patch: Attrs) -> io::Result<()> {
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

    #[cfg(windows)]
    fn path_wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    #[cfg(windows)]
    fn attrs_from_path(path: PathBuf, _follow: bool) -> io::Result<Attrs> {
        let path = Self::path_wide(&path);
        let attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            Err(io::Error::last_os_error())
        } else {
            Ok(Attrs::from_win_attrs(attrs))
        }
    }

    #[cfg(windows)]
    fn set_windows_compression(path: &[u16], compressed: bool) -> io::Result<()> {
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let _handle = unsafe { OwnedHandle::from_raw_handle(handle) };

        let format = if compressed {
            COMPRESSION_FORMAT_DEFAULT
        } else {
            COMPRESSION_FORMAT_NONE
        };
        let mut bytes_returned = 0;
        if unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_COMPRESSION,
                std::ptr::from_ref(&format).cast(),
                u32::try_from(std::mem::size_of_val(&format)).unwrap(),
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(windows)]
    fn open_for_metadata(path: &Path, follow: bool) -> io::Result<File> {
        let path = Self::path_wide(path);
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | flags,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        Ok(File::from_std(StdFile::from(handle)))
    }

    #[cfg(windows)]
    fn set_attrs_path(path: PathBuf, patch: Attrs) -> io::Result<()> {
        if patch.reparse_point.is_some()
            || patch.encrypted.is_some()
            || patch.immutable.is_some()
            || patch.append_only.is_some()
            || patch.no_dump.is_some()
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

        fn apply(attrs: &mut u32, flag: u32, value: Option<bool>) {
            match value {
                Some(true) => *attrs |= flag,
                Some(false) => *attrs &= !flag,
                None => {}
            }
        }

        let path = Self::path_wide(&path);
        let mut attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }

        apply(&mut attrs, FILE_ATTRIBUTE_READONLY, patch.readonly);
        apply(&mut attrs, FILE_ATTRIBUTE_HIDDEN, patch.hidden);
        apply(&mut attrs, FILE_ATTRIBUTE_SYSTEM, patch.system);
        apply(&mut attrs, FILE_ATTRIBUTE_ARCHIVE, patch.archive);
        apply(&mut attrs, FILE_ATTRIBUTE_TEMPORARY, patch.temporary);
        apply(&mut attrs, FILE_ATTRIBUTE_OFFLINE, patch.offline);
        apply(
            &mut attrs,
            FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
            patch.not_content_indexed,
        );

        if patch.readonly.is_some()
            || patch.hidden.is_some()
            || patch.system.is_some()
            || patch.archive.is_some()
            || patch.temporary.is_some()
            || patch.offline.is_some()
            || patch.not_content_indexed.is_some()
        {
            let res = unsafe { SetFileAttributesW(path.as_ptr(), attrs) };
            if res == 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if let Some(compressed) = patch.compressed {
            Self::set_windows_compression(&path, compressed)?;
        }

        Ok(())
    }

    #[cfg(unix)]
    fn override_or_env(env: &HashMap<String, Option<String>>, key: &str) -> Option<OsString> {
        match env.get(key) {
            Some(Some(value)) => Some(OsString::from(value)),
            Some(None) => None,
            None => std::env::var_os(key),
        }
    }

    #[cfg(unix)]
    fn absolute_env_path(
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

    #[cfg(unix)]
    fn home_dir_unix(env: &HashMap<String, Option<String>>) -> Result<PathBuf, io::Error> {
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

    #[cfg(windows)]
    fn known_folder(folder_id: &GUID) -> Result<PathBuf, io::Error> {
        use std::slice;

        unsafe extern "C" {
            fn wcslen(buf: *const u16) -> usize;
        }

        unsafe {
            let mut path = std::ptr::null_mut();
            let result = SHGetKnownFolderPath(
                folder_id,
                KF_FLAG_DONT_VERIFY as u32,
                std::ptr::null_mut(),
                &mut path,
            );
            if result == S_OK {
                let path_slice = slice::from_raw_parts(path, wcslen(path));
                let out = PathBuf::from(OsString::from_wide(path_slice));
                CoTaskMemFree(path.cast());
                Ok(out)
            } else {
                CoTaskMemFree(path.cast());
                Err(io::Error::from_raw_os_error(result))
            }
        }
    }

    #[cfg(windows)]
    fn home_dir_windows(_env: &HashMap<String, Option<String>>) -> Result<PathBuf, io::Error> {
        Self::known_folder(&FOLDERID_Profile)
    }

    fn home_dir_local(env: &HashMap<String, Option<String>>) -> Result<PathBuf, io::Error> {
        #[cfg(unix)]
        {
            Self::home_dir_unix(env)
        }
        #[cfg(windows)]
        {
            Self::home_dir_windows(env)
        }
    }

    fn cache_dir_local(env: &HashMap<String, Option<String>>) -> Result<PathBuf, io::Error> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self::home_dir_local(env)?.join("Library").join("Caches"))
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if let Some(cache) = Self::absolute_env_path(env, "XDG_CACHE_HOME")? {
                Ok(cache)
            } else {
                Ok(Self::home_dir_local(env)?.join(".cache"))
            }
        }
        #[cfg(windows)]
        {
            let _ = env;
            Self::known_folder(&FOLDERID_LocalAppData)
        }
    }
}

impl<'a> DirectCommand<'a> {
    fn new(direct: &'a Direct, program: impl AsRef<Path>) -> Self {
        Self {
            direct,
            program: program.as_ref().to_path_buf(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }
}

impl DirectChild {
    fn new(child: tokio::process::Child) -> Self {
        Self { inner: child }
    }
}

impl Child for DirectChild {
    async fn wait(&mut self) -> Result<ExitStatus, io::Error> {
        self.inner.wait().await
    }

    async fn terminate(self) -> Result<ExitStatus, io::Error> {
        #[cfg(unix)]
        {
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
        #[cfg(not(unix))]
        {
            let mut child = self.inner;
            if child.id().is_some() {
                let _ = child.start_kill();
            }
            child.wait().await
        }
    }
}

impl Command for DirectCommand<'_> {
    type Child = DirectChild;

    fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(arg.to_owned());
        self
    }

    fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.env.insert(key.to_owned(), Some(val.to_owned()));
        self
    }

    fn env_remove(&mut self, key: &str) -> &mut Self {
        self.env.insert(key.to_owned(), None);
        self
    }

    fn current_dir(&mut self, dir: &Path) -> &mut Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    fn stdin_pipe(&mut self, pipe: PipeRecv) -> io::Result<&mut Self> {
        self.stdin = Some(pipe.into_stdio()?);
        Ok(self)
    }

    fn stdout_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stdout = Some(pipe.into_stdio()?);
        Ok(self)
    }

    fn stdin_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdin = Some(Stdio::inherit());
        Ok(self)
    }

    fn stdout_inherit(&mut self) -> io::Result<&mut Self> {
        self.stdout = Some(Stdio::inherit());
        Ok(self)
    }

    #[cfg(unix)]
    fn stdin_fd(&mut self, fd: OwnedFd) -> &mut Self {
        self.stdin = Some(Stdio::from(fd));
        self
    }

    #[cfg(unix)]
    fn stdout_fd(&mut self, fd: OwnedFd) -> &mut Self {
        self.stdout = Some(Stdio::from(fd));
        self
    }

    fn stdin_null(&mut self) -> &mut Self {
        self.stdin = Some(Stdio::null());
        self
    }

    fn stdout_null(&mut self) -> &mut Self {
        self.stdout = Some(Stdio::null());
        self
    }

    fn stderr_pipe(&mut self, pipe: PipeSend) -> io::Result<&mut Self> {
        self.stderr = Some(pipe.into_stdio()?);
        Ok(self)
    }

    fn stderr_inherit(&mut self) -> io::Result<&mut Self> {
        self.stderr = Some(Stdio::inherit());
        Ok(self)
    }

    fn stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        #[cfg(unix)]
        {
            self.stderr = Some(Stdio::from(std::io::stdout().as_fd().try_clone_to_owned()?));
        }
        #[cfg(windows)]
        {
            self.stderr = Some(Stdio::from(
                std::io::stdout().as_handle().try_clone_to_owned()?,
            ));
        }
        Ok(self)
    }

    #[cfg(unix)]
    fn stderr_fd(&mut self, fd: OwnedFd) -> &mut Self {
        self.stderr = Some(Stdio::from(fd));
        self
    }

    fn stderr_null(&mut self) -> &mut Self {
        self.stderr = Some(Stdio::null());
        self
    }

    async fn spawn(self) -> io::Result<Self::Child> {
        let path_override = self
            .env
            .get("PATH")
            .map(|path| path.as_deref().unwrap_or(""));
        let resolved = self
            .direct
            .which(&self.program, path_override, self.cwd.as_deref())
            .await?;
        let resolved = resolved.ok_or_else(|| {
            #[cfg(unix)]
            {
                io::Error::from_raw_os_error(libc::ENOENT)
            }
            #[cfg(not(unix))]
            {
                io::Error::from(io::ErrorKind::NotFound)
            }
        })?;

        let mut command = TokioCommand::new(&resolved);
        command.args(&self.args);

        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }

        for (k, v) in self.env {
            match v {
                Some(val) => {
                    command.env(k, val);
                }
                None => {
                    command.env_remove(k);
                }
            }
        }

        if let Some(stdin) = self.stdin {
            command.stdin(stdin);
        }
        if let Some(stdout) = self.stdout {
            command.stdout(stdout);
        }
        if let Some(stderr) = self.stderr {
            command.stderr(stderr);
        }

        command.spawn().map(DirectChild::new)
    }
}

impl DirectOpenOptions {
    fn as_tokio(&self) -> OpenOptions {
        let mut opts = OpenOptions::new();
        opts.read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
        #[cfg(unix)]
        if self.no_follow {
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            if self.no_follow {
                opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            }
        }
        opts
    }
}

impl crate::OpenOptions for DirectOpenOptions {
    fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
        self
    }

    async fn open(&self, path: impl AsRef<Path>) -> Result<File, io::Error> {
        self.as_tokio().open(path).await
    }
}

impl Direct {
    #[cfg(unix)]
    fn unix_xattr_namespace(namespace: XattrNamespace<'_>) -> io::Result<Option<Vec<u8>>> {
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

    #[cfg(unix)]
    fn xattr_path(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    }

    #[cfg(unix)]
    fn xattr_name(name: &str, namespace: Option<&str>) -> io::Result<CString> {
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    fn unix_list_xattrs(
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

    #[cfg(unix)]
    fn unix_get_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> io::Result<Vec<u8>> {
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

    #[cfg(unix)]
    fn unix_set_xattr(target: UnixXattrTarget<'_>, name: &CStr, value: &[u8]) -> io::Result<()> {
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

    #[cfg(unix)]
    fn unix_remove_xattr(target: UnixXattrTarget<'_>, name: &CStr) -> io::Result<()> {
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

    #[cfg(windows)]
    fn nt_error(status: windows_sys::Win32::Foundation::NTSTATUS) -> io::Error {
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
    }

    #[cfg(windows)]
    fn windows_xattr_name(name: &str, namespace: Option<&str>) -> io::Result<Vec<u8>> {
        if namespace.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xattr namespaces are not supported on this platform",
            ));
        }
        if name.as_bytes().contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xattr name contains NUL",
            ));
        }
        let name = name.as_bytes().to_vec();
        let Ok(_len) = u8::try_from(name.len()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xattr name is too long",
            ));
        };
        Ok(name)
    }

    #[cfg(windows)]
    const fn align_windows_ea(len: usize) -> usize {
        (len + 3) & !3
    }

    #[cfg(windows)]
    fn windows_get_ea_list(name: &[u8]) -> io::Result<Vec<u8>> {
        let len =
            usize::from(u8::try_from(name.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "xattr name is too long")
            })?);
        let size =
            Self::align_windows_ea(std::mem::offset_of!(FILE_GET_EA_INFORMATION, EaName) + len + 1);
        let mut buf = vec![0u8; size];
        let entry = buf.as_mut_ptr().cast::<FILE_GET_EA_INFORMATION>();
        unsafe {
            (*entry).NextEntryOffset = 0;
            (*entry).EaNameLength = len as u8;
            ptr::copy_nonoverlapping(
                name.as_ptr(),
                (*entry).EaName.as_mut_ptr().cast::<u8>(),
                len,
            );
        }
        Ok(buf)
    }

    #[cfg(windows)]
    fn windows_full_ea(name: &[u8], value: &[u8]) -> io::Result<Vec<u8>> {
        let name_len =
            usize::from(u8::try_from(name.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "xattr name is too long")
            })?);
        let value_len = usize::from(u16::try_from(value.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "xattr value is too large")
        })?);
        let size = Self::align_windows_ea(
            std::mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName) + name_len + 1 + value_len,
        );
        let mut buf = vec![0u8; size];
        let entry = buf.as_mut_ptr().cast::<FILE_FULL_EA_INFORMATION>();
        unsafe {
            (*entry).NextEntryOffset = 0;
            (*entry).Flags = 0;
            (*entry).EaNameLength = name_len as u8;
            (*entry).EaValueLength = value_len as u16;
            let name_ptr = (*entry).EaName.as_mut_ptr().cast::<u8>();
            ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, name_len);
            ptr::copy_nonoverlapping(value.as_ptr(), name_ptr.add(name_len + 1), value_len);
        }
        Ok(buf)
    }

    #[cfg(windows)]
    fn windows_parse_full_ea_chunk(buf: &[u8]) -> io::Result<Vec<XattrEntry>> {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while offset < buf.len() {
            let remaining = &buf[offset..];
            if remaining.len() < std::mem::size_of::<FILE_FULL_EA_INFORMATION>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "EA buffer truncated",
                ));
            }
            let entry = unsafe { &*remaining.as_ptr().cast::<FILE_FULL_EA_INFORMATION>() };
            let name_len = usize::from(entry.EaNameLength);
            let value_len = usize::from(entry.EaValueLength);
            let name_offset = std::mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName);
            let total_len = name_offset
                .checked_add(name_len)
                .and_then(|v| v.checked_add(1))
                .and_then(|v| v.checked_add(value_len))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "EA buffer overflow"))?;
            if total_len > remaining.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "EA entry truncated",
                ));
            }
            let name = unsafe {
                slice::from_raw_parts(entry.EaName.as_ptr().cast::<u8>(), name_len).to_vec()
            };
            entries.push(XattrEntry {
                name: String::from_utf8(name).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "xattr name is not UTF-8")
                })?,
                namespace: None,
                size: Some(value_len as u64),
                flags: Some(entry.Flags),
            });
            if entry.NextEntryOffset == 0 {
                break;
            }
            let next = usize::try_from(entry.NextEntryOffset).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid EA entry offset")
            })?;
            if next > remaining.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid EA entry offset",
                ));
            }
            offset += next;
        }
        Ok(entries)
    }

    #[cfg(windows)]
    fn windows_parse_full_ea_value(buf: &[u8]) -> io::Result<(String, Vec<u8>)> {
        if buf.len() < mem::size_of::<FILE_FULL_EA_INFORMATION>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "EA buffer truncated",
            ));
        }
        let entry = unsafe { &*buf.as_ptr().cast::<FILE_FULL_EA_INFORMATION>() };
        let name_len = usize::from(entry.EaNameLength);
        let value_len = usize::from(entry.EaValueLength);
        let name_offset = mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName);
        let value_offset = mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName) + name_len + 1;
        let end = value_offset
            .checked_add(value_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "EA buffer overflow"))?;
        if end > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "EA entry truncated",
            ));
        }
        let name = String::from_utf8(buf[name_offset..name_offset + name_len].to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "xattr name is not UTF-8"))?;
        Ok((name, buf[value_offset..end].to_vec()))
    }

    #[cfg(windows)]
    unsafe fn windows_list_xattrs(handle: BorrowedHandle<'_>) -> io::Result<Vec<XattrEntry>> {
        let handle = handle.as_raw_handle();
        let mut entries = Vec::new();
        let mut restart_scan = true;
        let mut buf = vec![0u8; 4096];
        loop {
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = unsafe {
                NtQueryEaFile(
                    handle,
                    &mut iosb,
                    buf.as_mut_ptr().cast(),
                    buf.len().try_into().unwrap_or(u32::MAX),
                    false,
                    ptr::null(),
                    0,
                    ptr::null(),
                    restart_scan,
                )
            };
            match status {
                STATUS_SUCCESS => {
                    let len = iosb.Information;
                    if len == 0 {
                        return Ok(entries);
                    }
                    entries.extend(Self::windows_parse_full_ea_chunk(&buf[..len])?);
                    return Ok(entries);
                }
                STATUS_BUFFER_OVERFLOW => {
                    let len = iosb.Information;
                    if len == 0 {
                        buf.resize(buf.len() * 2, 0);
                        continue;
                    }
                    entries.extend(Self::windows_parse_full_ea_chunk(&buf[..len])?);
                    restart_scan = false;
                }
                STATUS_BUFFER_TOO_SMALL => {
                    buf.resize(buf.len() * 2, 0);
                }
                STATUS_NO_EAS_ON_FILE | STATUS_NO_MORE_EAS => return Ok(entries),
                _ => return Err(Self::nt_error(status)),
            }
        }
    }

    #[cfg(windows)]
    unsafe fn windows_get_xattr(handle: BorrowedHandle<'_>, name: &[u8]) -> io::Result<Vec<u8>> {
        let handle = handle.as_raw_handle();
        let ea_list = Self::windows_get_ea_list(name)?;
        let mut buf = vec![0u8; 256];
        loop {
            let mut iosb = IO_STATUS_BLOCK::default();
            let status = unsafe {
                NtQueryEaFile(
                    handle,
                    &mut iosb,
                    buf.as_mut_ptr().cast(),
                    buf.len().try_into().unwrap_or(u32::MAX),
                    true,
                    ea_list.as_ptr().cast(),
                    ea_list.len().try_into().unwrap_or(u32::MAX),
                    ptr::null(),
                    true,
                )
            };
            match status {
                STATUS_SUCCESS => {
                    let (found_name, value) =
                        Self::windows_parse_full_ea_value(&buf[..iosb.Information])?;
                    if value.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("xattr {found_name:?} not found"),
                        ));
                    }
                    return Ok(value);
                }
                STATUS_BUFFER_OVERFLOW | STATUS_BUFFER_TOO_SMALL => {
                    let next_len = std::cmp::max(buf.len() * 2, iosb.Information.saturating_add(1));
                    buf.resize(next_len, 0);
                }
                _ => return Err(Self::nt_error(status)),
            }
        }
    }

    #[cfg(windows)]
    unsafe fn windows_set_xattr(
        handle: BorrowedHandle<'_>,
        name: &[u8],
        value: &[u8],
    ) -> io::Result<()> {
        let handle = handle.as_raw_handle();
        let ea = Self::windows_full_ea(name, value)?;
        let mut iosb = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetEaFile(
                handle,
                &mut iosb,
                ea.as_ptr().cast(),
                ea.len().try_into().unwrap_or(u32::MAX),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Self::nt_error(status))
        }
    }

    #[cfg(windows)]
    fn windows_parse_stream_name(name: &str) -> io::Result<(String, String)> {
        let rest = name.strip_prefix(':').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "stream name missing `:` prefix")
        })?;
        let split = rest.rfind(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream name missing type suffix",
            )
        })?;
        let stream_type = rest[split + 1..].strip_prefix('$').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "stream type missing `$` prefix")
        })?;
        Ok((rest[..split].to_owned(), stream_type.to_owned()))
    }

    #[cfg(windows)]
    fn windows_parse_streams(buf: &[u8]) -> io::Result<Vec<StreamEntry>> {
        let mut streams = Vec::new();
        let mut offset = 0usize;
        while offset < buf.len() {
            if buf.len() - offset < mem::size_of::<FILE_STREAM_INFO>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated FILE_STREAM_INFO entry",
                ));
            }
            let info = unsafe { &*buf[offset..].as_ptr().cast::<FILE_STREAM_INFO>() };
            let name_len = usize::try_from(info.StreamNameLength)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stream name too large"))?;
            if name_len % 2 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid stream name length",
                ));
            }
            let name_slice =
                unsafe { std::slice::from_raw_parts(info.StreamName.as_ptr(), name_len / 2) };
            let raw_name = String::from_utf16(name_slice).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "stream name is not UTF-16")
            })?;
            let (name, r#type) = Self::windows_parse_stream_name(&raw_name)?;
            let size = u64::try_from(info.StreamSize).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "stream size out of range")
            })?;
            let alloc_size = u64::try_from(info.StreamAllocationSize).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream allocation size out of range",
                )
            })?;
            streams.push(StreamEntry {
                name,
                r#type,
                size,
                alloc_size,
            });

            let next = usize::try_from(info.NextEntryOffset).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream entry offset out of range",
                )
            })?;
            if next == 0 {
                break;
            }
            offset = offset.checked_add(next).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "stream entry offset overflow")
            })?;
        }
        Ok(streams)
    }

    #[cfg(windows)]
    unsafe fn windows_list_streams(handle: BorrowedHandle<'_>) -> io::Result<Vec<StreamEntry>> {
        let handle = handle.as_raw_handle();
        let mut len = 4096usize;
        loop {
            let mut buf = vec![0u8; len];
            let status = unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FileStreamInfo,
                    buf.as_mut_ptr().cast(),
                    u32::try_from(buf.len()).unwrap_or(u32::MAX),
                )
            };
            if status != 0 {
                return Self::windows_parse_streams(&buf);
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_MORE_DATA as i32) {
                len = len.saturating_mul(2);
                continue;
            }
            if err.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                return Ok(Vec::new());
            }
            return Err(err);
        }
    }

    fn directory_requires_all_error() -> io::Error {
        #[cfg(unix)]
        {
            io::Error::from_raw_os_error(libc::EISDIR)
        }
        #[cfg(not(unix))]
        {
            io::Error::new(
                io::ErrorKind::IsADirectory,
                "directory operations require all: true",
            )
        }
    }

    fn directory_not_empty_error() -> io::Error {
        #[cfg(unix)]
        {
            io::Error::from_raw_os_error(libc::ENOTEMPTY)
        }
        #[cfg(not(unix))]
        {
            io::Error::from(io::ErrorKind::DirectoryNotEmpty)
        }
    }

    fn not_a_directory_error() -> io::Error {
        #[cfg(unix)]
        {
            io::Error::from_raw_os_error(libc::ENOTDIR)
        }
        #[cfg(not(unix))]
        {
            io::Error::from(io::ErrorKind::NotADirectory)
        }
    }

    #[cfg(unix)]
    async fn chown_local(
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

    #[cfg(unix)]
    async fn create_symlink(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink(src, dst).await
    }

    #[cfg(windows)]
    async fn create_symlink(src: &Path, dst: &Path) -> io::Result<()> {
        let metadata = fs::metadata(src).await?;
        if metadata.is_dir() {
            Self::create_symlink_dir(src, dst).await
        } else {
            Self::create_symlink_file(src, dst).await
        }
    }

    #[cfg(unix)]
    async fn create_symlink_dir(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink(src, dst).await
    }

    #[cfg(windows)]
    async fn create_symlink_dir(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink_dir(src, dst).await
    }

    #[cfg(unix)]
    async fn create_symlink_file(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink(src, dst).await
    }

    #[cfg(windows)]
    async fn create_symlink_file(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink_file(src, dst).await
    }

    async fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
        let target = fs::read_link(src).await?;
        Self::create_symlink(&target, dst).await
    }

    async fn copy_local(from: &Path, to: &Path, all: bool) -> io::Result<()> {
        let metadata = fs::symlink_metadata(from).await?;

        if metadata.is_dir() {
            if !all {
                return Err(Self::directory_requires_all_error());
            }

            fs::create_dir(to).await?;
            let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
            while let Some((src_dir, dst_dir)) = stack.pop() {
                let mut entries = fs::read_dir(&src_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let src_path = entry.path();
                    let dst_path = dst_dir.join(entry.file_name());
                    let metadata = fs::symlink_metadata(&src_path).await?;
                    if metadata.is_dir() {
                        fs::create_dir(&dst_path).await?;
                        stack.push((src_path, dst_path));
                    } else if metadata.is_file() {
                        fs::copy(&src_path, &dst_path).await?;
                    } else if metadata.file_type().is_symlink() {
                        Self::copy_symlink(&src_path, &dst_path).await?;
                    } else {
                        return Err(io::Error::other("unsupported file type"));
                    }
                }
            }
        } else if metadata.is_file() {
            fs::copy(from, to).await?;
        } else if metadata.file_type().is_symlink() {
            Self::copy_symlink(from, to).await?;
        } else {
            return Err(io::Error::other("unsupported file type"));
        }

        Ok(())
    }

    async fn move_local(from: &Path, to: &Path, all: bool) -> io::Result<()> {
        let metadata = fs::symlink_metadata(from).await?;
        let is_dir = metadata.is_dir();

        if is_dir && !all {
            return Err(Self::directory_requires_all_error());
        }

        match fs::rename(from, to).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                Self::copy_local(from, to, all).await?;
                if is_dir {
                    fs::remove_dir_all(from).await
                } else {
                    fs::remove_file(from).await
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn read_dir_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut read_dir = fs::read_dir(path).await?;
        let mut paths = Vec::new();
        while let Some(entry) = read_dir.next_entry().await? {
            paths.push(entry.path());
        }
        Ok(paths)
    }

    async fn remove_dir_empty_tree_local(path: &Path, ignore: bool) -> io::Result<bool> {
        let metadata = fs::symlink_metadata(path).await?;
        if !metadata.is_dir() {
            return Err(Self::not_a_directory_error());
        }

        struct Frame {
            path: PathBuf,
            entries: Vec<PathBuf>,
            next: usize,
            removable: bool,
        }

        let mut stack = vec![Frame {
            path: path.to_owned(),
            entries: Self::read_dir_paths(path).await?,
            next: 0,
            removable: true,
        }];
        let mut last_result = None;

        while let Some(frame) = stack.last_mut() {
            if let Some(child_removed) = last_result.take() {
                frame.removable &= child_removed;
            }

            if frame.next == frame.entries.len() {
                let removable = frame.removable;
                let path = frame.path.clone();
                stack.pop();
                if removable {
                    fs::remove_dir(&path).await?;
                }
                last_result = Some(removable);
                continue;
            }

            let child_path = frame.entries[frame.next].clone();
            frame.next += 1;
            let metadata = fs::symlink_metadata(&child_path).await?;
            if metadata.is_dir() {
                stack.push(Frame {
                    path: child_path.clone(),
                    entries: Self::read_dir_paths(&child_path).await?,
                    next: 0,
                    removable: true,
                });
            } else if ignore {
                frame.removable = false;
            } else {
                return Err(Self::directory_not_empty_error());
            }
        }

        Ok(last_result.unwrap_or(false))
    }
}

impl Vfs for Direct {
    type OpenOptions<'a>
        = DirectOpenOptions
    where
        Self: 'a;
    type Command<'a>
        = DirectCommand<'a>
    where
        Self: 'a;

    fn open_options(&self) -> Self::OpenOptions<'_> {
        DirectOpenOptions::default()
    }

    fn command(&self, program: impl AsRef<Path>) -> Self::Command<'_> {
        DirectCommand::new(self, program)
    }

    async fn read_dir(&self, path: impl AsRef<Path>) -> Result<ReadDir, io::Error> {
        ReadDir::open(path.as_ref()).await
    }

    async fn which(
        &self,
        program: impl AsRef<Path>,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<PathBuf>, io::Error> {
        Ok(self.path_cache.resolve(program.as_ref(), path, cwd).await)
    }

    async fn well_known_path(
        &self,
        key: WellKnownPath,
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        match key {
            WellKnownPath::HomeDir => Self::home_dir_local(env),
            WellKnownPath::CacheDir => Self::cache_dir_local(env),
        }
    }

    async fn clear_cache(&self) -> Result<(), io::Error> {
        self.path_cache.clear().await;
        Ok(())
    }

    #[cfg(unix)]
    async fn xattrs(
        &self,
        path: impl AsRef<Path>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        let path = Self::xattr_path(path.as_ref())?;
        let namespace = Self::unix_xattr_namespace(namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_list_xattrs(UnixXattrTarget::Path(&path, follow), namespace)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    #[cfg(windows)]
    async fn xattrs(
        &self,
        path: impl AsRef<Path>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        let file = self
            .open_options()
            .read(true)
            .no_follow(!follow)
            .open(path.as_ref())
            .await?;
        self.file_xattrs(&file, namespace).await
    }

    #[cfg(unix)]
    async fn streams(
        &self,
        _path: impl AsRef<Path>,
        _follow: bool,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    #[cfg(windows)]
    async fn streams(
        &self,
        path: impl AsRef<Path>,
        follow: bool,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        let file = Self::open_for_metadata(path.as_ref(), follow)?;
        self.file_streams(&file).await
    }

    #[cfg(not(any(unix, windows)))]
    async fn streams(
        &self,
        _path: impl AsRef<Path>,
        _follow: bool,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    #[cfg(unix)]
    async fn xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>, io::Error> {
        let path = Self::xattr_path(path.as_ref())?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_get_xattr(UnixXattrTarget::Path(&path, follow), &name)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    #[cfg(windows)]
    async fn xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>, io::Error> {
        let file = self
            .open_options()
            .read(true)
            .no_follow(!follow)
            .open(path.as_ref())
            .await?;
        self.file_xattr(&file, name, namespace).await
    }

    #[cfg(unix)]
    async fn set_xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<(), io::Error> {
        let path = Self::xattr_path(path.as_ref())?;
        let name = Self::xattr_name(name, namespace)?;
        let value = value.to_vec();
        tokio::task::spawn_blocking(move || {
            Self::unix_set_xattr(UnixXattrTarget::Path(&path, follow), &name, &value)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    #[cfg(windows)]
    async fn set_xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<(), io::Error> {
        let file = self
            .open_options()
            .write(true)
            .no_follow(!follow)
            .open(path.as_ref())
            .await?;
        self.file_set_xattr(&file, name, namespace, value).await
    }

    #[cfg(unix)]
    async fn remove_xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<(), io::Error> {
        let path = Self::xattr_path(path.as_ref())?;
        let name = Self::xattr_name(name, namespace)?;
        tokio::task::spawn_blocking(move || {
            Self::unix_remove_xattr(UnixXattrTarget::Path(&path, follow), &name)
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    #[cfg(windows)]
    async fn remove_xattr(
        &self,
        path: impl AsRef<Path>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<(), io::Error> {
        let file = self
            .open_options()
            .read(true)
            .write(true)
            .no_follow(!follow)
            .open(path.as_ref())
            .await?;
        self.file_remove_xattr(&file, name, namespace).await
    }

    #[cfg(unix)]
    async fn file_xattrs(
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

    #[cfg(windows)]
    async fn file_xattrs(
        &self,
        file: &File,
        namespace: XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        if let XattrNamespace::Named(_) = namespace {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xattr namespaces are not supported on this platform",
            ));
        }
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || unsafe { Self::windows_list_xattrs(file.as_handle()) })
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    #[cfg(not(any(unix, windows)))]
    async fn file_xattrs(
        &self,
        _file: &File,
        _namespace: XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "xattrs are not supported on this platform",
        ))
    }

    #[cfg(unix)]
    async fn file_xattr(
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

    #[cfg(windows)]
    async fn file_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<u8>, io::Error> {
        let name = Self::windows_xattr_name(name, namespace)?;
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || unsafe {
            Self::windows_get_xattr(file.as_handle(), &name)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    #[cfg(not(any(unix, windows)))]
    async fn file_xattr(
        &self,
        _file: &File,
        _name: &str,
        _namespace: Option<&str>,
    ) -> Result<Vec<u8>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "xattrs are not supported on this platform",
        ))
    }

    #[cfg(windows)]
    async fn file_streams(&self, file: &File) -> Result<Vec<StreamEntry>, io::Error> {
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || unsafe { Self::windows_list_streams(file.as_handle()) })
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    #[cfg(not(windows))]
    async fn file_streams(&self, _file: &File) -> Result<Vec<StreamEntry>, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streams are not supported on this platform",
        ))
    }

    #[cfg(unix)]
    async fn file_set_xattr(
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

    #[cfg(windows)]
    async fn file_set_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<(), io::Error> {
        if value.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty xattr values are not supported on this platform",
            ));
        }
        let name = Self::windows_xattr_name(name, namespace)?;
        let value = value.to_vec();
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || unsafe {
            Self::windows_set_xattr(file.as_handle(), &name, &value)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    #[cfg(not(any(unix, windows)))]
    async fn file_set_xattr(
        &self,
        _file: &File,
        _name: &str,
        _namespace: Option<&str>,
        _value: &[u8],
    ) -> Result<(), io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "xattrs are not supported on this platform",
        ))
    }

    #[cfg(unix)]
    async fn file_remove_xattr(
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

    #[cfg(windows)]
    async fn file_remove_xattr(
        &self,
        file: &File,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<(), io::Error> {
        let name = Self::windows_xattr_name(name, namespace)?;
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || unsafe {
            Self::windows_set_xattr(file.as_handle(), &name, &[])
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    #[cfg(not(any(unix, windows)))]
    async fn file_remove_xattr(
        &self,
        _file: &File,
        _name: &str,
        _namespace: Option<&str>,
    ) -> Result<(), io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "xattrs are not supported on this platform",
        ))
    }

    async fn remove(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        let path = path.as_ref();
        let result = if all {
            match fs::symlink_metadata(path).await {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).await,
                Ok(_) => fs::remove_file(path).await,
                Err(e) => Err(e),
            }
        } else {
            fs::remove_file(path).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if ignore && e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
        fs::metadata(path.as_ref())
            .await
            .map(crate::metadata_from_std)
    }

    async fn file_fs_metadata(&self, file: &File) -> Result<FsMetadata, io::Error> {
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || Self::fs_metadata_from_file(&file))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join fs metadata query task")))
    }

    async fn fs_metadata(
        &self,
        path: impl AsRef<Path>,
        follow: bool,
    ) -> Result<FsMetadata, io::Error> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::fs_metadata_from_path(&path, follow))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join fs metadata query task")))
    }

    async fn create_dir(&self, path: impl AsRef<Path>, all: bool) -> Result<(), io::Error> {
        if all {
            fs::create_dir_all(path.as_ref()).await
        } else {
            fs::create_dir(path.as_ref()).await
        }
    }

    async fn remove_dir(
        &self,
        path: impl AsRef<Path>,
        all: bool,
        ignore: bool,
    ) -> Result<(), io::Error> {
        let result = if all {
            Self::remove_dir_empty_tree_local(path.as_ref(), ignore)
                .await
                .map(|_| ())
        } else {
            fs::remove_dir(path.as_ref()).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if ignore && e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn copy(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error> {
        Self::copy_local(from.as_ref(), to.as_ref(), all).await
    }

    async fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), io::Error> {
        fs::rename(from.as_ref(), to.as_ref()).await
    }

    async fn move_(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        all: bool,
    ) -> Result<(), io::Error> {
        Self::move_local(from.as_ref(), to.as_ref(), all).await
    }

    async fn symlink(&self, src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<(), io::Error> {
        Self::create_symlink(src.as_ref(), dst.as_ref()).await
    }

    async fn hard_link(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        fs::hard_link(src.as_ref(), dst.as_ref()).await
    }

    async fn symlink_dir(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        Self::create_symlink_dir(src.as_ref(), dst.as_ref()).await
    }

    async fn symlink_file(
        &self,
        src: impl AsRef<Path>,
        dst: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        Self::create_symlink_file(src.as_ref(), dst.as_ref()).await
    }

    async fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata, io::Error> {
        fs::symlink_metadata(path.as_ref())
            .await
            .map(crate::metadata_from_std)
    }

    async fn attrs(&self, path: impl AsRef<Path>, follow: bool) -> Result<Attrs, io::Error> {
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        {
            let path = path.as_ref().to_path_buf();
            tokio::task::spawn_blocking(move || Self::attrs_from_path(path, follow))
                .await
                .unwrap_or_else(|_| Err(io::Error::other("failed to join attrs query task")))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = (path, follow);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "attrs is not supported on this platform",
            ))
        }
    }

    async fn set_attrs(&self, path: impl AsRef<Path>, attrs: Attrs) -> Result<(), io::Error> {
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        {
            let path = path.as_ref().to_path_buf();
            tokio::task::spawn_blocking(move || Self::set_attrs_path(path, attrs))
                .await
                .unwrap_or_else(|_| Err(io::Error::other("failed to join attrs update task")))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = (path, attrs);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "set_attrs is not supported on this platform",
            ))
        }
    }

    async fn canonicalize(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
        #[cfg(target_os = "windows")]
        {
            let path = path.as_ref().to_path_buf();
            tokio::task::spawn_blocking(move || dunce::canonicalize(path))
                .await
                .unwrap_or_else(|e| Err(io::Error::other(e)))
        }

        #[cfg(not(target_os = "windows"))]
        {
            fs::canonicalize(path.as_ref()).await
        }
    }

    async fn read_link(&self, path: impl AsRef<Path>) -> Result<PathBuf, io::Error> {
        fs::read_link(path.as_ref()).await
    }

    async fn glob(
        &self,
        pattern: impl Into<String>,
        root: &Path,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<PathBuf>, io::Error> {
        let pattern = pattern.into();
        let root = root.to_owned();
        tokio::task::spawn_blocking(move || {
            let (prefix, glob) = Glob::new(&pattern)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid glob pattern"))?
                .partition();
            let walk_root = root.join(&prefix);

            let mut behavior = WalkBehavior::default();
            if follow_symlinks {
                behavior.link = LinkBehavior::ReadTarget;
            }
            if let Some(depth) = max_depth {
                behavior.depth =
                    DepthBehavior::Max(DepthMax(depth.saturating_sub(prefix.components().count())));
            }

            let mut paths = Vec::new();
            let walk = match glob {
                Some(g) => g.walk_with_behavior(&walk_root, behavior),
                None => Glob::tree().walk_with_behavior(&walk_root, behavior),
            };

            for entry in walk {
                paths.push(prefix.join(entry?.root_relative_paths().1));
            }

            paths.sort();
            Ok(paths)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    async fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        perm: Permissions,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(perm.mode())).await
        }

        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(path.as_ref()).await?.permissions();
            permissions.set_readonly(perm.readonly());
            fs::set_permissions(path.as_ref(), permissions).await
        }
    }

    async fn set_times(
        &self,
        path: impl AsRef<Path>,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
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

            let path = path.as_ref().to_path_buf();
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

        #[cfg(windows)]
        {
            use std::{
                fs::{FileTimes, OpenOptions as StdOpenOptions},
                os::windows::fs::{FileTimesExt, OpenOptionsExt},
            };
            use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

            let path = path.as_ref().to_path_buf();
            tokio::task::spawn_blocking(move || {
                let file = StdOpenOptions::new()
                    .access_mode(FILE_WRITE_ATTRIBUTES)
                    .open(path)?;
                let mut times = FileTimes::new();
                if let Some(accessed) = parts_to_system_time(accessed) {
                    times = times.set_accessed(accessed);
                }
                if let Some(modified) = parts_to_system_time(modified) {
                    times = times.set_modified(modified);
                }
                if let Some(created) = parts_to_system_time(created) {
                    times = times.set_created(created);
                }
                file.set_times(times)
            })
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)))
        }
    }

    async fn chown(
        &self,
        path: impl AsRef<Path>,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            Self::chown_local(path.as_ref().to_path_buf(), user, group, follow).await
        }

        #[cfg(not(unix))]
        {
            let _ = (path, user, group, follow);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "chown is not supported on this platform",
            ))
        }
    }
}

#[cfg(windows)]
fn parts_to_system_time(parts: Option<(i64, u32)>) -> Option<SystemTime> {
    let (secs, nanos) = parts?;
    if secs >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs as u64, nanos))
    } else {
        let secs_abs = secs.unsigned_abs();
        let duration = if nanos == 0 {
            Duration::new(secs_abs, 0)
        } else {
            Duration::new(secs_abs - 1, 1_000_000_000u32 - nanos)
        };
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    }
}
