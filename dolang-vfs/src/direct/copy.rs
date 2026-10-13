use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[cfg(windows)]
use std::os::windows::{fs::FileExt, io::AsRawHandle};
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::FileExt};

use crate::{
    error::{Error, ErrorKind, Result},
    file::{CopyDataResult, CopyDest, CopyMode},
};

use super::File;

#[derive(Default)]
struct BackendCache {
    entries: HashMap<(usize, usize), BackendEntry>,
    insertions: usize,
}

struct BackendEntry {
    src: Weak<std::fs::File>,
    dst: Weak<std::fs::File>,
    state: BackendState,
}

#[derive(Debug, Default)]
struct BackendState {
    unavailable: u8,
    #[cfg(windows)]
    resume_key: Option<[u8; 24]>,
    #[cfg(windows)]
    copychunk_limit: Option<u32>,
}

#[derive(Clone, Copy)]
enum Facility {
    Extents = 1,
    Zero = 2,
    AcceleratedCopy = 4,
    RequiredClone = 8,
}

impl BackendState {
    fn unavailable(&self, facility: Facility) -> bool {
        self.unavailable & facility as u8 != 0
    }

    fn mark_unavailable(&mut self, facility: Facility) {
        self.unavailable |= facility as u8;
    }
}

impl BackendCache {
    fn pair_state<'a>(
        &'a mut self,
        src: &Arc<std::fs::File>,
        dst: &Arc<std::fs::File>,
    ) -> &'a mut BackendState {
        let key = pair_key(src, dst);
        let matches = self.entries.get(&key).is_some_and(|entry| {
            entry.src.ptr_eq(&Arc::downgrade(src)) && entry.dst.ptr_eq(&Arc::downgrade(dst))
        });
        if !matches {
            self.entries.insert(
                key,
                BackendEntry {
                    src: Arc::downgrade(src),
                    dst: Arc::downgrade(dst),
                    state: BackendState::default(),
                },
            );
            self.insertions += 1;
            if self.insertions.is_multiple_of(64) {
                self.entries.retain(|_, entry| {
                    entry.src.strong_count() != 0 && entry.dst.strong_count() != 0
                });
            }
        }
        &mut self.entries.get_mut(&key).unwrap().state
    }
}

fn cache() -> &'static Mutex<BackendCache> {
    static CACHE: OnceLock<Mutex<BackendCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BackendCache::default()))
}

fn pair_key(src: &Arc<std::fs::File>, dst: &Arc<std::fs::File>) -> (usize, usize) {
    (Arc::as_ptr(src) as usize, Arc::as_ptr(dst) as usize)
}

fn with_pair<R>(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    f: impl FnOnce(&mut BackendState) -> R,
) -> R {
    let mut cache = cache().lock().unwrap();
    f(cache.pair_state(src, dst))
}

pub(super) fn clear_cache() {
    *cache().lock().unwrap() = BackendCache::default();
}

fn io_error() -> io::Error {
    io::Error::last_os_error()
}

#[cfg(unix)]
fn checked_offset(value: u64) -> Result<libc::off_t> {
    libc::off_t::try_from(value).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "file copy offset is outside the platform range",
        )
    })
}

#[cfg(unix)]
fn unsupported_seek(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOTSUP | libc::ENOSYS)
    )
}

#[cfg(unix)]
fn definitive_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::ENOTSUP | libc::ENOTTY)
    )
}

#[cfg(unix)]
fn seek_extent(file: &std::fs::File, offset: u64, whence: libc::c_int) -> io::Result<u64> {
    let offset = libc::off_t::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds off_t"))?;
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result < 0 {
        Err(io_error())
    } else {
        Ok(result as u64)
    }
}

#[cfg(unix)]
fn write_all_at(file: &std::fs::File, mut data: &[u8], mut offset: u64) -> Result<()> {
    while !data.is_empty() {
        let written = file.write_at(data, offset)?;
        if written == 0 {
            return Err(Error::new(
                ErrorKind::WriteZero,
                "file copy write made no progress",
            ));
        }
        data = &data[written..];
        offset += written as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn dense_copy(
    src: &std::fs::File,
    dst: &std::fs::File,
    src_offset: u64,
    dst_offset: u64,
    len: u64,
) -> Result<u64> {
    let mut data = vec![0; usize::try_from(len).expect("bounded copy length fits usize")];
    let read = src.read_at(&mut data, src_offset)?;
    if read == 0 {
        return Ok(0);
    }
    write_all_at(dst, &data[..read], dst_offset)?;
    Ok(read as u64)
}

#[cfg(unix)]
fn zero_range(dst: &std::fs::File, mut offset: u64, mut len: u64) -> Result<()> {
    let zeroes = [0; 64 * 1024];
    while len != 0 {
        let take = usize::try_from(len.min(zeroes.len() as u64)).unwrap();
        write_all_at(dst, &zeroes[..take], offset)?;
        offset += take as u64;
        len -= take as u64;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn deallocate(dst: &std::fs::File, offset: libc::off_t, len: libc::off_t) -> io::Result<()> {
    let result = unsafe {
        libc::fallocate(
            dst.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            len,
        )
    };
    if result == 0 { Ok(()) } else { Err(io_error()) }
}

#[cfg(target_os = "freebsd")]
fn deallocate(dst: &std::fs::File, offset: libc::off_t, len: libc::off_t) -> io::Result<()> {
    nix::fcntl::fspacectl_all(dst, offset, len)
        .map_err(|error| io::Error::from_raw_os_error(error as i32))
}

#[cfg(target_os = "macos")]
fn deallocate(dst: &std::fs::File, offset: libc::off_t, len: libc::off_t) -> io::Result<()> {
    let range = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset,
        fp_length: len,
    };
    let result = unsafe { libc::fcntl(dst.as_raw_fd(), libc::F_PUNCHHOLE, &range) };
    if result == 0 { Ok(()) } else { Err(io_error()) }
}

#[cfg(unix)]
fn punch_hole(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    offset: u64,
    len: u64,
) -> Result<()> {
    // Bytes beyond the current EOF are already holes. Restrict replacement to
    // existing data; a later data extent or the trailing-size fix establishes
    // the requested logical length without materializing zeroes.
    let len = len.min(dst.metadata()?.len().saturating_sub(offset));
    if len == 0 {
        return Ok(());
    }
    if with_pair(src, dst, |state| state.unavailable(Facility::Zero)) {
        return zero_range(dst, offset, len);
    }
    let offset_i = checked_offset(offset)?;
    let len_i = checked_offset(len)?;
    let Err(error) = deallocate(dst, offset_i, len_i) else {
        return Ok(());
    };
    if definitive_unsupported(&error) {
        with_pair(src, dst, |state| state.mark_unavailable(Facility::Zero));
    }
    // EINVAL can be alignment- or file-specific. Either way, zeroing is the
    // required semantic fallback; actual I/O failures remain errors.
    if definitive_unsupported(&error) || error.raw_os_error() == Some(libc::EINVAL) {
        zero_range(dst, offset, len)
    } else {
        Err(error.into())
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn copy_range(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    src_offset: u64,
    dst_offset: u64,
    len: u64,
) -> Result<Option<u64>> {
    if with_pair(src, dst, |state| {
        state.unavailable(Facility::AcceleratedCopy)
    }) {
        return Ok(None);
    }
    let mut src_at = checked_offset(src_offset)?;
    let mut dst_at = checked_offset(dst_offset)?;
    let result = unsafe {
        libc::copy_file_range(
            src.as_raw_fd(),
            &mut src_at,
            dst.as_raw_fd(),
            &mut dst_at,
            usize::try_from(len).unwrap(),
            0,
        )
    };
    if result >= 0 {
        return Ok(Some(result as u64));
    }
    let error = io_error();
    if definitive_unsupported(&error) || error.raw_os_error() == Some(libc::EXDEV) {
        with_pair(src, dst, |state| {
            state.mark_unavailable(Facility::AcceleratedCopy)
        });
        Ok(None)
    } else if error.raw_os_error() == Some(libc::EINVAL) {
        // File types and individual ranges can be incompatible without the
        // device pair lacking the facility.
        Ok(None)
    } else {
        Err(error.into())
    }
}

#[cfg(target_os = "macos")]
fn copy_range(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    _src_offset: u64,
    _dst_offset: u64,
    _len: u64,
) -> Result<Option<u64>> {
    // macOS only offers whole-file cloning. Keep range copying sparse-aware,
    // but leave path-based clone acceleration to a separate facility.
    if with_pair(src, dst, |state| {
        state.unavailable(Facility::AcceleratedCopy)
    }) {
        return Ok(None);
    }
    with_pair(src, dst, |state| {
        state.mark_unavailable(Facility::AcceleratedCopy)
    });
    Ok(None)
}

#[cfg(target_os = "linux")]
fn clone_range(
    src: &std::fs::File,
    dst: &std::fs::File,
    src_offset: u64,
    dst_offset: u64,
    len: u64,
) -> io::Result<u64> {
    let range = libc::file_clone_range {
        src_fd: src.as_raw_fd() as i64,
        src_offset,
        src_length: len,
        dest_offset: dst_offset,
    };
    let result = unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONERANGE, &range) };
    if result == 0 {
        Ok(len)
    } else {
        Err(io_error())
    }
}

#[cfg(target_os = "freebsd")]
fn clone_range(
    src: &std::fs::File,
    dst: &std::fs::File,
    src_offset: u64,
    dst_offset: u64,
    len: u64,
) -> io::Result<u64> {
    const COPY_FILE_RANGE_CLONE: libc::c_uint = 1;
    let mut src_at = libc::off_t::try_from(src_offset).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut dst_at = libc::off_t::try_from(dst_offset).map_err(|_| io::ErrorKind::InvalidInput)?;
    let result = unsafe {
        libc::copy_file_range(
            src.as_raw_fd(),
            &mut src_at,
            dst.as_raw_fd(),
            &mut dst_at,
            usize::try_from(len).unwrap(),
            COPY_FILE_RANGE_CLONE,
        )
    };
    if result < 0 {
        Err(io_error())
    } else {
        Ok(result as u64)
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn require_clone(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    src_offset: u64,
    dst_offset: u64,
    len: u64,
) -> Result<u64> {
    if with_pair(src, dst, |state| state.unavailable(Facility::RequiredClone)) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "block sharing is not supported",
        ));
    }
    match clone_range(src, dst, src_offset, dst_offset, len) {
        Ok(count) => Ok(count),
        Err(error)
            if definitive_unsupported(&error)
                || matches!(error.raw_os_error(), Some(libc::EXDEV)) =>
        {
            with_pair(src, dst, |state| {
                state.mark_unavailable(Facility::RequiredClone)
            });
            Err(Error::new(ErrorKind::Unsupported, error))
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::EBADF | libc::EISDIR)
            ) =>
        {
            Err(Error::new(ErrorKind::Unsupported, error))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
fn require_clone(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    _src_offset: u64,
    _dst_offset: u64,
    _len: u64,
) -> Result<u64> {
    if with_pair(src, dst, |state| state.unavailable(Facility::RequiredClone)) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "block sharing is not supported",
        ));
    }
    with_pair(src, dst, |state| {
        state.mark_unavailable(Facility::RequiredClone)
    });
    Err(Error::new(
        ErrorKind::Unsupported,
        "block sharing is not supported",
    ))
}

#[cfg(unix)]
fn copy_data_blocking(
    src: &Arc<std::fs::File>,
    dst: &Arc<std::fs::File>,
    src_offset: u64,
    dst_offset: u64,
    len: Option<u64>,
    mode: CopyMode,
) -> Result<CopyDataResult> {
    let src_meta = src.metadata()?;
    let dst_meta = dst.metadata()?;
    let available = src_meta.len().saturating_sub(src_offset);
    let len = len.unwrap_or(crate::file::COPY_LIMIT).min(available);
    if len == 0 {
        return Ok(CopyDataResult {
            count: 0,
            destination_end: None,
        });
    }
    checked_offset(
        src_offset
            .checked_add(len)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "source copy range overflows"))?,
    )?;
    let logical_end = dst_offset
        .checked_add(len)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "destination copy range overflows"))?;
    checked_offset(logical_end)?;
    if mode == CopyMode::Require {
        return require_clone(src, dst, src_offset, dst_offset, len).map(|count| CopyDataResult {
            count,
            destination_end: None,
        });
    }

    let extent_supported = !with_pair(src, dst, |state| state.unavailable(Facility::Extents));
    let mut cursor = 0u64;
    let mut dense = !extent_supported;
    while cursor < len {
        let source_at = src_offset + cursor;
        let data_at = if dense {
            source_at
        } else {
            match seek_extent(src, source_at, libc::SEEK_DATA) {
                Ok(at) if at < source_at => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "extent seek moved before the requested source offset",
                    ));
                }
                Ok(at) => at.min(src_offset + len),
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => src_offset + len,
                Err(error) if unsupported_seek(&error) => {
                    with_pair(src, dst, |state| state.mark_unavailable(Facility::Extents));
                    dense = true;
                    source_at
                }
                Err(error) => return Err(error.into()),
            }
        };
        if data_at > source_at {
            let hole_len = data_at - source_at;
            punch_hole(src, dst, dst_offset + cursor, hole_len)?;
            cursor += hole_len;
            if cursor == len {
                if dst_meta.len() < logical_end {
                    dst.set_len(logical_end)?;
                }
                break;
            }
        }
        let source_at = src_offset + cursor;
        let data_end = if dense {
            src_offset + len
        } else {
            match seek_extent(src, source_at, libc::SEEK_HOLE) {
                Ok(at) if at <= source_at => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "extent seek did not advance past source data",
                    ));
                }
                Ok(at) => at.min(src_offset + len),
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => src_offset + len,
                Err(error) if unsupported_seek(&error) => {
                    with_pair(src, dst, |state| state.mark_unavailable(Facility::Extents));
                    dense = true;
                    src_offset + len
                }
                Err(error) => return Err(error.into()),
            }
        };
        let data_len = data_end.saturating_sub(source_at);
        debug_assert_ne!(data_len, 0);
        let copied = if mode == CopyMode::Auto {
            copy_range(src, dst, source_at, dst_offset + cursor, data_len)?.map_or_else(
                || dense_copy(src, dst, source_at, dst_offset + cursor, data_len),
                Ok,
            )?
        } else {
            dense_copy(src, dst, source_at, dst_offset + cursor, data_len)?
        };
        cursor += copied;
        if copied < data_len {
            break;
        }
    }
    Ok(CopyDataResult {
        count: cursor,
        destination_end: None,
    })
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        mem::{size_of, size_of_val},
        os::windows::fs::MetadataExt,
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_BAD_NET_NAME, ERROR_INVALID_FUNCTION,
            ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NOT_SAME_DEVICE, ERROR_NOT_SUPPORTED,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_INTEGRITY_STREAM,
            FILE_ATTRIBUTE_SPARSE_FILE, GetFileInformationByHandle,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                DUPLICATE_EXTENTS_DATA, DUPLICATE_EXTENTS_DATA_EX,
                DUPLICATE_EXTENTS_DATA_EX_SOURCE_ATOMIC, FILE_ALLOCATED_RANGE_BUFFER,
                FILE_ZERO_DATA_INFORMATION, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE_EX, FSCTL_QUERY_ALLOCATED_RANGES,
                FSCTL_SET_ZERO_DATA,
            },
        },
    };

    // These network filesystem controls and their buffers are documented by
    // MS-SMB2 but are not currently projected by windows-sys.
    const FSCTL_SRV_REQUEST_RESUME_KEY: u32 = 0x0014_0078;
    const FSCTL_SRV_COPYCHUNK: u32 = 0x0014_40f2;
    const FSCTL_SRV_COPYCHUNK_WRITE: u32 = 0x0014_80f2;
    const COPYCHUNK_INITIAL_LIMIT: u32 = 1024 * 1024;

    #[repr(C)]
    struct CopyChunkRequest {
        source_key: [u8; 24],
        chunk_count: u32,
        reserved: u32,
        source_offset: i64,
        destination_offset: i64,
        length: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct CopyChunkResponse {
        chunks_written: u32,
        chunk_bytes_written: u32,
        total_bytes_written: u32,
    }

    fn ioctl<I, O>(file: &std::fs::File, code: u32, input: &I, output: &mut O) -> io::Result<u32> {
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                ptr::from_ref(input).cast(),
                u32::try_from(size_of::<I>()).unwrap(),
                ptr::from_mut(output).cast(),
                u32::try_from(size_of::<O>()).unwrap(),
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io_error())
        } else {
            Ok(returned)
        }
    }

    fn ioctl_no_output<I>(file: &std::fs::File, code: u32, input: &I) -> io::Result<()> {
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                ptr::from_ref(input).cast(),
                u32::try_from(size_of::<I>()).unwrap(),
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 { Err(io_error()) } else { Ok(()) }
    }

    fn stable_unsupported(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error().map(|value| value as u32),
            Some(
                ERROR_INVALID_FUNCTION
                    | ERROR_NOT_SUPPORTED
                    | ERROR_NOT_SAME_DEVICE
                    | ERROR_BAD_NET_NAME
            )
        )
    }

    fn checked_i64(value: u64, what: &'static str) -> Result<i64> {
        i64::try_from(value)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, format!("{what} exceeds i64")))
    }

    fn write_all_at(file: &std::fs::File, mut data: &[u8], mut offset: u64) -> Result<()> {
        while !data.is_empty() {
            let written = file.seek_write(data, offset)?;
            if written == 0 {
                return Err(Error::new(
                    ErrorKind::WriteZero,
                    "file copy write made no progress",
                ));
            }
            data = &data[written..];
            offset += written as u64;
        }
        Ok(())
    }

    fn dense_copy(
        src: &std::fs::File,
        dst: &std::fs::File,
        src_offset: u64,
        dst_offset: u64,
        len: u64,
    ) -> Result<u64> {
        let mut data = vec![0; usize::try_from(len).expect("bounded copy length fits usize")];
        let read = src.seek_read(&mut data, src_offset)?;
        if read != 0 {
            write_all_at(dst, &data[..read], dst_offset)?;
        }
        Ok(read as u64)
    }

    fn write_zeroes(dst: &std::fs::File, mut offset: u64, mut len: u64) -> Result<()> {
        let zeroes = [0; 64 * 1024];
        while len != 0 {
            let take = usize::try_from(len.min(zeroes.len() as u64)).unwrap();
            write_all_at(dst, &zeroes[..take], offset)?;
            offset += take as u64;
            len -= take as u64;
        }
        Ok(())
    }

    fn zero_range(
        src: &Arc<std::fs::File>,
        dst: &Arc<std::fs::File>,
        offset: u64,
        len: u64,
    ) -> Result<()> {
        let len = len.min(dst.metadata()?.len().saturating_sub(offset));
        if len == 0 {
            return Ok(());
        }
        if with_pair(src, dst, |state| state.unavailable(Facility::Zero)) {
            return write_zeroes(dst, offset, len);
        }
        let input = FILE_ZERO_DATA_INFORMATION {
            FileOffset: checked_i64(offset, "zero-range offset")?,
            BeyondFinalZero: checked_i64(offset + len, "zero-range end")?,
        };
        match ioctl_no_output(dst, FSCTL_SET_ZERO_DATA, &input) {
            Ok(_) => Ok(()),
            Err(error)
                if stable_unsupported(&error)
                    || error.raw_os_error().map(|v| v as u32) == Some(ERROR_INVALID_PARAMETER) =>
            {
                if stable_unsupported(&error) {
                    with_pair(src, dst, |state| state.mark_unavailable(Facility::Zero));
                }
                write_zeroes(dst, offset, len)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn allocated_ranges(
        src: &Arc<std::fs::File>,
        dst: &Arc<std::fs::File>,
        offset: u64,
        len: u64,
    ) -> Result<Option<Vec<(u64, u64)>>> {
        let attrs = src.metadata()?.file_attributes();
        if attrs & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED) == 0
            || with_pair(src, dst, |state| state.unavailable(Facility::Extents))
        {
            return Ok(None);
        }
        let end = offset + len;
        let mut cursor = offset;
        let mut ranges = Vec::new();
        while cursor < end {
            let input = FILE_ALLOCATED_RANGE_BUFFER {
                FileOffset: checked_i64(cursor, "extent offset")?,
                Length: checked_i64(end - cursor, "extent length")?,
            };
            let mut output = [FILE_ALLOCATED_RANGE_BUFFER::default(); 64];
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    src.as_raw_handle(),
                    FSCTL_QUERY_ALLOCATED_RANGES,
                    ptr::from_ref(&input).cast(),
                    size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                    output.as_mut_ptr().cast(),
                    size_of_val(&output) as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            let error = (ok == 0).then(io_error);
            if let Some(error) = &error
                && error.raw_os_error().map(|v| v as u32) != Some(ERROR_MORE_DATA)
            {
                if stable_unsupported(error)
                    || error.raw_os_error().map(|v| v as u32) == Some(ERROR_INVALID_PARAMETER)
                {
                    with_pair(src, dst, |state| state.mark_unavailable(Facility::Extents));
                    return Ok(None);
                }
                return Err(io::Error::from_raw_os_error(error.raw_os_error().unwrap()).into());
            }
            let count = returned as usize / size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
            if count == 0 {
                break;
            }
            for range in &output[..count] {
                let start = u64::try_from(range.FileOffset)
                    .unwrap_or(0)
                    .max(offset)
                    .min(end);
                let range_end = u64::try_from(range.FileOffset.saturating_add(range.Length))
                    .unwrap_or(u64::MAX)
                    .max(start)
                    .min(end);
                if start < range_end {
                    ranges.push((start, range_end));
                }
            }
            let next = ranges.last().map_or(end, |(_, end)| *end);
            if next <= cursor {
                break;
            }
            cursor = next;
            if error.is_none() {
                break;
            }
        }
        Ok(Some(ranges))
    }

    fn file_info(file: &std::fs::File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        if ok == 0 { Err(io_error()) } else { Ok(info) }
    }

    fn clone_range(
        src: &Arc<std::fs::File>,
        dst: &Arc<std::fs::File>,
        src_offset: u64,
        dst_offset: u64,
        len: u64,
    ) -> Result<Option<u64>> {
        if with_pair(src, dst, |state| state.unavailable(Facility::RequiredClone)) {
            return Ok(None);
        }
        let src_info = file_info(src)?;
        let dst_info = file_info(dst)?;
        let src_attrs = src_info.dwFileAttributes;
        let dst_attrs = dst_info.dwFileAttributes;
        let cluster = super::super::Direct::allocation_unit_size(src)?;
        if src_info.dwVolumeSerialNumber != dst_info.dwVolumeSerialNumber
            || (src_attrs & FILE_ATTRIBUTE_SPARSE_FILE != 0
                && dst_attrs & FILE_ATTRIBUTE_SPARSE_FILE == 0)
            || (src_attrs & FILE_ATTRIBUTE_INTEGRITY_STREAM)
                != (dst_attrs & FILE_ATTRIBUTE_INTEGRITY_STREAM)
            || cluster == 0
            || !src_offset.is_multiple_of(cluster)
            || !dst_offset.is_multiple_of(cluster)
            || !len.is_multiple_of(cluster)
        {
            return Ok(None);
        }
        let old_len = dst.metadata()?.len();
        let end = dst_offset
            .checked_add(len)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "destination range overflows"))?;
        if end > old_len {
            dst.set_len(end)?;
        }
        let ex = DUPLICATE_EXTENTS_DATA_EX {
            Size: size_of::<DUPLICATE_EXTENTS_DATA_EX>(),
            FileHandle: src.as_raw_handle(),
            SourceFileOffset: checked_i64(src_offset, "source offset")?,
            TargetFileOffset: checked_i64(dst_offset, "destination offset")?,
            ByteCount: checked_i64(len, "clone length")?,
            Flags: DUPLICATE_EXTENTS_DATA_EX_SOURCE_ATOMIC,
        };
        let first = ioctl_no_output(dst, FSCTL_DUPLICATE_EXTENTS_TO_FILE_EX, &ex);
        let result = match first {
            Err(error) if stable_unsupported(&error) => {
                let basic = DUPLICATE_EXTENTS_DATA {
                    FileHandle: ex.FileHandle,
                    SourceFileOffset: ex.SourceFileOffset,
                    TargetFileOffset: ex.TargetFileOffset,
                    ByteCount: ex.ByteCount,
                };
                ioctl_no_output(dst, FSCTL_DUPLICATE_EXTENTS_TO_FILE, &basic)
            }
            other => other,
        };
        match result {
            Ok(_) => Ok(Some(len)),
            Err(clone_error) => {
                if end > old_len
                    && let Err(rollback) = dst.set_len(old_len)
                {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "failed to restore destination after clone error {clone_error}: {rollback}"
                        ),
                    ));
                }
                if stable_unsupported(&clone_error) {
                    with_pair(src, dst, |state| {
                        state.mark_unavailable(Facility::RequiredClone)
                    });
                    Ok(None)
                } else if clone_error.raw_os_error().map(|v| v as u32)
                    == Some(ERROR_INVALID_PARAMETER)
                {
                    Ok(None)
                } else {
                    Err(clone_error.into())
                }
            }
        }
    }

    fn resume_key(src: &Arc<std::fs::File>, dst: &Arc<std::fs::File>) -> Result<Option<[u8; 24]>> {
        if let Some(key) = with_pair(src, dst, |state| state.resume_key) {
            return Ok(Some(key));
        }
        let mut response = [0u8; 32];
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                src.as_raw_handle(),
                FSCTL_SRV_REQUEST_RESUME_KEY,
                ptr::null(),
                0,
                response.as_mut_ptr().cast(),
                response.len() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok != 0 && returned >= 24 {
            let mut key = [0u8; 24];
            key.copy_from_slice(&response[..24]);
            with_pair(src, dst, |state| state.resume_key = Some(key));
            Ok(Some(key))
        } else {
            let error = io_error();
            if stable_unsupported(&error)
                || matches!(
                    error.raw_os_error().map(|value| value as u32),
                    Some(ERROR_ACCESS_DENIED | ERROR_INVALID_PARAMETER)
                )
            {
                // Unlike the clone and CopyChunk requests, the resume-key
                // probe has no range or alignment parameters.  Windows uses
                // ERROR_INVALID_PARAMETER for local and otherwise
                // incompatible handles, so it is stable for this pair.
                with_pair(src, dst, |state| {
                    state.mark_unavailable(Facility::AcceleratedCopy)
                });
                Ok(None)
            } else {
                Err(error.into())
            }
        }
    }

    pub(super) fn copychunk(
        src: &Arc<std::fs::File>,
        dst: &Arc<std::fs::File>,
        dst_readable: bool,
        src_offset: u64,
        dst_offset: u64,
        len: u64,
    ) -> Result<Option<u64>> {
        if with_pair(src, dst, |state| {
            state.unavailable(Facility::AcceleratedCopy)
        }) {
            return Ok(None);
        }
        let Some(key) = resume_key(src, dst)? else {
            return Ok(None);
        };
        let limit = with_pair(src, dst, |state| {
            state.copychunk_limit.unwrap_or(COPYCHUNK_INITIAL_LIMIT)
        });
        let take = u32::try_from(len.min(u64::from(limit))).unwrap();
        let request = CopyChunkRequest {
            source_key: key,
            chunk_count: 1,
            reserved: 0,
            source_offset: checked_i64(src_offset, "source offset")?,
            destination_offset: checked_i64(dst_offset, "destination offset")?,
            length: take,
        };
        let mut response = CopyChunkResponse::default();
        let code = if dst_readable {
            FSCTL_SRV_COPYCHUNK
        } else {
            FSCTL_SRV_COPYCHUNK_WRITE
        };
        match ioctl(dst, code, &request, &mut response) {
            Ok(_) => Ok(Some(u64::from(response.total_bytes_written))),
            Err(error) => {
                if error.raw_os_error().map(|v| v as u32) == Some(ERROR_INVALID_PARAMETER)
                    && response.chunk_bytes_written != 0
                    && response.chunk_bytes_written < limit
                {
                    with_pair(src, dst, |state| {
                        state.copychunk_limit = Some(response.chunk_bytes_written)
                    });
                    return copychunk(src, dst, dst_readable, src_offset, dst_offset, len);
                }
                if error.raw_os_error().map(|v| v as u32) != Some(ERROR_INVALID_PARAMETER)
                    && response.total_bytes_written != 0
                {
                    return Ok(Some(u64::from(response.total_bytes_written)));
                }
                if stable_unsupported(&error)
                    || error.raw_os_error().map(|v| v as u32) == Some(ERROR_ACCESS_DENIED)
                    || error.raw_os_error().map(|v| v as u32) == Some(ERROR_INVALID_PARAMETER)
                {
                    with_pair(src, dst, |state| {
                        state.mark_unavailable(Facility::AcceleratedCopy)
                    });
                    Ok(None)
                } else {
                    Err(error.into())
                }
            }
        }
    }

    pub(super) fn copy_data_blocking(
        src: &Arc<std::fs::File>,
        dst: &Arc<std::fs::File>,
        dst_readable: bool,
        src_offset: u64,
        dst_offset: u64,
        len: Option<u64>,
        mode: CopyMode,
    ) -> Result<CopyDataResult> {
        let src_len = src.metadata()?.len();
        let len = len
            .unwrap_or(crate::file::COPY_LIMIT)
            .min(src_len.saturating_sub(src_offset));
        if len == 0 {
            return Ok(CopyDataResult {
                count: 0,
                destination_end: None,
            });
        }
        let logical_end = dst_offset
            .checked_add(len)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "destination range overflows"))?;
        checked_i64(src_offset + len, "source range")?;
        checked_i64(logical_end, "destination range")?;

        if mode != CopyMode::Never
            && let Some(count) = clone_range(src, dst, src_offset, dst_offset, len)?
        {
            return Ok(CopyDataResult {
                count,
                destination_end: None,
            });
        }
        if mode == CopyMode::Require {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "block sharing is not supported",
            ));
        }

        let ranges = allocated_ranges(src, dst, src_offset, len)?;
        let ranges = ranges.unwrap_or_else(|| vec![(src_offset, src_offset + len)]);
        let mut cursor = 0u64;
        for (data_start, data_end) in ranges {
            if data_start > src_offset + cursor {
                let hole = data_start - (src_offset + cursor);
                zero_range(src, dst, dst_offset + cursor, hole)?;
                cursor += hole;
            }
            let data_len = data_end - data_start;
            let copied = if mode == CopyMode::Auto {
                copychunk(
                    src,
                    dst,
                    dst_readable,
                    data_start,
                    dst_offset + cursor,
                    data_len,
                )?
                .map_or_else(
                    || dense_copy(src, dst, data_start, dst_offset + cursor, data_len),
                    Ok,
                )?
            } else {
                dense_copy(src, dst, data_start, dst_offset + cursor, data_len)?
            };
            cursor += copied;
            if copied < data_len {
                return Ok(CopyDataResult {
                    count: cursor,
                    destination_end: None,
                });
            }
        }
        if cursor < len {
            zero_range(src, dst, dst_offset + cursor, len - cursor)?;
            cursor = len;
        }
        if dst.metadata()?.len() < logical_end {
            dst.set_len(logical_end)?;
        }
        Ok(CopyDataResult {
            count: cursor,
            destination_end: None,
        })
    }
}

impl File {
    pub(crate) async fn copy_data(
        &self,
        dst: &Self,
        src_offset: u64,
        target: CopyDest,
        len: Option<u64>,
        mode: CopyMode,
    ) -> Result<CopyDataResult> {
        let CopyDest::At(dst_offset) = target else {
            unreachable!("append copies use the representation-independent path")
        };
        #[cfg(windows)]
        let dst_readable = dst.flags.contains(super::FileFlags::READ);
        let src = Arc::clone(&self.file);
        let dst = Arc::clone(&dst.file);
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                copy_data_blocking(&src, &dst, src_offset, dst_offset, len, mode)
            }
            #[cfg(windows)]
            {
                windows::copy_data_blocking(
                    &src,
                    &dst,
                    dst_readable,
                    src_offset,
                    dst_offset,
                    len,
                    mode,
                )
            }
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("file copy worker failed")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Arc<std::fs::File> {
        // These tests only use distinct Arc/file-handle identities.
        Arc::new(std::fs::File::open(std::env::current_exe().unwrap()).unwrap())
    }

    #[test]
    fn pair_cache_is_ordered_and_facilities_are_independent() {
        let a = file();
        let b = file();
        let mut cache = BackendCache::default();
        cache.pair_state(&a, &b).mark_unavailable(Facility::Extents);

        assert!(cache.pair_state(&a, &b).unavailable(Facility::Extents));
        assert!(!cache.pair_state(&a, &b).unavailable(Facility::Zero));
        assert!(!cache.pair_state(&b, &a).unavailable(Facility::Extents));
    }

    #[test]
    fn pair_cache_rejects_stale_weak_identity_at_the_same_key() {
        let src = file();
        let dst = file();
        let stale_src = file();
        let stale_dst = file();
        let key = pair_key(&src, &dst);
        let mut state = BackendState::default();
        state.mark_unavailable(Facility::RequiredClone);
        let mut cache = BackendCache::default();
        cache.entries.insert(
            key,
            BackendEntry {
                src: Arc::downgrade(&stale_src),
                dst: Arc::downgrade(&stale_dst),
                state,
            },
        );

        assert!(
            !cache
                .pair_state(&src, &dst)
                .unavailable(Facility::RequiredClone)
        );
    }

    #[test]
    fn pair_cache_periodically_trims_dead_entries() {
        let mut cache = BackendCache::default();
        let mut files = Vec::new();
        for _ in 0..63 {
            let src = file();
            let dst = file();
            cache.pair_state(&src, &dst);
            files.push((src, dst));
        }
        assert_eq!(cache.entries.len(), 63);
        drop(files);
        let src = file();
        let dst = file();
        cache.pair_state(&src, &dst);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn clear_cache_drops_all_pair_state() {
        clear_cache();
        let src = file();
        let dst = file();
        with_pair(&src, &dst, |state| {
            state.mark_unavailable(Facility::AcceleratedCopy)
        });
        assert_eq!(cache().lock().unwrap().entries.len(), 1);
        clear_cache();
        assert!(cache().lock().unwrap().entries.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn pair_cache_retains_copychunk_negotiation_state() {
        let src = file();
        let dst = file();
        let mut cache = BackendCache::default();
        let key = [7; 24];
        let state = cache.pair_state(&src, &dst);
        state.resume_key = Some(key);
        state.copychunk_limit = Some(64 * 1024);
        let state = cache.pair_state(&src, &dst);
        assert_eq!(state.resume_key, Some(key));
        assert_eq!(state.copychunk_limit, Some(64 * 1024));
    }

    #[cfg(windows)]
    #[test]
    fn smb_copychunk_moves_more_than_two_mebibytes_when_configured() {
        use std::{path::PathBuf, time::SystemTime};

        let Some(root) = std::env::var_os("DOLANG_TEST_SMB_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        assert!(
            root.as_os_str().to_string_lossy().starts_with(r"\\"),
            "DOLANG_TEST_SMB_ROOT must be a UNC root"
        );
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let src_path = root.join(format!(
            "dolang-copychunk-{}-{unique}-src",
            std::process::id()
        ));
        let dst_path = root.join(format!(
            "dolang-copychunk-{}-{unique}-dst",
            std::process::id()
        ));
        let content: Vec<u8> = (0..2_500_123u32).map(|index| (index % 251) as u8).collect();
        std::fs::write(&src_path, &content).unwrap();
        std::fs::write(&dst_path, []).unwrap();
        let src = Arc::new(std::fs::File::open(&src_path).unwrap());
        let dst = Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&dst_path)
                .unwrap(),
        );

        let mut copied = 0;
        while copied < content.len() as u64 {
            let count = windows::copychunk(
                &src,
                &dst,
                true,
                copied,
                copied,
                content.len() as u64 - copied,
            )
            .unwrap()
            .expect("configured SMB server did not support CopyChunk");
            assert_ne!(count, 0, "CopyChunk made no progress");
            copied += count;
        }
        drop(src);
        drop(dst);
        assert_eq!(std::fs::read(&dst_path).unwrap(), content);
        std::fs::remove_file(src_path).unwrap();
        std::fs::remove_file(dst_path).unwrap();
    }
}
