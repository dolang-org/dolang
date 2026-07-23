use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, OnceLock, Weak},
};

use crate::{FileLockBehavior, FileLockMode, FileLockRange, FileLockRequest};

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, OwnedHandle};

#[derive(Debug)]
pub(crate) struct DirectFileLocks {
    table: OnceLock<Arc<LockTable>>,
}

impl DirectFileLocks {
    pub(crate) fn new() -> Self {
        Self {
            table: OnceLock::new(),
        }
    }

    pub(crate) async fn acquire(
        &self,
        handle: NativeHandle,
        request: FileLockRequest,
    ) -> io::Result<Option<DirectFileLock>> {
        if request
            .range
            .end
            .is_some_and(|end| end < request.range.start)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lock range end precedes its start",
            ));
        }
        #[cfg(unix)]
        if request.range.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-length file locks are unsupported on Unix",
            ));
        }
        #[cfg(windows)]
        if request.range.start == 0 && request.range.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a zero-length Windows file lock at offset zero is unsupported",
            ));
        }
        let table = self.table.get_or_init(|| Arc::new(LockTable::default()));
        let reservation = table.reserve(request.range)?;
        let result = tokio::task::spawn_blocking(move || {
            let acquired = ArmedLock::acquire(handle, request)?;
            Ok::<_, io::Error>(acquired.map(|armed| (reservation, armed)))
        })
        .await
        .map_err(|_| io::Error::other("file lock worker failed"))??;

        let Some((reservation, armed)) = result else {
            return Ok(None);
        };
        reservation.commit(armed).map(Some)
    }
}

#[derive(Debug)]
pub(crate) struct DirectFileLock {
    table: Weak<LockTable>,
    id: u64,
}

impl DirectFileLock {
    pub(crate) async fn release(&mut self) -> crate::Result<()> {
        let Some(table) = self.table.upgrade() else {
            return Ok(());
        };
        let armed = table.take_armed(self.id)?;
        drop(table);
        let Some(armed) = armed else {
            return Ok(());
        };
        let table = self.table.clone();
        let id = self.id;
        let outcome = tokio::task::spawn_blocking(move || release(armed, table, id))
            .await
            .map_err(|_| io::Error::other("file unlock worker failed"))?;
        outcome.map_err(Into::into)
    }
}

impl Drop for DirectFileLock {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            let Ok(Some(armed)) = table.take_armed(self.id) else {
                return;
            };
            if tokio::runtime::Handle::try_current().is_ok() {
                let table = self.table.clone();
                let id = self.id;
                drop(tokio::task::spawn_blocking(move || {
                    release(armed, table, id)
                }));
            } else {
                let _ = armed.release();
                table.remove(self.id);
            }
        }
    }
}

#[derive(Debug, Default)]
struct LockTable {
    state: Mutex<LockTableState>,
}

#[derive(Debug, Default)]
struct LockTableState {
    next: u64,
    entries: HashMap<u64, LockEntry>,
}

#[derive(Debug)]
struct LockEntry {
    range: FileLockRange,
    armed: Option<ArmedLock>,
}

impl LockTable {
    fn reserve(self: &Arc<Self>, range: FileLockRange) -> io::Result<Reservation> {
        let mut state = self.state.lock().unwrap();
        if state
            .entries
            .values()
            .any(|entry| range.conflicts(entry.range))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "overlapping locks on the same file handle are unsupported",
            ));
        }
        let id = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("file lock reservation ids exhausted"))?;
        state.entries.insert(id, LockEntry { range, armed: None });
        Ok(Reservation {
            table: Arc::downgrade(self),
            id,
            committed: false,
        })
    }

    fn commit(self: &Arc<Self>, id: u64, armed: ArmedLock) -> io::Result<DirectFileLock> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .entries
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("file lock reservation was lost"))?;
        if entry.armed.is_some() {
            return Err(io::Error::other(
                "file lock reservation was already committed",
            ));
        }
        entry.armed = Some(armed);
        Ok(DirectFileLock {
            table: Arc::downgrade(self),
            id,
        })
    }

    fn remove(&self, id: u64) {
        self.state.lock().unwrap().entries.remove(&id);
    }

    fn take_armed(&self, id: u64) -> io::Result<Option<ArmedLock>> {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(&id) else {
            return Ok(None);
        };
        entry.armed.take().map(Some).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "file lock release is already in progress",
            )
        })
    }
}

fn release(armed: ArmedLock, table: Weak<LockTable>, id: u64) -> io::Result<()> {
    let outcome = armed.release();
    if let Some(table) = table.upgrade() {
        table.remove(id);
    }
    outcome
}

impl Drop for LockTable {
    fn drop(&mut self) {
        for entry in self.state.get_mut().unwrap().entries.values_mut() {
            if let Some(armed) = &mut entry.armed {
                armed.disarm();
            }
        }
    }
}

struct Reservation {
    table: Weak<LockTable>,
    id: u64,
    committed: bool,
}

impl Reservation {
    fn commit(mut self, armed: ArmedLock) -> io::Result<DirectFileLock> {
        let table = self
            .table
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "file is closed"))?;
        let result = table.commit(self.id, armed);
        self.committed = result.is_ok();
        result
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed
            && let Some(table) = self.table.upgrade()
        {
            table.remove(self.id);
        }
    }
}

#[cfg(unix)]
pub(crate) type NativeHandle = OwnedFd;
#[cfg(windows)]
pub(crate) type NativeHandle = OwnedHandle;

#[derive(Debug)]
struct ArmedLock {
    handle: Option<NativeHandle>,
    range: FileLockRange,
}

impl ArmedLock {
    fn acquire(handle: NativeHandle, request: FileLockRequest) -> io::Result<Option<Self>> {
        #[cfg(unix)]
        return Self::acquire_unix(handle, request);
        #[cfg(windows)]
        return Self::acquire_windows(handle, request);
    }

    fn release(mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        unlock(&handle, self.range)
    }

    fn disarm(&mut self) {
        self.handle.take();
    }
}

impl Drop for ArmedLock {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = unlock(&handle, self.range);
        }
    }
}

#[cfg(unix)]
impl ArmedLock {
    fn acquire_unix(handle: NativeHandle, request: FileLockRequest) -> io::Result<Option<Self>> {
        let start = libc::off_t::try_from(request.range.start)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lock start is too large"))?;
        let len = match request.range.end {
            Some(end) => libc::off_t::try_from(end - request.range.start).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "lock length is too large")
            })?,
            None => 0,
        };
        let lock = libc::flock {
            l_type: match request.mode {
                FileLockMode::Exclusive => libc::F_WRLCK as _,
                FileLockMode::Shared => libc::F_RDLCK as _,
            },
            l_whence: libc::SEEK_SET as _,
            l_start: start,
            l_len: len,
            l_pid: 0,
        };
        let command = ofd_command(request.behavior);
        let result = unsafe { libc::fcntl(handle.as_raw_fd(), command, &lock) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if request.behavior == FileLockBehavior::Try
                && matches!(error.raw_os_error(), Some(code) if code == libc::EACCES || code == libc::EAGAIN)
            {
                return Ok(None);
            }
            return Err(error);
        }
        Ok(Some(Self {
            handle: Some(handle),
            range: request.range,
        }))
    }
}

#[cfg(target_os = "linux")]
fn ofd_command(behavior: FileLockBehavior) -> libc::c_int {
    match behavior {
        FileLockBehavior::Blocking => libc::F_OFD_SETLKW,
        FileLockBehavior::Try => libc::F_OFD_SETLK,
    }
}

#[cfg(target_os = "macos")]
fn ofd_command(behavior: FileLockBehavior) -> libc::c_int {
    const F_OFD_SETLK: libc::c_int = 90;
    const F_OFD_SETLKW: libc::c_int = 91;
    match behavior {
        FileLockBehavior::Blocking => F_OFD_SETLKW,
        FileLockBehavior::Try => F_OFD_SETLK,
    }
}

#[cfg(unix)]
fn unlock(handle: &NativeHandle, range: FileLockRange) -> io::Result<()> {
    if range.is_empty() {
        return Ok(());
    }
    let lock = libc::flock {
        l_type: libc::F_UNLCK as _,
        l_whence: libc::SEEK_SET as _,
        l_start: libc::off_t::try_from(range.start)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lock start is too large"))?,
        l_len: match range.end {
            Some(end) => libc::off_t::try_from(end - range.start).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "lock length is too large")
            })?,
            None => 0,
        },
        l_pid: 0,
    };
    let result = unsafe {
        libc::fcntl(
            handle.as_raw_fd(),
            ofd_command(FileLockBehavior::Try),
            &lock,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
impl ArmedLock {
    fn acquire_windows(handle: NativeHandle, request: FileLockRequest) -> io::Result<Option<Self>> {
        use windows_sys::Win32::{
            Foundation::ERROR_LOCK_VIOLATION,
            Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx},
            System::IO::OVERLAPPED,
        };
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = request.range.start as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (request.range.start >> 32) as u32;
        let len = windows_lock_len(request.range)?;
        let mut flags = match request.mode {
            FileLockMode::Exclusive => LOCKFILE_EXCLUSIVE_LOCK,
            FileLockMode::Shared => 0,
        };
        if request.behavior == FileLockBehavior::Try {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }
        let result = unsafe {
            LockFileEx(
                handle.as_raw_handle(),
                flags,
                0,
                len as u32,
                (len >> 32) as u32,
                &mut overlapped,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if request.behavior == FileLockBehavior::Try
                && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32)
            {
                return Ok(None);
            }
            return Err(error);
        }
        Ok(Some(Self {
            handle: Some(handle),
            range: request.range,
        }))
    }
}

#[cfg(windows)]
fn unlock(handle: &NativeHandle, range: FileLockRange) -> io::Result<()> {
    use windows_sys::Win32::{Storage::FileSystem::UnlockFileEx, System::IO::OVERLAPPED};
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = range.start as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (range.start >> 32) as u32;
    let len = windows_lock_len(range)?;
    let result = unsafe {
        UnlockFileEx(
            handle.as_raw_handle(),
            0,
            len as u32,
            (len >> 32) as u32,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_lock_len(range: FileLockRange) -> io::Result<u64> {
    match range.end {
        Some(end) => end
            .checked_sub(range.start)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid lock range")),
        None => u64::MAX
            .checked_sub(range.start)
            .filter(|&len| len != 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "open-ended lock range starts too late",
                )
            }),
    }
}
