use super::{
    Child, Command, Direct, File, OpenOptions,
    win_dir::{self, DirIter},
};
use crate::{
    directory::{DirEntry, DirEntryFamily},
    error::{Error, ErrorKind, Result},
    file::{AccessFlags, FileId, ShareMode, StreamEntry},
    file::{XattrEntry, XattrNamespace},
    metadata::{
        AttrFlags, AttrsPatch, FsMetadata, FsMetadataFamily, Metadata, MetadataPatch, Resolve,
        WindowsFsMetadata, metadata_from_std, metadata_with_reparse_tag, metadata_with_sids,
    },
    security::{Acl, AclKind, OwnershipIdentity, SecurityInfo, SidName, SidNameUse},
};
use dolang_winterop::security::{SecDesc, SecDescControl, SecInfo, Sid, WellKnownSid};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{File as StdFile, OpenOptions as StdOpenOptions},
    mem,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr, slice,
    sync::Arc,
    time::SystemTime,
};
use tokio::{
    fs,
    time::{Duration, timeout},
};
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_FULL_EA_INFORMATION, FILE_GET_EA_INFORMATION, FILE_RENAME_POSIX_SEMANTICS,
        FILE_RENAME_REPLACE_IF_EXISTS, IO_REPARSE_TAG_LX_SYMLINK, NtQueryEaFile, NtSetEaFile,
        SYMLINK_FLAG_RELATIVE,
    },
    Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_CANT_ACCESS_FILE, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF,
            ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
            ERROR_INVALID_REPARSE_DATA, ERROR_MORE_DATA, ERROR_NONE_MAPPED,
            ERROR_NOT_A_REPARSE_POINT, ERROR_NOT_SUPPORTED, GENERIC_READ, GENERIC_WRITE,
            GetLastError, INVALID_HANDLE_VALUE, LocalFree, MAX_PATH, RtlNtStatusToDosError, S_OK,
            STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_NO_EAS_ON_FILE,
            STATUS_NO_MORE_EAS, STATUS_SUCCESS,
        },
        Security::{
            ACL,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
            GetSecurityDescriptorLength, LookupAccountNameW, LookupAccountSidW,
            PROTECTED_DACL_SECURITY_INFORMATION, PROTECTED_SACL_SECURITY_INFORMATION,
            SECURITY_ATTRIBUTES, SidTypeAlias as SID_TYPE_ALIAS,
            SidTypeComputer as SID_TYPE_COMPUTER,
            SidTypeDeletedAccount as SID_TYPE_DELETED_ACCOUNT, SidTypeDomain as SID_TYPE_DOMAIN,
            SidTypeGroup as SID_TYPE_GROUP, SidTypeInvalid as SID_TYPE_INVALID,
            SidTypeLabel as SID_TYPE_LABEL, SidTypeLogonSession as SID_TYPE_LOGON_SESSION,
            SidTypeUnknown as SID_TYPE_UNKNOWN, SidTypeUser as SID_TYPE_USER,
            SidTypeWellKnownGroup as SID_TYPE_WELL_KNOWN_GROUP,
            UNPROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_SACL_SECURITY_INFORMATION,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, COMPRESSION_FORMAT_DEFAULT, COMPRESSION_FORMAT_NONE,
            CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN,
            FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM,
            FILE_ATTRIBUTE_TAG_INFO, FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_RENAME_INFO, FILE_RENAME_INFO_0,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STREAM_INFO,
            FILE_WRITE_DATA, FileAttributeTagInfo, FileRenameInfo, FileRenameInfoEx,
            FileStreamInfo, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetFileAttributesW,
            GetFileInformationByHandle, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
            GetVolumeInformationByHandleW, INVALID_FILE_ATTRIBUTES,
            MAXIMUM_REPARSE_DATA_BUFFER_SIZE, OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL,
            SetFileAttributesW, SetFileInformationByHandle, TRUNCATE_EXISTING, VOLUME_NAME_DOS,
            WRITE_DAC, WRITE_OWNER,
        },
        System::{
            Com::CoTaskMemFree,
            Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent},
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            IO::{DeviceIoControl, IO_STATUS_BLOCK},
            Ioctl::{
                FILE_SET_SPARSE_BUFFER, FSCTL_DELETE_REPARSE_POINT, FSCTL_GET_REPARSE_POINT,
                FSCTL_SET_COMPRESSION, FSCTL_SET_REPARSE_POINT, FSCTL_SET_SPARSE,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JobObjectBasicAccountingInformation, QueryInformationJobObject, TerminateJobObject,
            },
            SystemServices::{
                ACCESS_SYSTEM_SECURITY, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
            },
            Threading::{
                CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenThread, ResumeThread,
                THREAD_SUSPEND_RESUME,
            },
        },
        UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData, FOLDERID_Windows,
            KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
        },
    },
    core::GUID,
};

#[derive(Debug)]
pub(crate) struct ReadDir {
    /// The listing, or `None` once it is exhausted.
    iter: Option<Box<DirIter>>,
    pending: std::collections::VecDeque<DirEntry>,
}

impl ReadDir {
    /// Entries read per trip to the blocking pool.
    const BATCH: usize = 128;

    pub(super) async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let handle = tokio::task::spawn_blocking(move || win_dir::open_dir(&path, false, true))
            .await
            .map_err(Error::other)??;
        Ok(Self {
            iter: Some(Box::new(DirIter::new(handle))),
            pending: std::collections::VecDeque::new(),
        })
    }

    pub(crate) async fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        if self.pending.is_empty()
            && let Some(mut iter) = self.iter.take()
        {
            let (iter, entries) = tokio::task::spawn_blocking(move || {
                let mut entries = Vec::new();
                while entries.len() < Self::BATCH {
                    match iter.next_entry()? {
                        Some(entry) => entries.push(entry),
                        None => return Ok::<_, Error>((None, entries)),
                    }
                }
                Ok((Some(iter), entries))
            })
            .await
            .map_err(Error::other)??;
            self.iter = iter;
            for entry in entries {
                let file_type = entry.file_type();
                let file_name = entry.name.into_string().map_err(|_| {
                    Error::new(ErrorKind::InvalidData, "directory entry is not valid UTF-8")
                })?;
                self.pending.push_back(DirEntry::new(
                    file_name,
                    file_type,
                    DirEntryFamily::Windows {
                        reparse_tag: entry.tag,
                    },
                ));
            }
        }
        Ok(self.pending.pop_front())
    }
}

impl File {
    /// Reads at an absolute offset.
    ///
    /// Windows has no true positional read: `seek_read` moves the handle's
    /// file pointer as a side effect. That is harmless because shared code
    /// never consumes that pointer and materializes the cursor before handing
    /// the descriptor to another process.
    pub(super) fn pread(file: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
        Ok(std::os::windows::fs::FileExt::seek_read(file, buf, offset)?)
    }

    pub(super) fn pwrite(file: &std::fs::File, buf: &[u8], offset: u64) -> Result<usize> {
        Ok(std::os::windows::fs::FileExt::seek_write(
            file, buf, offset,
        )?)
    }

    /// Identifies the file behind an open handle, as
    /// `(volume serial, file index)`.
    ///
    /// `std`'s accessors for these are still unstable, and the crate's own
    /// `WindowsMetadata` carries neither — its volume serial lives on the
    /// *filesystem* metadata — so this asks the platform directly.
    pub(super) fn impl_id(file: &std::fs::File) -> Result<FileId> {
        Self::handle_id(file.as_handle())
    }

    pub(super) fn handle_id(handle: BorrowedHandle<'_>) -> Result<FileId> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
        // SAFETY: the handle is borrowed from a live owner and `info` is a
        // correctly sized, writable output buffer.
        let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &raw mut info) };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
        Ok(FileId {
            volume: u64::from(info.dwVolumeSerialNumber),
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
}

fn typed_windows_path(path: &Path) -> Result<crate::path::Path<'_>> {
    let path = path
        .to_str()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "path is not UTF-8"))?;
    Ok(crate::path::Path::windows(path))
}

impl Direct {
    pub(super) async fn impl_access(_path: PathBuf, _mode: AccessFlags) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "POSIX access checks are not supported on Windows",
        ))
    }

    pub(super) async fn impl_rename(from: PathBuf, to: PathBuf, replace: bool) -> Result<()> {
        tokio::task::spawn_blocking(move || Self::rename_path(&from, &to, replace))
            .await
            .unwrap_or_else(|_| Err(Error::other("rename task failed")))
    }

    fn rename_path(from: &Path, to: &Path, replace: bool) -> Result<()> {
        let mut from = Self::rename_path_wide(from)?;
        from.push(0);
        let handle = unsafe {
            CreateFileW(
                from.as_ptr(),
                DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };

        if !replace {
            return Self::set_rename_information(handle.as_raw_handle(), to, false, false);
        }

        match Self::set_rename_information(handle.as_raw_handle(), to, true, true) {
            Err(error)
                if error.raw_os_error().is_some_and(|code| {
                    code == ERROR_NOT_SUPPORTED as i32
                        || code == ERROR_INVALID_FUNCTION as i32
                        || code == ERROR_INVALID_PARAMETER as i32
                }) =>
            {
                Self::set_rename_information(handle.as_raw_handle(), to, true, false)
            }
            result => result,
        }
    }

    fn rename_path_wide(path: &Path) -> Result<Vec<u16>> {
        let path: Vec<_> = path.as_os_str().encode_wide().collect();
        if path.contains(&0) {
            return Err(Error::new(ErrorKind::InvalidInput, "path contains NUL"));
        }
        Ok(path)
    }

    fn set_rename_information(
        handle: windows_sys::Win32::Foundation::HANDLE,
        to: &Path,
        replace: bool,
        extended: bool,
    ) -> Result<()> {
        let to = Self::rename_path_wide(to)?;
        let name_bytes = to
            .len()
            .checked_mul(mem::size_of::<u16>())
            .and_then(|len| u32::try_from(len).ok())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "path is too long"))?;
        let offset = mem::offset_of!(FILE_RENAME_INFO, FileName);
        let buffer_len = offset
            .checked_add(name_bytes as usize)
            .and_then(|len| len.checked_add(mem::size_of::<u16>()))
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "path is too long"))?;
        let word_size = mem::size_of::<usize>();
        let mut buffer = vec![0usize; buffer_len.div_ceil(word_size)];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();

        unsafe {
            if extended {
                (&raw mut (*info).Anonymous).write(FILE_RENAME_INFO_0 {
                    Flags: FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS,
                });
            } else {
                (&raw mut (*info).Anonymous).write(FILE_RENAME_INFO_0 {
                    ReplaceIfExists: replace,
                });
            }
            (&raw mut (*info).RootDirectory).write(ptr::null_mut());
            (&raw mut (*info).FileNameLength).write(name_bytes);
            to.as_ptr()
                .copy_to_nonoverlapping((&raw mut (*info).FileName).cast::<u16>(), to.len());
        }

        let buffer_len = u32::try_from(buffer_len)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "path is too long"))?;
        let class = if extended {
            FileRenameInfoEx
        } else {
            FileRenameInfo
        };
        if unsafe { SetFileInformationByHandle(handle, class, info.cast(), buffer_len) } == 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn acl_from_path(
        _path: &Path,
        _kind: AclKind,
        _default: bool,
        _resolve: Resolve,
    ) -> Result<Option<Acl>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "ACLs are not supported on Windows",
        ))
    }

    pub(super) fn set_acl_path(
        _path: &Path,
        _kind: AclKind,
        _acl: Option<&Acl>,
        _default: bool,
        _resolve: Resolve,
    ) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "ACLs are not supported on Windows",
        ))
    }

    pub(super) fn acl_from_file(
        _file: &std::fs::File,
        _kind: AclKind,
        _default: bool,
    ) -> Result<Option<Acl>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "ACLs are not supported on Windows",
        ))
    }

    pub(super) fn set_acl_file(
        _file: &std::fs::File,
        _kind: crate::security::AclKind,
        _acl: Option<&crate::security::Acl>,
        _default: bool,
    ) -> Result<()> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "ACLs are not supported on Windows",
        ))
    }

    /// Returns the attributes and reparse tag of the object at `path`, opening
    /// a final reparse point as itself.
    pub(super) fn probe_entry(path: &Path) -> Result<(u32, Option<u32>)> {
        let handle = Self::security_handle_flags(path, 0, true)?;
        Self::attribute_tag(handle.as_handle())
    }

    /// Returns the reparse tag of the object `handle` refers to, if it is a
    /// reparse point.
    pub(super) fn reparse_tag(handle: BorrowedHandle<'_>) -> Result<Option<u32>> {
        Ok(Self::attribute_tag(handle)?.1)
    }

    /// Returns the attributes of the object `handle` refers to, and its
    /// reparse tag if it is a reparse point.
    fn attribute_tag(handle: BorrowedHandle<'_>) -> Result<(u32, Option<u32>)> {
        let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { mem::zeroed() };
        let status = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileAttributeTagInfo,
                (&raw mut info).cast(),
                u32::try_from(mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap(),
            )
        };
        if status == 0 {
            return Err(Error::last_os_error());
        }
        let tag =
            (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag);
        Ok((info.FileAttributes, tag))
    }

    /// Returns whether opening `path` the way `resolve` asks needs
    /// `FILE_FLAG_OPEN_REPARSE_POINT`: whether its final component, if it is a
    /// reparse point, is one `resolve` opens as itself.
    ///
    /// This probes with a handle that can't touch the file's contents, so a
    /// write or truncating open never lands on a reparse point it then has to
    /// follow. A probe that fails says to open the final component as itself,
    /// and leaves the real open to report the error.
    fn opens_reparse_point(path: &Path, resolve: Resolve) -> bool {
        if resolve == Resolve::Target {
            return false;
        }
        let wide = Self::path_wide(path);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return true;
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        match Self::reparse_tag(handle.as_handle()) {
            Ok(Some(tag)) => Resolve::of_reparse_tag(tag) <= resolve,
            _ => true,
        }
    }

    /// Opens `path` the way `resolve` asks, calling `open` with whether to
    /// pass `FILE_FLAG_OPEN_REPARSE_POINT`.
    ///
    /// Some reparse points can't be followed by Win32 at all (app execution
    /// aliases, WSL symlinks), and fail with `ERROR_CANT_ACCESS_FILE`. Those
    /// are opened as themselves instead, as Python's `os.stat` does.
    fn open_resolved<T>(
        path: &Path,
        resolve: Resolve,
        open: impl Fn(bool) -> Result<T>,
    ) -> Result<T> {
        let reparse = Self::opens_reparse_point(path, resolve);
        match open(reparse) {
            Err(err)
                if !reparse && err.raw_os_error() == Some(ERROR_CANT_ACCESS_FILE.cast_signed()) =>
            {
                open(true)
            }
            result => result,
        }
    }

    fn security_handle(path: &Path, access: u32, resolve: Resolve) -> Result<OwnedHandle> {
        Self::open_resolved(path, resolve, |reparse| {
            Self::security_handle_flags(path, access, reparse)
        })
    }

    fn security_handle_flags(path: &Path, access: u32, reparse: bool) -> Result<OwnedHandle> {
        let path = Self::path_wide(path);
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if reparse {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let open = || {
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | flags,
                    ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(Error::last_os_error());
            }
            Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
        };
        if access & ACCESS_SYSTEM_SECURITY != 0 {
            dolang_winterop::security::with_security_privilege(open)
        } else {
            open()
        }
    }

    fn sec_desc_from_handle(
        handle: BorrowedHandle<'_>,
        mask: dolang_winterop::security::SecInfo,
    ) -> Result<SecDesc> {
        let mut descriptor = ptr::null_mut();
        let query_mask = if mask.is_empty() {
            SecInfo::OWNER
        } else {
            mask
        };
        let error = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                query_mask.bits(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if error != 0 {
            return Err(Error::from_raw_os_error(error as i32));
        }
        struct LocalDescriptor(*mut std::ffi::c_void);
        impl Drop for LocalDescriptor {
            fn drop(&mut self) {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
        let descriptor = LocalDescriptor(descriptor);
        let length = unsafe { GetSecurityDescriptorLength(descriptor.0) } as usize;
        let bytes = unsafe { slice::from_raw_parts(descriptor.0.cast::<u8>(), length) };
        SecDesc::from_bytes_with_mask(bytes, mask)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))
    }

    fn update_sec_desc_on_handle(handle: BorrowedHandle<'_>, descriptor: &SecDesc) -> Result<()> {
        let mask = descriptor.mask() & SecInfo::ALL;
        if mask.is_empty() {
            return Ok(());
        }

        let bytes = descriptor.to_bytes();
        let mut storage = vec![0u32; bytes.len().div_ceil(4)];
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), storage.as_mut_ptr().cast(), bytes.len());
        }
        let base = storage.as_mut_ptr().cast::<u8>();
        let component = |at: usize| unsafe {
            let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (!offset.eq(&0)).then(|| base.add(offset))
        };
        let owner = component(4).map_or(ptr::null_mut(), |value| value.cast());
        let group = component(8).map_or(ptr::null_mut(), |value| value.cast());
        let sacl = component(12).map_or(ptr::null(), |value| value.cast::<ACL>());
        let dacl = component(16).map_or(ptr::null(), |value| value.cast::<ACL>());

        let mut update_mask = mask;
        if mask.contains(SecInfo::DACL) {
            update_mask |= SecInfo::from_bits_retain(
                if descriptor
                    .control()
                    .contains(SecDescControl::DACL_PROTECTED)
                {
                    PROTECTED_DACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_DACL_SECURITY_INFORMATION
                },
            );
        }
        if mask.contains(SecInfo::SACL) {
            update_mask |= SecInfo::from_bits_retain(
                if descriptor
                    .control()
                    .contains(SecDescControl::SACL_PROTECTED)
                {
                    PROTECTED_SACL_SECURITY_INFORMATION
                } else {
                    UNPROTECTED_SACL_SECURITY_INFORMATION
                },
            );
        }
        let set = || {
            let error = unsafe {
                SetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_FILE_OBJECT,
                    update_mask.bits(),
                    owner,
                    group,
                    dacl,
                    sacl,
                )
            };
            if error == 0 {
                Ok(())
            } else {
                Err(Error::from_raw_os_error(error as i32))
            }
        };
        if mask.contains(SecInfo::SACL) {
            dolang_winterop::security::with_security_privilege(set)
        } else {
            set()
        }
    }

    pub(super) fn sec_desc_from_path(
        path: &Path,
        mask: dolang_winterop::security::SecInfo,
        resolve: Resolve,
    ) -> Result<SecDesc> {
        let mask = mask & SecInfo::ALL;
        let access = if mask.is_empty() || mask.intersects(SecInfo::ALL - SecInfo::SACL) {
            READ_CONTROL
        } else {
            0
        } | if mask.contains(SecInfo::SACL) {
            ACCESS_SYSTEM_SECURITY
        } else {
            0
        };
        let handle = Self::security_handle(path, access, resolve)?;
        Self::sec_desc_from_handle(handle.as_handle(), mask)
    }

    pub(super) fn update_sec_desc_path(
        path: &Path,
        descriptor: &SecDesc,
        resolve: Resolve,
    ) -> Result<()> {
        let mask = descriptor.mask();
        let mut access = 0;
        if mask.intersects(SecInfo::OWNER | SecInfo::GROUP) {
            access |= WRITE_OWNER;
        }
        // An ACL write also needs READ_CONTROL: `SetSecurityInfo` reads the
        // object's current descriptor back in order to recompute inheritance,
        // and without it every DACL write fails with ERROR_ACCESS_DENIED no
        // matter what the ACL says. Owner and group writes need no such read,
        // which is why they succeed on a handle that cannot write an ACL.
        if mask.contains(SecInfo::DACL) {
            access |= WRITE_DAC | READ_CONTROL;
        }
        if mask.contains(SecInfo::SACL) {
            access |= ACCESS_SYSTEM_SECURITY | READ_CONTROL;
        }
        let handle = Self::security_handle(path, access, resolve)?;
        Self::update_sec_desc_on_handle(handle.as_handle(), descriptor)
    }

    pub(super) fn sec_desc_from_file(
        file: &std::fs::File,
        mask: dolang_winterop::security::SecInfo,
    ) -> Result<SecDesc> {
        let mask = mask & SecInfo::ALL;
        Self::sec_desc_from_handle(file.as_handle(), mask)
    }

    pub(super) fn update_sec_desc_file(file: &std::fs::File, descriptor: &SecDesc) -> Result<()> {
        Self::update_sec_desc_on_handle(file.as_handle(), descriptor)
    }

    fn sid_name_use(value: i32) -> Result<SidNameUse> {
        match value {
            SID_TYPE_USER => Ok(SidNameUse::User),
            SID_TYPE_GROUP => Ok(SidNameUse::Group),
            SID_TYPE_DOMAIN => Ok(SidNameUse::Domain),
            SID_TYPE_ALIAS => Ok(SidNameUse::Alias),
            SID_TYPE_WELL_KNOWN_GROUP => Ok(SidNameUse::WellKnownGroup),
            SID_TYPE_DELETED_ACCOUNT => Ok(SidNameUse::DeletedAccount),
            SID_TYPE_INVALID => Ok(SidNameUse::Invalid),
            SID_TYPE_UNKNOWN => Ok(SidNameUse::Unknown),
            SID_TYPE_COMPUTER => Ok(SidNameUse::Computer),
            SID_TYPE_LABEL => Ok(SidNameUse::Label),
            SID_TYPE_LOGON_SESSION => Ok(SidNameUse::LogonSession),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "LookupAccount returned an invalid SID name use",
            )),
        }
    }

    fn lookup_error(error: Error) -> Error {
        if error.raw_os_error() == Some(ERROR_NONE_MAPPED as i32) {
            Error::from_system_code(
                ErrorKind::NotFound,
                error.to_string(),
                crate::target::OperatingSystem::Windows,
                ERROR_NONE_MAPPED as i32,
            )
        } else {
            error
        }
    }

    fn wide_result(buffer: &[u16], len: u32) -> String {
        let len = usize::try_from(len)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        let value = &buffer[..len];
        let value = value.strip_suffix(&[0]).unwrap_or(value);
        String::from_utf16_lossy(value)
    }

    fn lookup_sid_name(sid: Sid) -> crate::error::Result<SidName> {
        let mut sid_bytes = sid.to_bytes();
        let mut name_len = 0;
        let mut domain_len = 0;
        let mut kind = 0;
        unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid_bytes.as_mut_ptr().cast(),
                ptr::null_mut(),
                &mut name_len,
                ptr::null_mut(),
                &mut domain_len,
                &mut kind,
            );
        }
        let error = Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(Self::lookup_error(error));
        }
        let mut name = vec![0u16; usize::try_from(name_len).unwrap()];
        let mut domain = vec![0u16; usize::try_from(domain_len).unwrap()];
        if unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid_bytes.as_mut_ptr().cast(),
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        } == 0
        {
            return Err(Self::lookup_error(Error::last_os_error()));
        }
        Ok(SidName {
            sid,
            name: Self::wide_result(&name, name_len),
            domain: Self::wide_result(&domain, domain_len),
            kind: Self::sid_name_use(kind)?,
        })
    }

    fn lookup_account_sid(name: &str) -> Result<Sid> {
        let name: Vec<u16> = OsString::from(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut sid_len = 0;
        let mut domain_len = 0;
        let mut kind = 0;
        unsafe {
            LookupAccountNameW(
                ptr::null(),
                name.as_ptr(),
                ptr::null_mut(),
                &mut sid_len,
                ptr::null_mut(),
                &mut domain_len,
                &mut kind,
            );
        }
        let error = Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
        let word_size = mem::size_of::<usize>();
        let mut sid = vec![0usize; usize::try_from(sid_len).unwrap().div_ceil(word_size)];
        let mut domain = vec![0u16; usize::try_from(domain_len).unwrap()];
        if unsafe {
            LookupAccountNameW(
                ptr::null(),
                name.as_ptr(),
                sid.as_mut_ptr().cast(),
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        } == 0
        {
            return Err(Error::last_os_error());
        }
        let bytes = unsafe {
            slice::from_raw_parts(sid.as_ptr().cast::<u8>(), usize::try_from(sid_len).unwrap())
        };
        Sid::from_bytes(bytes).map_err(|error| Error::new(ErrorKind::InvalidData, error))
    }

    pub(super) async fn impl_sid_name(&self, sid: &Sid) -> crate::error::Result<SidName> {
        let sid = sid.clone();
        tokio::task::spawn_blocking(move || Self::lookup_sid_name(sid))
            .await
            .unwrap_or_else(|_| Err(Error::other("SID lookup task failed")))
    }

    pub(super) async fn impl_account_name(&self, name: &str) -> crate::error::Result<SidName> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            let sid = Self::lookup_account_sid(&name).map_err(Self::lookup_error)?;
            Self::lookup_sid_name(sid)
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("account lookup task failed")))
    }

    pub(super) fn program_not_found_error() -> Error {
        Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32)
    }

    pub(super) fn directory_requires_all_error() -> Error {
        Error::new(
            ErrorKind::IsADirectory,
            "directory operations require all: true",
        )
    }

    pub(super) fn directory_not_empty_error() -> Error {
        Error::new(ErrorKind::DirectoryNotEmpty, "directory not empty")
    }

    pub(super) fn not_a_directory_error() -> Error {
        Error::new(ErrorKind::NotADirectory, "not a directory")
    }

    fn final_path_from_handle(handle: BorrowedHandle<'_>) -> Result<PathBuf> {
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
            return Err(Error::last_os_error());
        }
        let len = usize::try_from(len).unwrap_or(path.len());
        if len >= path.len() {
            return Err(Error::other("path buffer too small"));
        }
        path.truncate(len);
        Ok(dunce::simplified(&PathBuf::from(OsString::from_wide(&path))).to_path_buf())
    }

    fn volume_root_path(path: &Path) -> Result<PathBuf> {
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
                _ => Err(Error::new(
                    ErrorKind::InvalidInput,
                    "unsupported Windows path prefix",
                )),
            },
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                "path has no Windows volume prefix",
            )),
        }
    }

    fn fs_query_root_metadata(root: &Path) -> Result<FsMetadata> {
        let root_str = Self::path_wide(root);
        let mut available = 0u64;
        let mut capacity = 0u64;
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(root_str.as_ptr(), &mut available, &mut capacity, &mut free)
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }

        let root_handle = Self::open_for_metadata(root, Resolve::Target)?;

        let mut serial = 0u32;
        let mut max_component = 0u32;
        let mut flags = 0u32;
        let mut fs_name = [0u16; MAX_PATH as usize + 1];
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                root_handle.as_raw_handle(),
                ptr::null_mut(),
                0,
                &mut serial,
                &mut max_component,
                &mut flags,
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
        let fs_name_len = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        let fs_type = String::from_utf16(&fs_name[..fs_name_len])
            .ok()
            .filter(|name| !name.is_empty());

        Ok(FsMetadata {
            capacity,
            free,
            available,
            block_size: 0,
            fs_type,
            family: FsMetadataFamily::Windows(WindowsFsMetadata {
                flags,
                volume_serial_number: serial,
                component_length_max: max_component,
            }),
        })
    }

    fn fs_metadata_from_handle(handle: BorrowedHandle<'_>) -> Result<FsMetadata> {
        let root = Self::volume_root_path(&Self::final_path_from_handle(handle)?)?;
        Self::fs_query_root_metadata(&root)
    }

    pub(super) fn fs_metadata_from_file(file: &std::fs::File) -> Result<FsMetadata> {
        Self::fs_metadata_from_handle(file.as_handle())
    }

    pub(super) fn allocation_unit_size(file: &std::fs::File) -> Result<u64> {
        let root = Self::volume_root_path(&Self::final_path_from_handle(file.as_handle())?)?;
        let root = Self::path_wide(&root);
        let mut sectors_per_cluster = 0;
        let mut bytes_per_sector = 0;
        let ok = unsafe {
            GetDiskFreeSpaceW(
                root.as_ptr(),
                &mut sectors_per_cluster,
                &mut bytes_per_sector,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
        Ok(u64::from(sectors_per_cluster) * u64::from(bytes_per_sector))
    }

    pub(super) fn metadata_with_security(
        metadata: std::fs::Metadata,
        file: &std::fs::File,
    ) -> Result<Metadata> {
        let descriptor = Self::sec_desc_from_file(file, SecInfo::OWNER | SecInfo::GROUP)?;
        let tag = Self::reparse_tag(file.as_handle())?;
        Ok(metadata_with_sids(
            metadata_with_reparse_tag(metadata_from_std(metadata), tag),
            descriptor.owner().cloned(),
            descriptor.group().cloned(),
        ))
    }

    pub(super) fn metadata_from_path(path: &Path, resolve: Resolve) -> Result<Metadata> {
        let handle = Self::security_handle(path, READ_CONTROL, resolve)?;
        let file = StdFile::from(handle);
        Self::metadata_with_security(file.metadata()?, &file)
    }

    pub(super) fn fs_metadata_from_path(path: &Path, resolve: Resolve) -> Result<FsMetadata> {
        let follow = resolve.follows();
        let root = if follow {
            Self::volume_root_path(&std::fs::canonicalize(path)?)?
        } else {
            Self::volume_root_path(path)?
        };
        Self::fs_query_root_metadata(&root)
    }

    fn path_wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    fn set_windows_compression(path: &[u16], compressed: bool) -> Result<()> {
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
            return Err(Error::last_os_error());
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
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn set_windows_sparse(path: &[u16], sparse: bool) -> Result<()> {
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
            return Err(Error::last_os_error());
        }
        let _handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let input = FILE_SET_SPARSE_BUFFER { SetSparse: sparse };
        let mut bytes_returned = 0;
        if unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_SPARSE,
                std::ptr::from_ref(&input).cast(),
                u32::try_from(std::mem::size_of_val(&input)).unwrap(),
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        } == 0
        {
            Err(Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn open_for_metadata(path: &Path, resolve: Resolve) -> Result<Arc<StdFile>> {
        let handle = Self::security_handle(path, 0, resolve)?;
        Ok(Arc::new(StdFile::from(handle)))
    }

    fn validate_attrs_patch(patch: AttrsPatch) -> Result<()> {
        let supported = AttrFlags::READONLY
            .union(AttrFlags::HIDDEN)
            .union(AttrFlags::SYSTEM)
            .union(AttrFlags::ARCHIVE)
            .union(AttrFlags::SPARSE)
            .union(AttrFlags::COMPRESSED)
            .union(AttrFlags::TEMPORARY)
            .union(AttrFlags::OFFLINE)
            .union(AttrFlags::NOT_CONTENT_INDEXED);
        if !patch.requested().difference(supported).is_empty() {
            Err(Error::new(
                ErrorKind::Unsupported,
                "one or more attributes cannot be set on this platform",
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn set_attrs_path(path: PathBuf, patch: AttrsPatch) -> Result<()> {
        Self::validate_attrs_patch(patch)?;

        if patch.is_empty() {
            return Ok(());
        }

        let path = Self::path_wide(&path);
        let mut attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            return Err(Error::last_os_error());
        }

        for (semantic, native) in [
            (AttrFlags::READONLY, FILE_ATTRIBUTE_READONLY),
            (AttrFlags::HIDDEN, FILE_ATTRIBUTE_HIDDEN),
            (AttrFlags::SYSTEM, FILE_ATTRIBUTE_SYSTEM),
            (AttrFlags::ARCHIVE, FILE_ATTRIBUTE_ARCHIVE),
            (AttrFlags::TEMPORARY, FILE_ATTRIBUTE_TEMPORARY),
            (AttrFlags::OFFLINE, FILE_ATTRIBUTE_OFFLINE),
            (
                AttrFlags::NOT_CONTENT_INDEXED,
                FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
            ),
        ] {
            if patch.set.contains(semantic) {
                attrs |= native;
            } else if patch.clear.contains(semantic) {
                attrs &= !native;
            }
        }

        let ordinary = AttrFlags::READONLY
            .union(AttrFlags::HIDDEN)
            .union(AttrFlags::SYSTEM)
            .union(AttrFlags::ARCHIVE)
            .union(AttrFlags::TEMPORARY)
            .union(AttrFlags::OFFLINE)
            .union(AttrFlags::NOT_CONTENT_INDEXED);
        if patch.requested().intersects(ordinary) {
            let res = unsafe { SetFileAttributesW(path.as_ptr(), attrs) };
            if res == 0 {
                return Err(Error::last_os_error());
            }
        }

        if patch.set.contains(AttrFlags::COMPRESSED) {
            Self::set_windows_compression(&path, true)?;
        } else if patch.clear.contains(AttrFlags::COMPRESSED) {
            Self::set_windows_compression(&path, false)?;
        }

        if patch.set.contains(AttrFlags::SPARSE) {
            Self::set_windows_sparse(&path, true)?;
        } else if patch.clear.contains(AttrFlags::SPARSE) {
            Self::set_windows_sparse(&path, false)?;
        }

        Ok(())
    }

    pub(crate) fn known_folder(folder_id: &GUID) -> Result<PathBuf> {
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
                Err(Error::from_raw_os_error(result))
            }
        }
    }

    pub(super) fn home_dir_platform(_env: &HashMap<String, Option<String>>) -> Result<PathBuf> {
        Self::known_folder(&FOLDERID_Profile)
    }

    pub(super) fn cache_dir_platform(
        app: Option<&str>,
        _env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf> {
        let base = Self::known_folder(&FOLDERID_LocalAppData)?;
        Ok(match app {
            Some(app) => base.join(app).join("Cache"),
            None => base,
        })
    }

    pub(super) fn data_dir_platform(
        app: Option<&str>,
        _env: &HashMap<String, Option<String>>,
        local: bool,
    ) -> Result<PathBuf> {
        let base = Self::known_folder(if local {
            &FOLDERID_LocalAppData
        } else {
            &FOLDERID_RoamingAppData
        })?;
        Ok(match app {
            Some(app) => base.join(app).join("data"),
            None => base,
        })
    }

    pub(super) fn state_dir_platform(
        _app: Option<&str>,
        _env: &HashMap<String, Option<String>>,
    ) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    pub(super) fn temp_dir_platform(env: &HashMap<String, Option<String>>) -> Result<PathBuf> {
        let override_value = |key: &str| match env
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        {
            Some((_, value)) => value.clone(),
            None => std::env::var(key).ok(),
        };
        let absolute = |key: &str| -> Result<Option<PathBuf>> {
            let Some(value) = override_value(key) else {
                return Ok(None);
            };
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(Some(path))
            } else {
                Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{key} must be an absolute path"),
                ))
            }
        };
        // Follows `GetTempPathW`'s lookup order (with `GetTempPath2W`'s
        // SystemTemp for SYSTEM), reading variables through the VM
        // environment rather than the process environment.
        for key in ["TMP", "TEMP"] {
            if let Some(path) = absolute(key)? {
                return Ok(path);
            }
        }
        let windows = Self::known_folder(&FOLDERID_Windows)?;
        if Self::is_local_system()? {
            return Ok(windows.join("SystemTemp"));
        }
        Ok(absolute("USERPROFILE")?.unwrap_or(windows))
    }

    pub(crate) fn is_local_system() -> Result<bool> {
        let info = SecurityInfo::current()?;
        let system = Sid::from(WellKnownSid::LocalSystem);
        Ok(info
            .windows()
            .is_some_and(|token| *token.user_sid() == system))
    }

    fn nt_error(status: windows_sys::Win32::Foundation::NTSTATUS) -> Error {
        Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
    }

    pub(super) fn windows_xattr_name(name: &str, namespace: Option<&str>) -> Result<Vec<u8>> {
        if namespace.is_some() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "xattr namespaces are not supported on this platform",
            ));
        }
        if name.as_bytes().contains(&0) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "xattr name contains NUL",
            ));
        }
        let name = name.as_bytes().to_vec();
        let Ok(_len) = u8::try_from(name.len()) else {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "xattr name is too long",
            ));
        };
        Ok(name)
    }

    const fn align_windows_ea(len: usize) -> usize {
        (len + 3) & !3
    }

    fn windows_get_ea_list(name: &[u8]) -> Result<Vec<u8>> {
        let len = usize::from(
            u8::try_from(name.len())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "xattr name is too long"))?,
        );
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

    fn windows_full_ea(name: &[u8], value: &[u8]) -> Result<Vec<u8>> {
        let name_len = usize::from(
            u8::try_from(name.len())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "xattr name is too long"))?,
        );
        let value_len = usize::from(
            u16::try_from(value.len())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "xattr value is too large"))?,
        );
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

    fn windows_parse_full_ea_chunk(buf: &[u8]) -> Result<Vec<XattrEntry>> {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while offset < buf.len() {
            let remaining = &buf[offset..];
            if remaining.len() < std::mem::size_of::<FILE_FULL_EA_INFORMATION>() {
                return Err(Error::new(ErrorKind::InvalidData, "EA buffer truncated"));
            }
            let entry = unsafe { &*remaining.as_ptr().cast::<FILE_FULL_EA_INFORMATION>() };
            let name_len = usize::from(entry.EaNameLength);
            let value_len = usize::from(entry.EaValueLength);
            let name_offset = std::mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName);
            let total_len = name_offset
                .checked_add(name_len)
                .and_then(|v| v.checked_add(1))
                .and_then(|v| v.checked_add(value_len))
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "EA buffer overflow"))?;
            if total_len > remaining.len() {
                return Err(Error::new(ErrorKind::InvalidData, "EA entry truncated"));
            }
            let name = unsafe {
                slice::from_raw_parts(entry.EaName.as_ptr().cast::<u8>(), name_len).to_vec()
            };
            entries.push(XattrEntry {
                name: String::from_utf8(name)
                    .map_err(|_| Error::new(ErrorKind::InvalidData, "xattr name is not UTF-8"))?,
                namespace: None,
                size: Some(value_len as u64),
                flags: Some(entry.Flags),
            });
            if entry.NextEntryOffset == 0 {
                break;
            }
            let next = usize::try_from(entry.NextEntryOffset)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid EA entry offset"))?;
            if next > remaining.len() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "invalid EA entry offset",
                ));
            }
            offset += next;
        }
        Ok(entries)
    }

    fn windows_parse_full_ea_value(buf: &[u8]) -> Result<(String, Vec<u8>)> {
        if buf.len() < mem::size_of::<FILE_FULL_EA_INFORMATION>() {
            return Err(Error::new(ErrorKind::InvalidData, "EA buffer truncated"));
        }
        let entry = unsafe { &*buf.as_ptr().cast::<FILE_FULL_EA_INFORMATION>() };
        let name_len = usize::from(entry.EaNameLength);
        let value_len = usize::from(entry.EaValueLength);
        let name_offset = mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName);
        let value_offset = mem::offset_of!(FILE_FULL_EA_INFORMATION, EaName) + name_len + 1;
        let end = value_offset
            .checked_add(value_len)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "EA buffer overflow"))?;
        if end > buf.len() {
            return Err(Error::new(ErrorKind::InvalidData, "EA entry truncated"));
        }
        let name = String::from_utf8(buf[name_offset..name_offset + name_len].to_vec())
            .map_err(|_| Error::new(ErrorKind::InvalidData, "xattr name is not UTF-8"))?;
        Ok((name, buf[value_offset..end].to_vec()))
    }

    pub(super) unsafe fn windows_list_xattrs(
        handle: BorrowedHandle<'_>,
    ) -> Result<Vec<XattrEntry>> {
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

    pub(super) unsafe fn windows_get_xattr(
        handle: BorrowedHandle<'_>,
        name: &[u8],
    ) -> Result<Vec<u8>> {
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
                        return Err(Error::new(
                            ErrorKind::NotFound,
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

    pub(super) unsafe fn windows_set_xattr(
        handle: BorrowedHandle<'_>,
        name: &[u8],
        value: &[u8],
    ) -> Result<()> {
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

    fn windows_parse_streams(buf: &[u8]) -> Result<Vec<StreamEntry>> {
        let mut streams = Vec::new();
        let mut offset = 0usize;
        while offset < buf.len() {
            if buf.len() - offset < mem::size_of::<FILE_STREAM_INFO>() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "truncated FILE_STREAM_INFO entry",
                ));
            }
            let info = unsafe { &*buf[offset..].as_ptr().cast::<FILE_STREAM_INFO>() };
            let name_len = usize::try_from(info.StreamNameLength)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "stream name too large"))?;
            if name_len % 2 != 0 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "invalid stream name length",
                ));
            }
            let name_slice =
                unsafe { slice::from_raw_parts(info.StreamName.as_ptr(), name_len / 2) };
            let raw_name = String::from_utf16(name_slice)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "stream name is not UTF-16"))?;
            let (name, r#type) = crate::path::stream::parse_raw_name(&raw_name)?;
            let size = u64::try_from(info.StreamSize)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "stream size out of range"))?;
            let alloc_size = u64::try_from(info.StreamAllocationSize).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
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
                Error::new(ErrorKind::InvalidData, "stream entry offset out of range")
            })?;
            if next == 0 {
                break;
            }
            offset = offset.checked_add(next).ok_or_else(|| {
                Error::new(ErrorKind::InvalidData, "stream entry offset overflow")
            })?;
        }
        Ok(streams)
    }

    pub(super) unsafe fn windows_list_streams(
        handle: BorrowedHandle<'_>,
    ) -> Result<Vec<StreamEntry>> {
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
            let err = Error::last_os_error();
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

    pub(super) async fn impl_symlink(cwd: &Path, src: &Path, dst: &Path) -> Result<()> {
        let metadata = fs::metadata(cwd.join(src)).await?;
        if metadata.is_dir() {
            Self::impl_symlink_dir(src, dst).await
        } else {
            Self::impl_symlink_file(src, dst).await
        }
    }

    pub(super) async fn impl_symlink_dir(src: &Path, dst: &Path) -> Result<()> {
        Ok(fs::symlink_dir(src, dst).await?)
    }

    pub(super) async fn impl_symlink_file(src: &Path, dst: &Path) -> Result<()> {
        Ok(fs::symlink_file(src, dst).await?)
    }

    /// Duplicates a reparse point (symlink, junction, or otherwise) byte for
    /// byte, rather than reinterpreting its target. This preserves details
    /// (relative vs. absolute, print name vs. substitute name, and the
    /// reparse tag itself) that re-deriving a fresh reparse point from a
    /// resolved target path cannot recover. `is_dir` says whether `src` has
    /// the directory attribute, which std's file type hides for a junction or
    /// directory symlink by reporting it as a symlink.
    pub(super) fn copy_reparse_point_sync(src: &Path, dst: &Path, is_dir: bool) -> Result<()> {
        let mut read_opts = StdOpenOptions::new();
        read_opts
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let src_file = read_opts.open(src)?;
        let buffer = Self::reparse_data(src_file.as_handle())?;
        drop(src_file);

        if is_dir {
            std::fs::create_dir(dst)?;
        } else {
            StdOpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dst)?;
        }

        let mut write_opts = StdOpenOptions::new();
        write_opts
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let dst_file = match write_opts.open(dst) {
            Ok(file) => file,
            Err(err) => {
                Self::remove_reparse_placeholder(dst, is_dir);
                return Err(err.into());
            }
        };

        let result = Self::fs_control(
            dst_file.as_handle(),
            FSCTL_SET_REPARSE_POINT,
            &buffer,
            &mut [],
        );
        drop(dst_file);
        if let Err(err) = result {
            Self::remove_reparse_placeholder(dst, is_dir);
            return Err(err);
        }
        Ok(())
    }

    fn remove_reparse_placeholder(dst: &Path, is_dir: bool) {
        if is_dir {
            let _ = std::fs::remove_dir(dst);
        } else {
            let _ = std::fs::remove_file(dst);
        }
    }

    /// Sends the file system control `code` to `handle`, returning how many
    /// bytes of `output` it filled.
    fn fs_control(
        handle: BorrowedHandle<'_>,
        code: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize> {
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                code,
                if input.is_empty() {
                    ptr::null()
                } else {
                    input.as_ptr().cast()
                },
                u32::try_from(input.len()).map_err(Error::other)?,
                if output.is_empty() {
                    ptr::null_mut()
                } else {
                    output.as_mut_ptr().cast()
                },
                u32::try_from(output.len()).map_err(Error::other)?,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
        Ok(usize::try_from(returned).unwrap_or(output.len()))
    }

    /// Reads the reparse data buffer of the reparse point `handle` refers to,
    /// which must have been opened with `FILE_FLAG_OPEN_REPARSE_POINT`.
    fn reparse_data(handle: BorrowedHandle<'_>) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
        let len = Self::fs_control(handle, FSCTL_GET_REPARSE_POINT, &[], &mut buffer)?;
        buffer.truncate(len);
        Ok(buffer)
    }

    /// Reads the reparse data of `handle` if it is a reparse point with one of
    /// `tags`, returning its tag and tag-specific data. Anything else is an
    /// `InvalidInput` error saying `mismatch`.
    fn tagged_reparse_data(
        handle: BorrowedHandle<'_>,
        tags: &[u32],
        mismatch: &str,
    ) -> Result<(u32, Vec<u8>)> {
        let wrong = || Error::new(ErrorKind::InvalidInput, mismatch);
        let buffer = match Self::reparse_data(handle) {
            Err(err) if err.raw_os_error() == Some(ERROR_NOT_A_REPARSE_POINT.cast_signed()) => {
                return Err(wrong());
            }
            result => result?,
        };
        let (tag, data) = reparse_parts(&buffer)?;
        if !tags.contains(&tag) {
            return Err(wrong());
        }
        Ok((tag, data.to_vec()))
    }

    /// Opens the final component of `path` as itself, with `access`.
    fn reparse_handle(path: &Path, access: u32) -> Result<OwnedHandle> {
        Self::security_handle_flags(path, access, true)
    }

    /// Reads the target of the symbolic link `handle` refers to: a Windows
    /// path for a Windows symbolic link, or a Unix path for a WSL one.
    pub(super) fn read_link_handle(handle: BorrowedHandle<'_>) -> Result<crate::path::PathBuf> {
        let lx_symlink = IO_REPARSE_TAG_LX_SYMLINK.cast_unsigned();
        let (tag, data) = Self::tagged_reparse_data(
            handle,
            &[IO_REPARSE_TAG_SYMLINK, lx_symlink],
            "not a symbolic link",
        )?;
        if tag == lx_symlink {
            // A version number, then the target as UTF-8 without a terminator
            let target = data.get(4..).ok_or_else(malformed_reparse_data)?;
            let target = std::str::from_utf8(target).map_err(|_| {
                Error::new(ErrorKind::InvalidData, "WSL symlink target is not UTF-8")
            })?;
            return Ok(crate::path::PathBuf::from_unix(target));
        }
        let flags = data.get(8..12).ok_or_else(malformed_reparse_data)?;
        let flags = u32::from_le_bytes(flags.try_into().unwrap());
        let name = substitute_name(&data, SYMLINK_PATH_START)?;
        let target = if flags & SYMLINK_FLAG_RELATIVE != 0 {
            PathBuf::from(OsString::from_wide(&name))
        } else {
            user_path(&name)
        };
        crate::path::PathBuf::from_native(target)
    }

    pub(super) fn read_link_path(path: &Path) -> Result<crate::path::PathBuf> {
        let handle = Self::reparse_handle(path, 0)?;
        Self::read_link_handle(handle.as_handle())
    }

    /// Reads the directory the junction `handle` refers to leads to.
    pub(super) fn read_junction_handle(handle: BorrowedHandle<'_>) -> Result<PathBuf> {
        let (_, data) =
            Self::tagged_reparse_data(handle, &[IO_REPARSE_TAG_MOUNT_POINT], "not a junction")?;
        Ok(user_path(&substitute_name(&data, MOUNT_POINT_PATH_START)?))
    }

    pub(super) fn read_junction_path(path: &Path) -> Result<PathBuf> {
        let handle = Self::reparse_handle(path, 0)?;
        Self::read_junction_handle(handle.as_handle())
    }

    /// Makes the empty directory `handle` refers to a junction to `target`,
    /// or with no target, removes its junction and leaves the directory.
    pub(super) fn set_junction_handle(
        handle: BorrowedHandle<'_>,
        target: Option<&Path>,
    ) -> Result<()> {
        match target {
            Some(target) => {
                let buffer = mount_point_buffer(target)?;
                Self::fs_control(handle, FSCTL_SET_REPARSE_POINT, &buffer, &mut [])?;
            }
            None => match Self::reparse_tag(handle)? {
                None => {}
                Some(IO_REPARSE_TAG_MOUNT_POINT) => {
                    // A header with no data names the tag to remove. Windows
                    // wants just the tag, length and reserved field for a
                    // Microsoft tag; Wine wants the whole
                    // `REPARSE_GUID_DATA_BUFFER` header, with a zero GUID.
                    let mut header = [0u8; REPARSE_GUID_HEADER_LEN];
                    header[..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
                    let delete = |len| {
                        Self::fs_control(
                            handle,
                            FSCTL_DELETE_REPARSE_POINT,
                            &header[..len],
                            &mut [],
                        )
                    };
                    match delete(REPARSE_HEADER_LEN) {
                        Err(err)
                            if err.raw_os_error()
                                == Some(ERROR_INVALID_REPARSE_DATA.cast_signed()) =>
                        {
                            delete(REPARSE_GUID_HEADER_LEN)?;
                        }
                        result => {
                            result?;
                        }
                    }
                }
                Some(_) => return Err(Error::new(ErrorKind::InvalidInput, "not a junction")),
            },
        }
        Ok(())
    }

    pub(super) fn set_junction_path(path: &Path, target: Option<&Path>) -> Result<()> {
        let handle = Self::reparse_handle(path, GENERIC_WRITE)?;
        Self::set_junction_handle(handle.as_handle(), target)
    }

    pub(super) fn read_junction_file(file: &StdFile) -> Result<PathBuf> {
        Self::read_junction_handle(file.as_handle())
    }

    pub(super) fn set_junction_file(file: &StdFile, target: Option<&Path>) -> Result<()> {
        Self::set_junction_handle(file.as_handle(), target)
    }

    /// Creates the directory `path` and makes it a junction to `target`,
    /// removing the directory again if that fails.
    pub(super) fn create_junction_path(path: &Path, target: &Path) -> Result<()> {
        // Check the target before touching the file system
        let buffer = mount_point_buffer(target)?;
        std::fs::create_dir(path)?;
        let result = Self::reparse_handle(path, GENERIC_WRITE).and_then(|handle| {
            Self::fs_control(
                handle.as_handle(),
                FSCTL_SET_REPARSE_POINT,
                &buffer,
                &mut [],
            )
        });
        if let Err(err) = result {
            Self::remove_reparse_placeholder(path, true);
            return Err(err);
        }
        Ok(())
    }

    pub(super) async fn impl_xattrs(
        &self,
        path: &Path,
        namespace: XattrNamespace<'_>,
        resolve: Resolve,
    ) -> Result<Vec<XattrEntry>> {
        let file = self
            .direct_open_options()
            .read(true)
            .resolve(resolve)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_xattrs(&file.fd()?, namespace).await
    }

    pub(super) async fn impl_streams(
        &self,
        path: &Path,
        resolve: Resolve,
    ) -> Result<Vec<StreamEntry>> {
        let file = Self::open_for_metadata(path, resolve)?;
        Self::impl_file_streams(&file).await
    }

    pub(super) async fn impl_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        resolve: Resolve,
    ) -> Result<Vec<u8>> {
        let file = self
            .direct_open_options()
            .read(true)
            .resolve(resolve)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_xattr(&file.fd()?, name, namespace).await
    }

    pub(super) async fn impl_set_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        resolve: Resolve,
    ) -> Result<()> {
        let file = self
            .direct_open_options()
            .write(true)
            .resolve(resolve)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_set_xattr(&file.fd()?, name, namespace, value).await
    }

    pub(super) async fn impl_remove_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        resolve: Resolve,
    ) -> Result<()> {
        let file = self
            .direct_open_options()
            .read(true)
            .write(true)
            .resolve(resolve)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_remove_xattr(&file.fd()?, name, namespace).await
    }

    pub(super) async fn impl_file_xattrs(
        file: &Arc<std::fs::File>,
        namespace: XattrNamespace<'_>,
    ) -> Result<Vec<XattrEntry>> {
        if let XattrNamespace::Named(_) = namespace {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "xattr namespaces are not supported on this platform",
            ));
        }
        let file = Arc::clone(file);
        tokio::task::spawn_blocking(move || unsafe { Self::windows_list_xattrs(file.as_handle()) })
            .await
            .unwrap_or_else(|e| Err(Error::other(e)))
    }

    pub(super) async fn impl_file_xattr(
        file: &Arc<std::fs::File>,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<u8>> {
        let name = Self::windows_xattr_name(name, namespace)?;
        let file = Arc::clone(file);
        tokio::task::spawn_blocking(move || unsafe {
            Self::windows_get_xattr(file.as_handle(), &name)
        })
        .await
        .unwrap_or_else(|e| Err(Error::other(e)))
    }

    pub(super) async fn impl_file_streams(file: &Arc<std::fs::File>) -> Result<Vec<StreamEntry>> {
        let file = Arc::clone(file);
        tokio::task::spawn_blocking(move || unsafe { Self::windows_list_streams(file.as_handle()) })
            .await
            .unwrap_or_else(|e| Err(Error::other(e)))
    }

    pub(super) async fn impl_file_set_xattr(
        file: &Arc<std::fs::File>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<()> {
        if value.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "empty xattr values are not supported on this platform",
            ));
        }
        let name = Self::windows_xattr_name(name, namespace)?;
        let value = value.to_vec();
        let file = Arc::clone(file);
        tokio::task::spawn_blocking(move || unsafe {
            Self::windows_set_xattr(file.as_handle(), &name, &value)
        })
        .await
        .unwrap_or_else(|e| Err(Error::other(e)))
    }

    pub(super) async fn impl_file_remove_xattr(
        file: &Arc<std::fs::File>,
        name: &str,
        namespace: Option<&str>,
    ) -> Result<()> {
        let name = Self::windows_xattr_name(name, namespace)?;
        let file = Arc::clone(file);
        tokio::task::spawn_blocking(move || unsafe {
            Self::windows_set_xattr(file.as_handle(), &name, &[])
        })
        .await
        .unwrap_or_else(|e| Err(Error::other(e)))
    }

    pub(super) async fn impl_update_metadata(
        &self,
        paths: &[PathBuf],
        patch: MetadataPatch,
    ) -> Result<()> {
        if patch.is_empty() {
            return Ok(());
        }
        if patch.mode.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "mode cannot be set on this platform",
            ));
        }
        if !patch.resolve.follows() && !patch.attrs.is_empty() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "attributes cannot be set without following symlinks on this platform",
            ));
        }
        Self::validate_attrs_patch(patch.attrs)?;

        let paths = paths.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut resolved_names: HashMap<String, Sid> = HashMap::new();
            let mut resolve = |identity| match identity {
                OwnershipIdentity::Sid(sid) => Ok(sid),
                OwnershipIdentity::Name(name) => {
                    if let Some(sid) = resolved_names.get(&name) {
                        Ok(sid.clone())
                    } else {
                        let sid = Self::lookup_account_sid(&name)?;
                        resolved_names.insert(name, sid.clone());
                        Ok(sid)
                    }
                }
                OwnershipIdentity::Id(_) => Err(Error::new(
                    ErrorKind::InvalidInput,
                    "numeric ownership IDs are not supported on Windows",
                )),
            };
            let user = patch.user.map(&mut resolve).transpose()?;
            let group = patch.group.map(&mut resolve).transpose()?;
            let mask = if user.is_some() {
                SecInfo::OWNER
            } else {
                SecInfo::empty()
            } | if group.is_some() {
                SecInfo::GROUP
            } else {
                SecInfo::empty()
            };
            let descriptor = if mask.is_empty() {
                None
            } else {
                Some(
                    SecDesc::new(mask, 0, SecDescControl::empty(), user, group, None, None)
                        .map_err(|error| Error::new(ErrorKind::InvalidData, error))?,
                )
            };

            for path in paths {
                if let Some(descriptor) = &descriptor {
                    Self::update_sec_desc_path(&path, descriptor, patch.resolve)?;
                }
                if !patch.attrs.is_empty() {
                    Self::set_attrs_path(path.clone(), patch.attrs)?;
                }
                if patch.accessed.is_some() || patch.modified.is_some() || patch.created.is_some() {
                    Self::set_file_times_path(
                        &path,
                        patch.accessed,
                        patch.modified,
                        patch.created,
                        patch.resolve,
                    )?;
                }
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("failed to join metadata update task")))
    }

    pub(super) async fn impl_canonicalize(&self, path: &Path) -> Result<PathBuf> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<PathBuf> { Ok(dunce::canonicalize(path)?) })
            .await
            .unwrap_or_else(|e| Err(Error::other(e)))
    }

    fn set_file_times_path(
        path: &Path,
        accessed: Option<i128>,
        modified: Option<i128>,
        created: Option<i128>,
        resolve: Resolve,
    ) -> Result<()> {
        use std::{fs::FileTimes, os::windows::fs::FileTimesExt};
        use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

        let file = Self::open_resolved(path, resolve, |reparse| {
            let mut opts = StdOpenOptions::new();
            opts.access_mode(FILE_WRITE_ATTRIBUTES);
            let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
            if reparse {
                flags |= FILE_FLAG_OPEN_REPARSE_POINT;
            }
            opts.custom_flags(flags);
            Ok(opts.open(path)?)
        })?;
        let mut times = FileTimes::new();
        if let Some(accessed) = accessed {
            times = times.set_accessed(nanos_to_system_time(accessed)?);
        }
        if let Some(modified) = modified {
            times = times.set_modified(nanos_to_system_time(modified)?);
        }
        if let Some(created) = created {
            times = times.set_created(nanos_to_system_time(created)?);
        }
        Ok(file.set_times(times)?)
    }
}

impl Direct {
    fn direct_open_options(&self) -> OpenOptions {
        OpenOptions::default()
    }
}

impl Child {
    pub(super) async fn impl_terminate(self) -> Result<Option<std::process::ExitStatus>> {
        let pid = self.inner.id();
        let mut child = self.inner;
        let Some(pid) = pid else {
            return Ok(child.wait().await.map(Some)?);
        };
        let _ = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };

        let wait = async {
            if self.process_control == crate::process::ProcessControl::Foreground {
                return Ok(child.wait().await?);
            }
            loop {
                let active = if let Some(job) = &self.job {
                    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { mem::zeroed() };
                    let result = unsafe {
                        QueryInformationJobObject(
                            job.as_raw_handle(),
                            JobObjectBasicAccountingInformation,
                            (&raw mut info).cast(),
                            mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                            ptr::null_mut(),
                        )
                    };
                    if result == 0 {
                        return Err(Error::last_os_error());
                    }
                    info.ActiveProcesses != 0
                } else {
                    false
                };
                if !active {
                    return Ok(child.wait().await?);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        if let Ok(status) = timeout(self.termination_policy.grace, wait).await {
            return status.map(Some);
        }
        if !self.termination_policy.force {
            return Ok(None);
        }
        if let Some(job) = &self.job {
            if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0 {
                return Err(Error::last_os_error());
            }
        } else {
            let _ = child.start_kill();
        }
        Ok(child.wait().await.map(Some)?)
    }
}

impl Command<'_> {
    pub(super) fn configure_process(&self, command: &mut tokio::process::Command) -> Result<()> {
        let mut flags = CREATE_NEW_PROCESS_GROUP;
        if self.process_control == crate::process::ProcessControl::Background {
            flags |= CREATE_SUSPENDED;
        }
        command.creation_flags(flags);
        Ok(())
    }

    pub(super) fn finish_spawn(&self, mut child: tokio::process::Child) -> Result<Child> {
        let job = if self.process_control == crate::process::ProcessControl::Background {
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if handle.is_null() {
                let _ = child.start_kill();
                return Err(Error::last_os_error());
            }
            let job = unsafe { OwnedHandle::from_raw_handle(handle) };
            let Some(pid) = child.id() else {
                let _ = child.start_kill();
                return Err(Error::other("spawned process has no process ID"));
            };
            let Some(process) = child.raw_handle() else {
                let _ = child.start_kill();
                return Err(Error::other("spawned process has no process handle"));
            };
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
                let error = Error::last_os_error();
                let _ = child.start_kill();
                return Err(error);
            }
            if let Err(error) = resume_process(pid) {
                let _ = child.start_kill();
                return Err(error);
            }
            Some(job)
        } else {
            None
        };
        Ok(Child::new(
            child,
            self.process_control,
            self.termination_policy,
            job,
        ))
    }

    pub(super) fn impl_stdout_inherit_stderr(&mut self) -> Result<&mut Self> {
        self.stdout = Some(std::process::Stdio::from(
            std::io::stderr().as_handle().try_clone_to_owned()?,
        ));
        Ok(self)
    }

    pub(super) fn impl_stderr_inherit_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = Some(std::process::Stdio::from(
            std::io::stdout().as_handle().try_clone_to_owned()?,
        ));
        Ok(self)
    }
}

fn resume_process(pid: u32) -> Result<()> {
    // std::process closes the primary thread handle returned by CreateProcess.
    // A newly created suspended process has only that thread, so locate it by
    // owner PID before allowing the process to execute.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: THREADENTRY32 = unsafe { mem::zeroed() };
    entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
    if unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) } == 0 {
        return Err(Error::last_os_error());
    }
    loop {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(Error::last_os_error());
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) } == 0 {
            return Err(Error::new(
                ErrorKind::NotFound,
                "spawned process has no primary thread",
            ));
        }
    }
}

fn nanos_to_system_time(nanos: i128) -> Result<SystemTime> {
    let (negative, nanos) = if nanos < 0 {
        (true, nanos.unsigned_abs())
    } else {
        (false, nanos as u128)
    };
    let secs = u64::try_from(nanos / 1_000_000_000)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid timestamp"))?;
    let subsec_nanos =
        u32::try_from(nanos % 1_000_000_000).expect("nanosecond remainder is in u32 range");
    let duration = Duration::new(secs, subsec_nanos);
    let time = if negative {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    };
    time.ok_or_else(|| Error::new(ErrorKind::InvalidInput, "invalid timestamp"))
}

/// The attributes `CreateFileW` can give a file it creates.
const CREATE_ATTRS: [(AttrFlags, u32); 7] = [
    (AttrFlags::READONLY, FILE_ATTRIBUTE_READONLY),
    (AttrFlags::HIDDEN, FILE_ATTRIBUTE_HIDDEN),
    (AttrFlags::SYSTEM, FILE_ATTRIBUTE_SYSTEM),
    (AttrFlags::ARCHIVE, FILE_ATTRIBUTE_ARCHIVE),
    (AttrFlags::TEMPORARY, FILE_ATTRIBUTE_TEMPORARY),
    (AttrFlags::OFFLINE, FILE_ATTRIBUTE_OFFLINE),
    (
        AttrFlags::NOT_CONTENT_INDEXED,
        FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
    ),
];

impl OpenOptions {
    /// The access mask to request: the presets `read`, `write` and `append`
    /// stand for, plus any explicit rights.
    fn access_mask(&self) -> u32 {
        let mut mask = self.extra.access.unwrap_or(0);
        if self.read {
            mask |= GENERIC_READ;
        }
        if self.append {
            // As std does: every write right but the one that allows writing
            // anywhere.
            mask |= FILE_GENERIC_WRITE & !FILE_WRITE_DATA;
        } else if self.write {
            mask |= GENERIC_WRITE;
        }
        mask
    }

    fn share_mask(&self) -> u32 {
        let share = self.extra.share.unwrap_or(ShareMode::all());
        [
            (ShareMode::READ, FILE_SHARE_READ),
            (ShareMode::WRITE, FILE_SHARE_WRITE),
            (ShareMode::DELETE, FILE_SHARE_DELETE),
        ]
        .into_iter()
        .filter(|(mode, _)| share.contains(*mode))
        .fold(0, |mask, (_, native)| mask | native)
    }

    fn attrs_mask(&self) -> Result<u32> {
        let attrs = self.extra.attrs;
        let supported = CREATE_ATTRS
            .iter()
            .fold(AttrFlags::empty(), |all, (attr, _)| all.union(*attr));
        if !attrs.difference(supported).is_empty() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "one or more attributes cannot be set when creating a file",
            ));
        }
        Ok(CREATE_ATTRS
            .iter()
            .filter(|(attr, _)| attrs.contains(*attr))
            .fold(0, |mask, (_, native)| mask | native))
    }

    fn flags_mask(&self, reparse: bool) -> u32 {
        let mut flags = 0;
        if self.extra.backup {
            flags |= FILE_FLAG_BACKUP_SEMANTICS;
        }
        // As std does for an exclusive create, so a dangling link is not
        // followed to create its target.
        if reparse || self.create_new {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        flags
    }

    pub(super) async fn open_native(&self, path: PathBuf) -> Result<StdFile> {
        if self.extra.mode.is_some() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "mode cannot be set on this platform",
            ));
        }
        let access = self.access_mask();
        let share = self.share_mask();
        let attrs = self.attrs_mask()?;
        let flags = [self.flags_mask(false), self.flags_mask(true)];
        let (create, create_new, truncate) = (self.create, self.create_new, self.truncate);
        let sec_desc = self.extra.sec_desc.clone();
        let resolve = self.resolve;
        tokio::task::spawn_blocking(move || {
            Direct::open_resolved(&path, resolve, |reparse| {
                let flags = flags[usize::from(reparse)];
                if let Some(sec_desc) = &sec_desc {
                    return create_file_with_sec_desc(
                        &path,
                        access,
                        share,
                        (create, create_new, truncate),
                        attrs | flags,
                        sec_desc,
                    );
                }
                let mut opts = StdOpenOptions::new();
                // The explicit access mask decides the rights; std's `write`
                // only gets past its check that creating or truncating needs
                // write access, which `CreateFileW` itself does not require.
                opts.write(true)
                    .access_mode(access)
                    .share_mode(share)
                    .create(create)
                    .create_new(create_new)
                    .truncate(truncate)
                    .attributes(attrs)
                    .custom_flags(flags);
                Ok(opts.open(&path)?)
            })
        })
        .await
        .map_err(Error::other)?
    }
}

/// Opens `path` with `CreateFileW`, giving a created file `sec_desc`, which
/// std has no way to pass. Mirrors std's handling of the rest.
fn create_file_with_sec_desc(
    path: &Path,
    access: u32,
    share: u32,
    (create, create_new, truncate): (bool, bool, bool),
    flags_and_attrs: u32,
    sec_desc: &SecDesc,
) -> Result<StdFile> {
    // `CREATE_ALWAYS` replaces an existing file's attributes and fails on a
    // hidden one, so emulate it as std does: `OPEN_ALWAYS`, then truncate.
    let disposition = match (create, truncate, create_new) {
        (_, _, true) => CREATE_NEW,
        (false, false, false) => OPEN_EXISTING,
        (true, _, false) => OPEN_ALWAYS,
        (false, true, false) => TRUNCATE_EXISTING,
    };
    // The self-relative descriptor's offsets assume pointer alignment.
    let bytes = sec_desc.to_bytes();
    let mut storage = vec![0u64; bytes.len().div_ceil(8)];
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), storage.as_mut_ptr().cast(), bytes.len());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap(),
        lpSecurityDescriptor: storage.as_mut_ptr().cast(),
        bInheritHandle: 0,
    };
    let path = Direct::path_wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            share,
            &raw const attributes,
            disposition,
            flags_and_attrs,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(Error::last_os_error());
    }
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let file = StdFile::from(unsafe { OwnedHandle::from_raw_handle(handle) });
    if truncate && disposition == OPEN_ALWAYS && existed {
        file.set_len(0)?;
    }
    Ok(file)
}

/// Size of the reparse data buffer header: tag, data length and a reserved
/// field.
const REPARSE_HEADER_LEN: usize = 8;

/// Size of the `REPARSE_GUID_DATA_BUFFER` header: the reparse data buffer
/// header followed by a GUID.
const REPARSE_GUID_HEADER_LEN: usize = REPARSE_HEADER_LEN + 16;

/// Where the path buffer starts in mount point reparse data: after the
/// substitute and print name offsets and lengths.
const MOUNT_POINT_PATH_START: usize = 8;

/// Where the path buffer starts in symbolic link reparse data: after the
/// name offsets and lengths, and the flags.
const SYMLINK_PATH_START: usize = 12;

/// The NT object namespace prefix that starts an absolute substitute name.
const NT_PREFIX: [u16; 4] = [b'\\' as u16, b'?' as u16, b'?' as u16, b'\\' as u16];

fn malformed_reparse_data() -> Error {
    Error::new(ErrorKind::InvalidData, "malformed reparse data")
}

/// Splits a reparse data buffer into its tag and its tag-specific data.
fn reparse_parts(buffer: &[u8]) -> Result<(u32, &[u8])> {
    let header = buffer
        .get(..REPARSE_HEADER_LEN)
        .ok_or_else(malformed_reparse_data)?;
    let tag = u32::from_le_bytes(header[..4].try_into().unwrap());
    let len = usize::from(u16::from_le_bytes(header[4..6].try_into().unwrap()));
    let data = buffer
        .get(REPARSE_HEADER_LEN..REPARSE_HEADER_LEN + len)
        .ok_or_else(malformed_reparse_data)?;
    Ok((tag, data))
}

/// Returns the substitute name from mount point or symbolic link reparse
/// data whose path buffer starts at `path_start`.
fn substitute_name(data: &[u8], path_start: usize) -> Result<Vec<u16>> {
    let field = |at: usize| {
        data.get(at..at + 2)
            .map(|bytes| usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
    };
    let (Some(offset), Some(len)) = (field(0), field(2)) else {
        return Err(malformed_reparse_data());
    };
    let start = path_start + offset;
    let bytes = data
        .get(start..start + len)
        .filter(|bytes| bytes.len() % 2 == 0)
        .ok_or_else(malformed_reparse_data)?;
    let (units, _) = bytes.as_chunks::<2>();
    Ok(units.iter().map(|unit| u16::from_le_bytes(*unit)).collect())
}

/// Turns an absolute substitute name into the path a user would write: `\??\`
/// becomes the verbatim `\\?\`, which is then dropped where that is safe.
/// Any other name is returned as it is.
fn user_path(name: &[u16]) -> PathBuf {
    if !name.starts_with(&NT_PREFIX) {
        return PathBuf::from(OsString::from_wide(name));
    }
    let mut verbatim = name.to_vec();
    verbatim[1] = u16::from(b'\\');
    let verbatim = PathBuf::from(OsString::from_wide(&verbatim));
    dunce::simplified(&verbatim).to_path_buf()
}

/// Builds the reparse data buffer for a junction to `target`, which must be
/// absolute.
fn mount_point_buffer(target: &Path) -> Result<Vec<u8>> {
    if !target.is_absolute() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "junction target must be absolute",
        ));
    }
    // Resolves `.` and `..` as Win32 does; a verbatim path is kept as it is
    let target = std::path::absolute(target)?;
    let wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    let substitute: Vec<u16> = {
        let verbatim = [
            u16::from(b'\\'),
            u16::from(b'\\'),
            u16::from(b'?'),
            u16::from(b'\\'),
        ];
        let device = [
            u16::from(b'\\'),
            u16::from(b'\\'),
            u16::from(b'.'),
            u16::from(b'\\'),
        ];
        let unc = [u16::from(b'\\'), u16::from(b'\\')];
        let unc_prefix = "\\??\\UNC\\".encode_utf16();
        if wide.starts_with(&verbatim) || wide.starts_with(&device) {
            NT_PREFIX
                .iter()
                .copied()
                .chain(wide[4..].iter().copied())
                .collect()
        } else if wide.starts_with(&unc) {
            unc_prefix.chain(wide[2..].iter().copied()).collect()
        } else {
            NT_PREFIX
                .iter()
                .copied()
                .chain(wide.iter().copied())
                .collect()
        }
    };
    let print: Vec<u16> = dunce::simplified(&target)
        .as_os_str()
        .encode_wide()
        .collect();

    let too_long = || Error::new(ErrorKind::InvalidInput, "junction target is too long");
    let substitute_len = u16::try_from(substitute.len() * 2).map_err(|_| too_long())?;
    let print_len = u16::try_from(print.len() * 2).map_err(|_| too_long())?;
    // Both names are followed by a NUL
    let print_offset = substitute_len.checked_add(2).ok_or_else(too_long)?;
    let data_len = usize::from(print_offset) + usize::from(print_len) + 2 + MOUNT_POINT_PATH_START;
    if REPARSE_HEADER_LEN + data_len > MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize {
        return Err(too_long());
    }
    let data_len = u16::try_from(data_len).map_err(|_| too_long())?;

    let mut buffer = Vec::with_capacity(REPARSE_HEADER_LEN + usize::from(data_len));
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&data_len.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&substitute_len.to_le_bytes());
    buffer.extend_from_slice(&print_offset.to_le_bytes());
    buffer.extend_from_slice(&print_len.to_le_bytes());
    for unit in substitute.iter().chain([&0]).chain(&print).chain([&0]) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(buffer)
}
