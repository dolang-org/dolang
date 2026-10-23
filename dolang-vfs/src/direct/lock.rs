use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    error::Result,
    file::{FileLockBehavior, FileLockMode, FileLockRange, FileLockRequest},
};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, OwnedHandle};

/// One handle's view of the locks on an open file.
///
/// Duplicates of a handle share the open file, and with it the OS locks, so
/// they share one table: its overlap check spans every duplicate, so no lock
/// can split another. Each handle owns the locks it took and releases only
/// those.
#[derive(Debug)]
pub(crate) struct FileLocks {
    /// Only handles hold this strongly, so its count is the number of live
    /// duplicates. Reservations and locks hold it weakly, and exist only once
    /// the table has been created.
    table: Arc<OnceLock<LockTable>>,
    /// This handle's owner id, assigned on its first lock; `0` until then.
    owner: AtomicU64,
}

impl FileLocks {
    pub(crate) fn new() -> Self {
        Self {
            table: Arc::new(OnceLock::new()),
            owner: AtomicU64::new(0),
        }
    }

    /// Returns the locks of a duplicate handle: the same table, and no locks
    /// of its own yet.
    pub(crate) fn dup(&self) -> Self {
        Self {
            table: self.table.clone(),
            owner: AtomicU64::new(0),
        }
    }

    /// Whether other duplicates of this handle are alive.
    pub(crate) fn is_shared(&self) -> bool {
        Arc::strong_count(&self.table) > 1
    }

    /// Gives up this handle's share of the table, returning whether it was the
    /// last one.
    ///
    /// The caller must have released this handle's locks already.
    pub(crate) fn into_last(self) -> bool {
        let table = self.table.clone();
        drop(self);
        Arc::into_inner(table).is_some()
    }

    fn owner(&self, table: &LockTable) -> u64 {
        let owner = self.owner.load(Ordering::Acquire);
        if owner != 0 {
            return owner;
        }
        // Racing first locks on one handle may each allocate an id; the loser
        // takes the winner's, and its own goes unused.
        let id = table.next_owner.fetch_add(1, Ordering::Relaxed);
        match self
            .owner
            .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => id,
            Err(winner) => winner,
        }
    }

    pub(crate) async fn acquire(
        &self,
        handle: NativeHandle,
        request: FileLockRequest,
    ) -> io::Result<Option<FileLock>> {
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
        let table = self.table.get_or_init(LockTable::default);
        let owner = self.owner(table);
        let reservation = table.reserve(&self.table, request.range, owner)?;
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

    /// Releases every lock this handle still holds, in band.
    ///
    /// Closing a descriptor only drops the underlying locks once *every*
    /// duplicate of the open file description is gone, and duplicates outlive
    /// this file for all sorts of reasons — they are handed to other processes
    /// over the wire, held by in-flight blocking work, and so on — so an
    /// explicit unlock is the only reliable release. Callers must do this
    /// before dropping the file's handles.
    pub(crate) async fn release_all(&self) -> io::Result<()> {
        let owner = self.owner.load(Ordering::Acquire);
        let Some(table) = self.table.get().filter(|_| owner != 0) else {
            return Ok(());
        };
        let armed = table.take_all_armed(owner);
        if armed.is_empty() {
            return Ok(());
        }
        tokio::task::spawn_blocking(move || {
            let mut outcome = Ok(());
            for lock in armed {
                if let Err(error) = lock.release()
                    && outcome.is_ok()
                {
                    outcome = Err(error);
                }
            }
            outcome
        })
        .await
        .map_err(|_| io::Error::other("file unlock worker failed"))?
    }
}

impl Drop for FileLocks {
    fn drop(&mut self) {
        // A duplicate dropped without closing must not leave its locks in
        // force for as long as the others live. Dropping them unlocks.
        let owner = *self.owner.get_mut();
        if owner != 0
            && let Some(table) = self.table.get()
        {
            drop(table.take_all_armed(owner));
        }
    }
}

#[derive(Debug)]
pub(crate) struct FileLock {
    table: Weak<OnceLock<LockTable>>,
    id: u64,
}

impl FileLock {
    pub(crate) async fn release(&mut self) -> Result<()> {
        let Some(table) = self.table.upgrade() else {
            return Ok(());
        };
        let armed = created(&table).take_armed(self.id)?;
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

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            let Ok(Some(armed)) = created(&table).take_armed(self.id) else {
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
                created(&table).remove(self.id);
            }
        }
    }
}

#[derive(Debug)]
struct LockTable {
    state: Mutex<LockTableState>,
    /// Next owner id to hand out; `0` means "none", so this starts at `1`.
    next_owner: AtomicU64,
}

impl Default for LockTable {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            next_owner: AtomicU64::new(1),
        }
    }
}

/// The table behind a reservation or lock, which exists by then.
fn created(table: &OnceLock<LockTable>) -> &LockTable {
    table
        .get()
        .expect("the lock table is created before any lock")
}

#[derive(Debug, Default)]
struct LockTableState {
    next: u64,
    entries: HashMap<u64, LockEntry>,
}

#[derive(Debug)]
struct LockEntry {
    range: FileLockRange,
    /// The handle that took the lock.
    owner: u64,
    armed: Option<ArmedLock>,
}

impl LockTable {
    fn reserve(
        &self,
        shared: &Arc<OnceLock<LockTable>>,
        range: FileLockRange,
        owner: u64,
    ) -> io::Result<Reservation> {
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
        state.entries.insert(
            id,
            LockEntry {
                range,
                owner,
                armed: None,
            },
        );
        Ok(Reservation {
            table: Arc::downgrade(shared),
            id,
            committed: false,
        })
    }

    fn commit(&self, id: u64, armed: ArmedLock) -> io::Result<()> {
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
        Ok(())
    }

    fn remove(&self, id: u64) {
        self.state.lock().unwrap().entries.remove(&id);
    }

    fn take_all_armed(&self, owner: u64) -> Vec<ArmedLock> {
        let mut state = self.state.lock().unwrap();
        let mut armed = Vec::new();
        state.entries.retain(|_, entry| {
            if entry.owner != owner {
                return true;
            }
            match entry.armed.take() {
                Some(lock) => {
                    armed.push(lock);
                    false
                }
                // A reservation that has not committed yet, or a release
                // already in progress elsewhere; neither is ours to take.
                None => true,
            }
        });
        armed
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

fn release(armed: ArmedLock, table: Weak<OnceLock<LockTable>>, id: u64) -> io::Result<()> {
    let outcome = armed.release();
    if let Some(table) = table.upgrade() {
        created(&table).remove(id);
    }
    outcome
}

struct Reservation {
    table: Weak<OnceLock<LockTable>>,
    id: u64,
    committed: bool,
}

impl Reservation {
    fn commit(mut self, armed: ArmedLock) -> io::Result<FileLock> {
        let table = self
            .table
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "file is closed"))?;
        created(&table).commit(self.id, armed)?;
        self.committed = true;
        Ok(FileLock {
            table: self.table.clone(),
            id: self.id,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed
            && let Some(table) = self.table.upgrade()
        {
            created(&table).remove(self.id);
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
}

impl Drop for ArmedLock {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = unlock(&handle, self.range);
        }
    }
}

#[cfg(all(unix, not(target_os = "freebsd")))]
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

#[cfg(target_os = "freebsd")]
impl ArmedLock {
    fn acquire_unix(handle: NativeHandle, request: FileLockRequest) -> io::Result<Option<Self>> {
        if request.range.start != 0 || request.range.end.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "byte-range locks are not supported on FreeBSD",
            ));
        }
        let mut operation = match request.mode {
            FileLockMode::Exclusive => libc::LOCK_EX,
            FileLockMode::Shared => libc::LOCK_SH,
        };
        if request.behavior == FileLockBehavior::Try {
            operation |= libc::LOCK_NB;
        }
        let result = unsafe { libc::flock(handle.as_raw_fd(), operation) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if request.behavior == FileLockBehavior::Try
                && matches!(
                    error.raw_os_error(),
                    Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
                )
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

#[cfg(all(unix, not(target_os = "freebsd")))]
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

#[cfg(target_os = "freebsd")]
fn unlock(handle: &NativeHandle, range: FileLockRange) -> io::Result<()> {
    if range.start != 0 || range.end.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "byte-range locks are not supported on FreeBSD",
        ));
    }
    let result = unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_UN) };
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
