use super::{Direct, DirectChild, DirectCommand, DirectOpenOptions};
use crate::{
    Attrs, ChownIdentity, FsMetadata, OpenOptions as _, StreamEntry, Utf8TypedPath,
    Utf8WindowsPath, XattrEntry, XattrNamespace,
};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs::File as StdFile,
    io, mem,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr, slice,
    time::SystemTime,
};
use tokio::{
    fs::{self, File, OpenOptions},
    time::Duration,
};
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_FULL_EA_INFORMATION, FILE_GET_EA_INFORMATION, NtQueryEaFile, NtSetEaFile,
    },
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE,
            INVALID_HANDLE_VALUE, RtlNtStatusToDosError, S_OK, STATUS_BUFFER_OVERFLOW,
            STATUS_BUFFER_TOO_SMALL, STATUS_NO_EAS_ON_FILE, STATUS_NO_MORE_EAS, STATUS_SUCCESS,
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

fn typed_windows_path(path: &Path) -> io::Result<Utf8TypedPath<'_>> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path is not UTF-8"))?;
    Ok(Utf8TypedPath::Windows(Utf8WindowsPath::new(path)))
}

impl Direct {
    pub(super) fn program_not_found_error() -> io::Error {
        io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32)
    }

    pub(super) fn directory_requires_all_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::IsADirectory,
            "directory operations require all: true",
        )
    }

    pub(super) fn directory_not_empty_error() -> io::Error {
        io::Error::from(io::ErrorKind::DirectoryNotEmpty)
    }

    pub(super) fn not_a_directory_error() -> io::Error {
        io::Error::from(io::ErrorKind::NotADirectory)
    }

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
        Ok(dunce::simplified(&PathBuf::from(OsString::from_wide(&path))).to_path_buf())
    }

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

    fn fs_metadata_from_handle(handle: BorrowedHandle<'_>) -> io::Result<FsMetadata> {
        let root = Self::volume_root_path(&Self::final_path_from_handle(handle)?)?;
        let (available, capacity, free, serial, max_component, flags) =
            Self::fs_query_root_metadata(&root)?;

        Ok(FsMetadata {
            capacity,
            free,
            available,
            block_size: 0,
            family: crate::FsMetadataFamily::Windows(crate::WindowsFsMetadata {
                flags,
                volume_serial_number: serial,
                component_length_max: max_component,
            }),
        })
    }

    pub(super) fn fs_metadata_from_file(file: &File) -> io::Result<FsMetadata> {
        Self::fs_metadata_from_handle(file.as_handle())
    }

    pub(super) fn fs_metadata_from_path(path: &Path, follow: bool) -> io::Result<FsMetadata> {
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
            family: crate::FsMetadataFamily::Windows(crate::WindowsFsMetadata {
                flags,
                volume_serial_number: serial,
                component_length_max: max_component,
            }),
        })
    }

    fn path_wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    pub(super) fn attrs_from_path(path: PathBuf, _follow: bool) -> io::Result<Attrs> {
        let path = Self::path_wide(&path);
        let attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            Err(io::Error::last_os_error())
        } else {
            Ok(Attrs::from_win_attrs(attrs))
        }
    }

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

    pub(super) fn open_for_metadata(path: &Path, follow: bool) -> io::Result<File> {
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

    pub(super) fn set_attrs_path(path: PathBuf, patch: Attrs) -> io::Result<()> {
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

    pub(super) fn known_folder(folder_id: &GUID) -> Result<PathBuf, io::Error> {
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

    pub(super) fn home_dir_platform(
        _env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        Self::known_folder(&FOLDERID_Profile)
    }

    pub(super) fn cache_dir_platform(
        _env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
        Self::known_folder(&FOLDERID_LocalAppData)
    }

    pub(super) fn temp_dir_platform(
        env: &HashMap<String, Option<String>>,
    ) -> Result<PathBuf, io::Error> {
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
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{key} must be an absolute path"),
                ));
            }
        }
        Ok(std::env::temp_dir())
    }

    fn nt_error(status: windows_sys::Win32::Foundation::NTSTATUS) -> io::Error {
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
    }

    pub(super) fn windows_xattr_name(name: &str, namespace: Option<&str>) -> io::Result<Vec<u8>> {
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

    const fn align_windows_ea(len: usize) -> usize {
        (len + 3) & !3
    }

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

    pub(super) unsafe fn windows_list_xattrs(
        handle: BorrowedHandle<'_>,
    ) -> io::Result<Vec<XattrEntry>> {
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
    ) -> io::Result<Vec<u8>> {
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

    pub(super) unsafe fn windows_set_xattr(
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
                unsafe { slice::from_raw_parts(info.StreamName.as_ptr(), name_len / 2) };
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

    pub(super) unsafe fn windows_list_streams(
        handle: BorrowedHandle<'_>,
    ) -> io::Result<Vec<StreamEntry>> {
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

    pub(super) async fn impl_symlink(cwd: &Path, src: &Path, dst: &Path) -> io::Result<()> {
        let metadata = fs::metadata(cwd.join(src)).await?;
        if metadata.is_dir() {
            Self::impl_symlink_dir(src, dst).await
        } else {
            Self::impl_symlink_file(src, dst).await
        }
    }

    pub(super) async fn impl_symlink_dir(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink_dir(src, dst).await
    }

    pub(super) async fn impl_symlink_file(src: &Path, dst: &Path) -> io::Result<()> {
        fs::symlink_file(src, dst).await
    }

    pub(super) async fn impl_xattrs(
        &self,
        path: &Path,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>, io::Error> {
        let file = self
            .direct_open_options()
            .read(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::Error::into_io_error)?;
        self.impl_file_xattrs(&file.0, namespace).await
    }

    pub(super) async fn impl_streams(
        &self,
        path: &Path,
        follow: bool,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        let file = Self::open_for_metadata(path, follow)?;
        self.impl_file_streams(&file).await
    }

    pub(super) async fn impl_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>, io::Error> {
        let file = self
            .direct_open_options()
            .read(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::Error::into_io_error)?;
        self.impl_file_xattr(&file.0, name, namespace).await
    }

    pub(super) async fn impl_set_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<(), io::Error> {
        let file = self
            .direct_open_options()
            .write(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::Error::into_io_error)?;
        self.impl_file_set_xattr(&file.0, name, namespace, value)
            .await
    }

    pub(super) async fn impl_remove_xattr(
        &self,
        path: &Path,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<(), io::Error> {
        let file = self
            .direct_open_options()
            .read(true)
            .write(true)
            .no_follow(!follow)
            .open(typed_windows_path(path)?)
            .await
            .map_err(crate::Error::into_io_error)?;
        self.impl_file_remove_xattr(&file.0, name, namespace).await
    }

    pub(super) async fn impl_file_xattrs(
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

    pub(super) async fn impl_file_xattr(
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

    pub(super) async fn impl_file_streams(
        &self,
        file: &File,
    ) -> Result<Vec<StreamEntry>, io::Error> {
        let file = file.try_clone().await?;
        tokio::task::spawn_blocking(move || unsafe { Self::windows_list_streams(file.as_handle()) })
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    pub(super) async fn impl_file_set_xattr(
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

    pub(super) async fn impl_file_remove_xattr(
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
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || dunce::canonicalize(path))
            .await
            .unwrap_or_else(|e| Err(io::Error::other(e)))
    }

    pub(super) async fn impl_set_permissions(
        &self,
        path: &Path,
        perm: crate::Permissions,
    ) -> Result<(), io::Error> {
        let mut permissions = fs::metadata(path).await?.permissions();
        permissions.set_readonly(perm.readonly());
        fs::set_permissions(path, permissions).await
    }

    pub(super) async fn impl_set_times(
        &self,
        path: &Path,
        accessed: Option<(i64, u32)>,
        modified: Option<(i64, u32)>,
        created: Option<(i64, u32)>,
    ) -> Result<(), io::Error> {
        use std::{
            fs::{FileTimes, OpenOptions as StdOpenOptions},
            os::windows::fs::{FileTimesExt, OpenOptionsExt},
        };
        use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;

        let path = path.to_path_buf();
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

    pub(super) async fn impl_chown(
        &self,
        path: &Path,
        user: Option<ChownIdentity>,
        group: Option<ChownIdentity>,
        follow: bool,
    ) -> Result<(), io::Error> {
        let _ = (path, user, group, follow);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "chown is not supported on this platform",
        ))
    }
}

impl Direct {
    fn direct_open_options(&self) -> DirectOpenOptions {
        DirectOpenOptions::default()
    }
}

impl DirectChild {
    pub(super) async fn impl_terminate(self) -> io::Result<std::process::ExitStatus> {
        let mut child = self.inner;
        if child.id().is_some() {
            let _ = child.start_kill();
        }
        child.wait().await
    }
}

impl DirectCommand<'_> {
    pub(super) fn impl_stderr_inherit_stdout(&mut self) -> io::Result<&mut Self> {
        self.stderr = Some(std::process::Stdio::from(
            std::io::stdout().as_handle().try_clone_to_owned()?,
        ));
        Ok(self)
    }
}

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

impl super::DirectOpenOptions {
    pub(super) fn apply_no_follow_flags(&self, opts: &mut OpenOptions) {
        if self.no_follow {
            opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
    }
}
