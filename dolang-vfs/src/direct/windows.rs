use super::{Child, Command, Direct, File, OpenOptions};
use crate::{
    directory::{DirEntry, DirEntryFamily},
    error::{Error, ErrorKind, Result},
    file::{AccessFlags, FileId, StreamEntry},
    file::{XattrEntry, XattrNamespace},
    metadata::{
        AttrFlags, AttrsPatch, FileType, FsMetadata, FsMetadataFamily, Metadata, MetadataPatch,
        WindowsFsMetadata, metadata_from_std, metadata_with_sids,
    },
    security::{Acl, AclKind, OwnershipIdentity, SidName, SidNameUse},
};
use dolang_winterop::security::{SecDesc, SecDescControl, SecInfo, Sid};
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
    fs::{self, OpenOptions as TokioOpenOptions},
    time::{Duration, timeout},
};
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_FULL_EA_INFORMATION, FILE_GET_EA_INFORMATION, FILE_RENAME_POSIX_SEMANTICS,
        FILE_RENAME_REPLACE_IF_EXISTS, NtQueryEaFile, NtSetEaFile,
    },
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_INSUFFICIENT_BUFFER,
            ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NONE_MAPPED,
            ERROR_NOT_SUPPORTED, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
            RtlNtStatusToDosError, S_OK, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
            STATUS_NO_EAS_ON_FILE, STATUS_NO_MORE_EAS, STATUS_SUCCESS,
        },
        Security::{
            ACL,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
            GetSecurityDescriptorLength, LookupAccountNameW, LookupAccountSidW,
            PROTECTED_DACL_SECURITY_INFORMATION, PROTECTED_SACL_SECURITY_INFORMATION,
            SidTypeAlias as SID_TYPE_ALIAS, SidTypeComputer as SID_TYPE_COMPUTER,
            SidTypeDeletedAccount as SID_TYPE_DELETED_ACCOUNT, SidTypeDomain as SID_TYPE_DOMAIN,
            SidTypeGroup as SID_TYPE_GROUP, SidTypeInvalid as SID_TYPE_INVALID,
            SidTypeLabel as SID_TYPE_LABEL, SidTypeLogonSession as SID_TYPE_LOGON_SESSION,
            SidTypeUnknown as SID_TYPE_UNKNOWN, SidTypeUser as SID_TYPE_USER,
            SidTypeWellKnownGroup as SID_TYPE_WELL_KNOWN_GROUP,
            UNPROTECTED_DACL_SECURITY_INFORMATION, UNPROTECTED_SACL_SECURITY_INFORMATION,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, COMPRESSION_FORMAT_DEFAULT, COMPRESSION_FORMAT_NONE,
            CreateFileW, DELETE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN,
            FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO,
            FILE_RENAME_INFO_0, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FILE_STREAM_INFO, FileRenameInfo, FileRenameInfoEx, FileStreamInfo,
            GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetFileAttributesW, GetFileInformationByHandle,
            GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
            INVALID_FILE_ATTRIBUTES, MAXIMUM_REPARSE_DATA_BUFFER_SIZE, OPEN_EXISTING, READ_CONTROL,
            SetFileAttributesW, SetFileInformationByHandle, VOLUME_NAME_DOS, WRITE_DAC,
            WRITE_OWNER,
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
                FILE_SET_SPARSE_BUFFER, FSCTL_GET_REPARSE_POINT, FSCTL_SET_COMPRESSION,
                FSCTL_SET_REPARSE_POINT, FSCTL_SET_SPARSE,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JobObjectBasicAccountingInformation, QueryInformationJobObject, TerminateJobObject,
            },
            SystemServices::ACCESS_SYSTEM_SECURITY,
            Threading::{
                CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenThread, ResumeThread,
                THREAD_SUSPEND_RESUME,
            },
        },
        UI::Shell::{
            FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData, KF_FLAG_DONT_VERIFY,
            SHGetKnownFolderPath,
        },
    },
    core::GUID,
};

#[derive(Debug)]
pub(crate) struct ReadDir {
    inner: Box<tokio::fs::ReadDir>,
}

impl ReadDir {
    pub(super) async fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            inner: Box::new(tokio::fs::read_dir(path).await?),
        })
    }

    pub(crate) async fn next_entry(&mut self) -> Result<Option<DirEntry>> {
        let Some(entry) = self.inner.next_entry().await? else {
            return Ok(None);
        };
        let file_type = entry.file_type().await?;
        let file_type = if file_type.is_file() {
            FileType::File
        } else if file_type.is_dir() {
            FileType::Dir
        } else if file_type.is_symlink() {
            FileType::Symlink
        } else {
            FileType::Unknown
        };
        let file_name = entry.file_name().into_string().map_err(|_| {
            Error::new(ErrorKind::InvalidData, "directory entry is not valid UTF-8")
        })?;
        Ok(Some(DirEntry::new(
            file_name,
            file_type,
            DirEntryFamily::Windows,
        )))
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
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
        // SAFETY: the handle is borrowed from a live `File` and `info` is a
        // correctly sized, writable output buffer.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) };
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
        _follow: bool,
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
        _follow: bool,
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

    fn security_handle(path: &Path, access: u32, follow: bool) -> Result<OwnedHandle> {
        let path = Self::path_wide(path);
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
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
        follow: bool,
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
        let handle = Self::security_handle(path, access, follow)?;
        Self::sec_desc_from_handle(handle.as_handle(), mask)
    }

    pub(super) fn update_sec_desc_path(
        path: &Path,
        descriptor: &SecDesc,
        follow: bool,
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
        let handle = Self::security_handle(path, access, follow)?;
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

    fn fs_query_root_metadata(root: &Path) -> Result<(u64, u64, u64, u32, u32, u32)> {
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
            return Err(Error::last_os_error());
        }

        Ok((available, capacity, free, serial, max_component, flags))
    }

    fn fs_metadata_from_handle(handle: BorrowedHandle<'_>) -> Result<FsMetadata> {
        let root = Self::volume_root_path(&Self::final_path_from_handle(handle)?)?;
        let (available, capacity, free, serial, max_component, flags) =
            Self::fs_query_root_metadata(&root)?;

        Ok(FsMetadata {
            capacity,
            free,
            available,
            block_size: 0,
            family: FsMetadataFamily::Windows(WindowsFsMetadata {
                flags,
                volume_serial_number: serial,
                component_length_max: max_component,
            }),
        })
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
        Ok(metadata_with_sids(
            metadata_from_std(metadata),
            descriptor.owner().cloned(),
            descriptor.group().cloned(),
        ))
    }

    pub(super) fn metadata_from_path(path: &Path, follow: bool) -> Result<Metadata> {
        let metadata = if follow {
            std::fs::metadata(path)?
        } else {
            std::fs::symlink_metadata(path)?
        };
        let descriptor = Self::sec_desc_from_path(path, SecInfo::OWNER | SecInfo::GROUP, follow)?;
        Ok(metadata_with_sids(
            metadata_from_std(metadata),
            descriptor.owner().cloned(),
            descriptor.group().cloned(),
        ))
    }

    pub(super) fn fs_metadata_from_path(path: &Path, follow: bool) -> Result<FsMetadata> {
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
            family: FsMetadataFamily::Windows(WindowsFsMetadata {
                flags,
                volume_serial_number: serial,
                component_length_max: max_component,
            }),
        })
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

    pub(super) fn open_for_metadata(path: &Path, follow: bool) -> Result<Arc<StdFile>> {
        let handle = Self::security_handle(path, 0, follow)?;
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
        for key in ["TMP", "TEMP"] {
            if let Some(value) = override_value(key) {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    return Ok(path);
                }
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{key} must be an absolute path"),
                ));
            }
        }
        Ok(std::env::temp_dir())
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

    pub(super) async fn impl_copy_symlink(src: &Path, dst: &Path) -> Result<()> {
        let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
        tokio::task::spawn_blocking(move || Self::copy_reparse_point_sync(&src, &dst))
            .await
            .unwrap_or_else(|_| Err(Error::other("copy symlink task failed")))
    }

    /// Duplicates a reparse point (symlink, junction, or otherwise) byte for
    /// byte, rather than reinterpreting its target. This preserves details
    /// (relative vs. absolute, print name vs. substitute name, and the
    /// reparse tag itself) that re-deriving a fresh reparse point from a
    /// resolved target path cannot recover.
    fn copy_reparse_point_sync(src: &Path, dst: &Path) -> Result<()> {
        let is_dir = std::fs::symlink_metadata(src)?.is_dir();

        let mut read_opts = StdOpenOptions::new();
        read_opts
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let src_file = read_opts.open(src)?;

        let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
        let mut bytes_returned = 0u32;
        if unsafe {
            DeviceIoControl(
                src_file.as_raw_handle(),
                FSCTL_GET_REPARSE_POINT,
                ptr::null(),
                0,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len()).unwrap(),
                &mut bytes_returned,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(Error::last_os_error());
        }
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

        let mut written = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                dst_file.as_raw_handle(),
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                bytes_returned,
                ptr::null_mut(),
                0,
                &mut written,
                ptr::null_mut(),
            )
        };
        drop(dst_file);
        if ok == 0 {
            let err = Error::last_os_error();
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

    pub(super) async fn impl_xattrs(
        &self,
        path: &Path,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>> {
        let file = self
            .direct_open_options()
            .read(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_xattrs(&file.file, namespace).await
    }

    pub(super) async fn impl_streams(&self, path: &Path, follow: bool) -> Result<Vec<StreamEntry>> {
        let file = Self::open_for_metadata(path, follow)?;
        Self::impl_file_streams(&file).await
    }

    pub(super) async fn impl_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>> {
        let file = self
            .direct_open_options()
            .read(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_xattr(&file.file, name, namespace).await
    }

    pub(super) async fn impl_set_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<()> {
        let file = self
            .direct_open_options()
            .write(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_set_xattr(&file.file, name, namespace, value).await
    }

    pub(super) async fn impl_remove_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<()> {
        let file = self
            .direct_open_options()
            .read(true)
            .write(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::error::Error::into_io_error)?;
        Self::impl_file_remove_xattr(&file.file, name, namespace).await
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
        if !patch.follow && !patch.attrs.is_empty() {
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
                    Self::update_sec_desc_path(&path, descriptor, patch.follow)?;
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
                        patch.follow,
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
        follow: bool,
    ) -> Result<()> {
        use std::{fs::FileTimes, os::windows::fs::FileTimesExt};
        use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

        let mut opts = StdOpenOptions::new();
        opts.access_mode(FILE_WRITE_ATTRIBUTES);
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        opts.custom_flags(flags);
        let file = opts.open(path)?;
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

impl OpenOptions {
    pub(super) fn apply_no_follow_flags(&self, opts: &mut TokioOpenOptions) {
        if self.no_follow {
            opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
    }
}
