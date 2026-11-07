//! Reading Windows directories through a handle, which reports each entry's
//! reparse tag where std reports only whether it is a name surrogate.

use std::{
    ffi::OsString,
    mem,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO,
        FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileFullDirectoryInfo, GetFileInformationByHandleEx, OPEN_EXISTING, SYNCHRONIZE,
    },
};

use crate::{
    error::{Error, Result},
    metadata::{FileType, Resolve},
};

/// One entry of a directory.
pub(super) struct RawEntry {
    pub(super) name: OsString,
    pub(super) attrs: u32,
    /// The reparse tag, if the entry is a reparse point.
    pub(super) tag: Option<u32>,
}

impl RawEntry {
    /// Returns the entry's type as the OS reports it: a symlink only for a
    /// tag that `Resolve::Link` opens as itself, so a junction is a directory.
    pub(super) fn file_type(&self) -> FileType {
        if self.tag.map(Resolve::of_reparse_tag) == Some(Resolve::Link) {
            FileType::Symlink
        } else if self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            FileType::Dir
        } else {
            FileType::File
        }
    }
}

/// Opens the directory at `path` for listing. With `reparse`, a final reparse
/// point is opened as itself. Without `share_delete`, the directory can't be
/// renamed or deleted while the handle is open, so a walk holding it can't be
/// redirected through a link swapped in for it.
pub(super) fn open_dir(path: &Path, reparse: bool, share_delete: bool) -> Result<OwnedHandle> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if reparse {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | if share_delete { FILE_SHARE_DELETE } else { 0 },
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Lists a directory handle's entries, without `.` and `..`.
#[derive(Debug)]
pub(super) struct DirIter {
    handle: OwnedHandle,
    /// `FILE_FULL_DIR_INFO` records, which need 8-byte alignment.
    buf: Vec<u64>,
    /// Byte offset of the next record in `buf`, or `None` to fetch more.
    next: Option<usize>,
    done: bool,
}

impl DirIter {
    pub(super) fn new(handle: OwnedHandle) -> Self {
        Self {
            handle,
            buf: vec![0; 8192],
            next: None,
            done: false,
        }
    }

    fn fill(&mut self) -> Result<bool> {
        let status = unsafe {
            GetFileInformationByHandleEx(
                self.handle.as_raw_handle(),
                FileFullDirectoryInfo,
                self.buf.as_mut_ptr().cast(),
                u32::try_from(self.buf.len() * mem::size_of::<u64>()).unwrap_or(u32::MAX),
            )
        };
        if status != 0 {
            return Ok(true);
        }
        let err = Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
            Ok(false)
        } else {
            Err(err)
        }
    }

    /// Returns the next entry, blocking.
    pub(super) fn next_entry(&mut self) -> Result<Option<RawEntry>> {
        loop {
            if self.done {
                return Ok(None);
            }
            let offset = match self.next {
                Some(offset) => offset,
                None => {
                    if !self.fill()? {
                        self.done = true;
                        return Ok(None);
                    }
                    0
                }
            };
            let base: *const u8 = self.buf.as_ptr().cast();
            // SAFETY: the kernel wrote a chain of records into `buf`, each
            // aligned and within it, linked by `NextEntryOffset`.
            let (info, name) = unsafe {
                let record = base.add(offset);
                let info = ptr::read(record.cast::<FILE_FULL_DIR_INFO>());
                let name = std::slice::from_raw_parts(
                    record
                        .add(mem::offset_of!(FILE_FULL_DIR_INFO, FileName))
                        .cast::<u16>(),
                    info.FileNameLength as usize / mem::size_of::<u16>(),
                );
                (info, OsString::from_wide(name))
            };
            self.next = (info.NextEntryOffset != 0).then(|| offset + info.NextEntryOffset as usize);
            if name == "." || name == ".." {
                continue;
            }
            // A reparse point's tag comes back in place of its EA size, as
            // `FindFirstFileExW` reports it in `dwReserved0`
            let tag =
                (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.EaSize);
            return Ok(Some(RawEntry {
                name,
                attrs: info.FileAttributes,
                tag,
            }));
        }
    }
}
