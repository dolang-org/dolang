// --- AI ALERT ---
//
// This file is largely AI-generated with reference to the original sqlite source code, particularly
// the Unix VFS in os_unix.c.  It passes tests and seems reasonably structured, at a high level, but
// could stand further human review.

#![cfg(unix)]

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    ffi::CString,
    fs::File,
    os::unix::{
        fs::{FileExt, MetadataExt},
        io::{AsRawFd, RawFd},
    },
    ptr::NonNull,
    sync::{
        Arc, LazyLock, Mutex, OnceLock,
        atomic::{Ordering, fence},
    },
};

use libc::{c_short, off_t};
use sqlite_plugin::{
    flags::{AccessFlags, CreateMode, LockLevel, OpenMode, OpenOpts, ShmLockMode},
    vars::{
        SQLITE_BUSY, SQLITE_CANTOPEN, SQLITE_IOERR, SQLITE_IOERR_DELETE, SQLITE_IOERR_DELETE_NOENT,
        SQLITE_IOERR_FSTAT, SQLITE_IOERR_FSYNC, SQLITE_IOERR_LOCK, SQLITE_IOERR_READ,
        SQLITE_IOERR_TRUNCATE, SQLITE_IOERR_UNLOCK, SQLITE_IOERR_WRITE,
    },
    vfs::{RegisterOpts, Vfs, VfsHandle, VfsResult, register_static},
};

use dolang_shell_vfs::{Client, FileHandle as _, Utf8TypedPath, Utf8UnixPath, Vfs as _};

// Shadow libc's F_RDLCK/F_WRLCK/F_UNLCK with i32 versions.
// On Linux these constants are already i32; on macOS they are i16.
// Defining them here as i32 avoids platform-specific casts at every call site.
#[allow(clippy::unnecessary_cast)]
const F_RDLCK: c_short = libc::F_RDLCK as _;
#[allow(clippy::unnecessary_cast)]
const F_WRLCK: c_short = libc::F_WRLCK as _;
#[allow(clippy::unnecessary_cast)]
const F_UNLCK: c_short = libc::F_UNLCK as _;

/// Generate a unique temporary file path of the form `/tmp/etilqs_<hex>`.
/// Uses `sqlite3_randomness` (SQLite's own PRNG) to fill 16 bytes, matching the
/// approach taken by SQLite's `unixGetTempname` and avoiding name-prediction attacks.
fn gen_temp_path() -> String {
    let mut bytes = [0u8; 16];
    unsafe {
        libsqlite3_sys::sqlite3_randomness(
            bytes.len() as std::ffi::c_int,
            bytes.as_mut_ptr().cast(),
        );
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("/tmp/etilqs_{hex}")
}

/// Maps a shell VFS error to a SQLite error code, following the same logic as
/// `sqliteErrorFromPosixError` in os_unix.c.
///
/// - `NotFound`        → `not_found_code` (caller supplies context-appropriate code)
/// - `PermissionDenied`, `AlreadyExists` → `SQLITE_CANTOPEN`
/// - Anything else    → `default_code`
fn map_vfs_err(err: dolang_shell_vfs::Error, not_found_code: i32, default_code: i32) -> i32 {
    match err.kind() {
        std::io::ErrorKind::NotFound => not_found_code,
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AlreadyExists => SQLITE_CANTOPEN,
        _ => default_code,
    }
}

// SQLite lock byte ranges (from os_unix.c lines 7465-7467)
const PENDING_BYTE: off_t = 0x40000000;
const RESERVED_BYTE: off_t = 0x40000001;
const SHARED_FIRST: off_t = 0x40000002;
const SHARED_SIZE: off_t = 510;

// WAL shared-memory lock constants (from os_unix.c).
// UNIX_SHM_BASE = (22 + SQLITE_SHM_NLOCK) * 4 = 120: first per-slot lock byte.
// UNIX_SHM_DMS  = UNIX_SHM_BASE + SQLITE_SHM_NLOCK = 128: deadman-switch byte.
const SQLITE_SHM_NLOCK: usize = 8;
const UNIX_SHM_BASE: off_t = (22 + SQLITE_SHM_NLOCK as off_t) * 4; // 120
const UNIX_SHM_DMS: off_t = UNIX_SHM_BASE + SQLITE_SHM_NLOCK as off_t; // 128

// Shm-specific SQLite extended error codes.  These are formed as
// `primary_code | (discriminant << 8)` per SQLite's error code convention.
const SQLITE_IOERR_SHMOPEN: i32 = SQLITE_IOERR | (18 << 8); // 4618
const SQLITE_IOERR_SHMSIZE: i32 = SQLITE_IOERR | (22 << 8); // 5642
const SQLITE_IOERR_SHMLOCK: i32 = SQLITE_IOERR | (20 << 8); // 5130
const SQLITE_IOERR_SHMMAP: i32 = SQLITE_IOERR | (21 << 8); // 5386
// SQLITE_READONLY = 8, not exported by sqlite_plugin::vars
const SQLITE_READONLY_CANTINIT: i32 = 8 | (5 << 8); // 1288

// Type alias for SQLite error codes
type SqliteErr = i32;

thread_local! {
    /// Thread-local shell VFS client for VFS callbacks.
    static SHELL_CLIENT: RefCell<Option<Client>> = const { RefCell::new(None) };
}

/// Set the shell VFS client in thread-local storage for the duration of the closure.
pub(crate) fn with_shell<F, R>(client: Client, f: F) -> R
where
    F: FnOnce() -> R,
{
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            SHELL_CLIENT.with(|c| *c.borrow_mut() = None);
        }
    }

    SHELL_CLIENT.with(|c| *c.borrow_mut() = Some(client));
    let _guard = ClearOnDrop;
    f()
}

fn get_shell_client() -> Client {
    SHELL_CLIENT
        .with(|c: &RefCell<Option<Client>>| c.borrow().clone())
        .expect("VFS entered without client set")
}

/// Execute an async shell VFS operation from a synchronous VFS context.
fn block_on_shell<T>(f: impl Future<Output = VfsResult<T>> + Send + 'static) -> VfsResult<T>
where
    T: Send + 'static,
{
    tokio::runtime::Handle::current().block_on(f)
}

/// Lookup key for the global inode table: uniquely identifies a file by device+inode.
#[derive(Hash, Eq, PartialEq, Clone, Copy)]
struct InodeId {
    dev: u64,
    ino: u64,
}

/// Per-inode shared state across all handles in this process.
/// Mutex ordering: always acquire INODE_TABLE lock before inode lock, never in reverse.
struct InodeState {
    /// Number of handles currently holding a SHARED lock (may piggyback on one fcntl lock).
    n_shared: usize,
    /// Number of handles holding any lock > UNLOCKED (gate for deferred fd close).
    n_lock: usize,
    /// Highest lock level held by any handle in this process on this inode.
    max_lock: LockLevel,
    /// File descriptors deferred for close until n_lock drops to zero.
    /// Closing any fd to an inode releases ALL process-wide fcntl locks on it (POSIX),
    /// so we keep fds open until no locks remain.
    pending_files: Vec<File>,
    /// Number of `ShellFileHandle` objects referencing this `InodeState`.
    n_ref: usize,
    /// WAL shared-memory node; `None` until first `shm_map`.
    /// Mutex ordering: InodeState → ShmNode (never reversed).
    shm_node: Option<Arc<Mutex<ShmNode>>>,
}

/// Global inode table. Mutex ordering: INODE_TABLE lock → per-inode Mutex (never reversed).
static INODE_TABLE: LazyLock<Mutex<HashMap<InodeId, Arc<Mutex<InodeState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Container-aware SQLite VFS that routes file operations through the `dolang-vfs` helper.
/// This VFS should only be used when a shell VFS client is available.
pub struct ShellVfs;

// ── WAL shared-memory types ────────────────────────────────────────────────────

/// Per-inode WAL shared-memory state.  One instance is shared by all
/// `ShellFileHandle` objects that open the same `<db>-shm` file within this
/// process.  Mirrors `unixShmNode` in os_unix.c.
///
/// Mutex ordering: InodeState → ShmNode (never reversed).
struct ShmNode {
    /// The `-shm` file (fd received via SCM_RIGHTS from the shell VFS helper).
    /// `None` after the node has been purged on last-connection close.
    shm_file: Option<File>,
    /// Path of the `-shm` file; kept for optional deletion in `shm_unmap`.
    shm_path: String,
    /// Size of each mapped region in bytes; fixed on the first `shm_map` call.
    sz_region: usize,
    /// Number of logical regions to cover per `mmap(2)` call.
    /// `= max(1, page_size / sz_region)` so each mmap is page-size aligned.
    regions_per_map: usize,
    /// One pointer per logical region; index matches the `region_idx` argument to `shm_map`.
    region_ptrs: Vec<*mut u8>,
    /// One `(base_ptr, total_len)` per `mmap` call, for correct `munmap` on drop.
    mmap_groups: Vec<(*mut u8, usize)>,
    /// Per-slot intra-process lock state.
    /// 0 = unlocked, +N = N shared holders (piggybacked), -1 = exclusive.
    a_lock: [i32; SQLITE_SHM_NLOCK],
    /// Number of live `ShmConn` references from `ShellFileHandle`.
    n_ref: usize,
    /// True if the shm file was opened read-only.
    is_readonly: bool,
    /// True if the DMS lock has not yet been acquired; retried on each `shm_map`.
    is_unlocked: bool,
}

// SAFETY: `region_ptrs` and `mmap_groups` hold raw pointers into mmap'd memory.
// The mapping is valid for the lifetime of `ShmNode`, accesses are serialised by
// the `Mutex`, and we never move the node after placing it behind `Arc<Mutex<>>`.
unsafe impl Send for ShmNode {}

impl Drop for ShmNode {
    fn drop(&mut self) {
        // munmap every group first so no dangling pointers remain, then close fd.
        for (ptr, len) in self.mmap_groups.drain(..) {
            unsafe { libc::munmap(ptr as *mut libc::c_void, len) };
        }
        self.region_ptrs.clear();
        // self.shm_file drops here, closing the fd and releasing all fcntl locks.
    }
}

/// Per-connection WAL shared-memory state; one per `ShellFileHandle`.
/// Mirrors the per-connection fields of `unixShm` in os_unix.c.
struct ShmConn {
    /// Bitmask of shm lock slots held as SHARED by this connection.
    shared_mask: u16,
    /// Bitmask of shm lock slots held as EXCLUSIVE by this connection.
    excl_mask: u16,
    /// Shared ownership of the per-inode node; also keeps the mmap alive while
    node: Arc<Mutex<ShmNode>>,
}

// ── ShellFileHandle ────────────────────────────────────────────────────────────

/// File handle for the shell VFS.
pub struct ShellFileHandle {
    file: File,
    inode_id: InodeId,
    inode: Arc<Mutex<InodeState>>,
    lock_level: LockLevel,
    /// Whether the file was opened read-only; returned as `SQLITE_OPEN_READONLY` out flag.
    readonly: bool,
    /// Path to delete when the handle is closed, or `None` if no deletion is needed.
    /// Set for temp files (path=None opens) and for named files with DELETEONCLOSE.
    delete_on_close: Option<String>,
    /// The database file path, needed to derive `<db>-shm` for WAL mode.
    db_path: Option<String>,
    /// Per-connection WAL shared-memory state; `None` until the first `shm_map`.
    shm_conn: Option<ShmConn>,
}

impl VfsHandle for ShellFileHandle {
    fn readonly(&self) -> bool {
        self.readonly
    }

    fn in_memory(&self) -> bool {
        false
    }
}

// ── fcntl helpers ──────────────────────────────────────────────────────────────

/// Calls `F_SETLK` with the given lock type and byte range.
/// Returns `SQLITE_BUSY` if errno is `EAGAIN` or `EACCES` (lock held by another process),
/// or `SQLITE_IOERR_LOCK` on other errors.
fn fcntl_set_lock(fd: RawFd, lock_type: c_short, start: off_t, len: off_t) -> VfsResult<()> {
    let flock = libc::flock {
        l_type: lock_type,
        l_whence: libc::SEEK_SET as c_short,
        l_start: start,
        l_len: len,
        l_pid: 0,
    };
    let ret = unsafe { libc::fcntl(fd, libc::F_SETLK, &flock) };
    if ret == 0 {
        Ok(())
    } else {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::EAGAIN || errno == libc::EACCES {
            Err(SQLITE_BUSY)
        } else {
            Err(SQLITE_IOERR_LOCK)
        }
    }
}

/// Calls `F_SETLK` with `F_UNLCK` for the given byte range.
/// `len = 0` means unlock from `start` to end of file (effectively all bytes when `start = 0`).
/// Returns `SQLITE_IOERR_UNLOCK` on error.
fn fcntl_unlock(fd: RawFd, start: off_t, len: off_t) -> VfsResult<()> {
    let flock = libc::flock {
        l_type: F_UNLCK,
        l_whence: libc::SEEK_SET as c_short,
        l_start: start,
        l_len: len,
        l_pid: 0,
    };
    let ret = unsafe { libc::fcntl(fd, libc::F_SETLK, &flock) };
    if ret == 0 {
        Ok(())
    } else {
        Err(SQLITE_IOERR_UNLOCK)
    }
}

/// Core unlock logic shared by `unlock()` and `close()`.
/// Replicates `posixUnlock` in os_unix.c (lines 2145-2296).
///
/// Caller must hold the inode mutex (passing `&mut InodeState`).
fn unlock_inner(
    fd: RawFd,
    lock_level: &mut LockLevel,
    inode_state: &mut InodeState,
    desired: LockLevel,
) -> VfsResult<()> {
    if *lock_level <= desired {
        return Ok(());
    }

    // Downgrade from above-SHARED (RESERVED/PENDING/EXCLUSIVE) toward SHARED or UNLOCKED.
    if *lock_level > LockLevel::Shared {
        if desired == LockLevel::Shared {
            // Downgrade the exclusive write lock on SHARED range to a read lock.
            fcntl_set_lock(fd, F_RDLCK, SHARED_FIRST, SHARED_SIZE)?;
        }
        // Release PENDING_BYTE and RESERVED_BYTE (2 adjacent bytes: PENDING_BYTE, PENDING_BYTE+1).
        fcntl_unlock(fd, PENDING_BYTE, 2)?;
        inode_state.max_lock = LockLevel::Shared;
    }

    // Release the SHARED lock if the target is UNLOCKED.
    if desired == LockLevel::Unlocked {
        inode_state.n_shared -= 1;
        if inode_state.n_shared == 0 {
            // We are the last SHARED holder — release the fcntl lock on the entire file.
            fcntl_unlock(fd, 0, 0)?;
            inode_state.max_lock = LockLevel::Unlocked;
        }
        inode_state.n_lock -= 1;
        if inode_state.n_lock == 0 {
            // No handles hold any lock: safe to close all deferred fds now.
            inode_state.pending_files.clear();
        }
    }

    *lock_level = desired;
    Ok(())
}

/// `F_SETLK` wrapper for shm slot locks.  Unlike `fcntl_set_lock`, all failures
/// map to `SQLITE_BUSY` — matching `unixShmSystemLock` in os_unix.c which returns
/// `SQLITE_BUSY` for any `fcntl` error on the shm file.
fn shm_fcntl_lock(fd: RawFd, lock_type: c_short, start: off_t, len: off_t) -> VfsResult<()> {
    let flock = libc::flock {
        l_type: lock_type,
        l_whence: libc::SEEK_SET as c_short,
        l_start: start,
        l_len: len,
        l_pid: 0,
    };
    let ret = unsafe { libc::fcntl(fd, libc::F_SETLK, &flock) };
    if ret == 0 { Ok(()) } else { Err(SQLITE_BUSY) }
}

// ── dead_code helpers (usable once sqlite-plugin passes the file handle) ────────

/// Compute SQLite device characteristics flags for the given file descriptor.
///
/// Replicates `setDeviceCharacteristics` in os_unix.c.
///
/// On Linux the function probes for F2FS atomic-write support via ioctl and always
/// sets `SQLITE_IOCAP_POWERSAFE_OVERWRITE` (SQLite's compile-time default of 1) and
/// `SQLITE_IOCAP_SUBPAGE_READ` (set unconditionally in modern SQLite Linux builds).
#[cfg(target_os = "linux")]
unsafe fn device_characteristics_for_fd(fd: RawFd) -> i32 {
    // F2FS_IOC_GET_FEATURES = _IOR(0xf5, 12, u32) — standard Linux IOC encoding:
    //   (IOC_READ=2 << 30) | (type=0xf5 << 8) | nr=12 | (sizeof(u32)=4 << 16)
    // This encoding holds on all common Linux architectures (x86, arm, aarch64, riscv, s390x).
    const F2FS_IOC_GET_FEATURES: libc::c_ulong = (2 << 30) | (0xf5 << 8) | 12 | (4 << 16);
    const F2FS_FEATURE_ATOMIC_WRITE: u32 = 0x0004;

    let mut flags: i32 = 0;

    // Check for F2FS atomic batch-write support (mirrors os_unix.c lines 4356-4364).
    let mut f: u32 = 0;
    if unsafe { libc::ioctl(fd, F2FS_IOC_GET_FEATURES, &mut f) } == 0
        && (f & F2FS_FEATURE_ATOMIC_WRITE) != 0
    {
        flags |= libsqlite3_sys::SQLITE_IOCAP_BATCH_ATOMIC;
    }

    // SQLITE_POWERSAFE_OVERWRITE defaults to 1 in all standard SQLite builds.
    flags |= libsqlite3_sys::SQLITE_IOCAP_POWERSAFE_OVERWRITE;

    // Set unconditionally in modern SQLite Linux builds (os_unix.c line 4371).
    flags |= libsqlite3_sys::SQLITE_IOCAP_SUBPAGE_READ;

    flags
}

/// Non-Linux Unix: return the same POWERSAFE_OVERWRITE + SUBPAGE_READ baseline.
/// There is no ioctl to probe for F2FS or other advanced capabilities here.
#[cfg(not(target_os = "linux"))]
unsafe fn device_characteristics_for_fd(_fd: RawFd) -> i32 {
    libsqlite3_sys::SQLITE_IOCAP_POWERSAFE_OVERWRITE | libsqlite3_sys::SQLITE_IOCAP_SUBPAGE_READ
}

/// Return the filesystem sector size for the given file descriptor.
///
/// Uses `fstat(2)` to read `st_blksize`, which the kernel sets to the filesystem's
/// preferred I/O transfer unit (typically 4096 for local filesystems, but may differ
/// for network or special filesystems running inside the container).
unsafe fn sector_size_for_fd(fd: RawFd) -> i32 {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } == 0 {
        // st_blksize is always positive; clamp to i32 for SQLite's interface.
        stat.st_blksize.clamp(512, i32::MAX as _) as i32
    } else {
        // SQLITE_DEFAULT_SECTOR_SIZE fallback.
        4096
    }
}

// ── WAL shm helpers ────────────────────────────────────────────────────────────

/// Acquire the DMS (deadman-switch) lock on the shm file.
/// Replicates `unixLockSharedMemory` in os_unix.c (lines 4841-4915).
///
/// Protocol (mirrors the C implementation):
///  - `F_GETLK` on `UNIX_SHM_DMS` to probe current state.
///  - **Unlocked + read/write**: take exclusive write lock, truncate to 3 bytes
///    (marks the shm as freshly initialised), downgrade to shared read lock.
///  - **Unlocked + readonly**: set `*is_unlocked = true`, return
///    `SQLITE_READONLY_CANTINIT`; caller may proceed but will retry on the next
///    `shm_map` call.
///  - **Read-locked** (another process holds DMS shared): skip to shared lock.
///  - **Write-locked** (another process is initialising): return `SQLITE_BUSY`.
fn lock_shm_dms(shm_file: &File, is_readonly: bool) -> VfsResult<()> {
    let fd = shm_file.as_raw_fd();

    let mut flock = libc::flock {
        l_type: F_WRLCK,
        l_whence: libc::SEEK_SET as c_short,
        l_start: UNIX_SHM_DMS,
        l_len: 1,
        l_pid: 0,
    };
    if unsafe { libc::fcntl(fd, libc::F_GETLK, &mut flock) } != 0 {
        return Err(SQLITE_IOERR_LOCK);
    }

    match flock.l_type {
        F_WRLCK => {
            // Another process is mid-initialisation.
            return Err(SQLITE_BUSY);
        }
        F_UNLCK => {
            // We are the first.
            if is_readonly {
                return Err(SQLITE_READONLY_CANTINIT);
            }
            // Take exclusive write lock on DMS.
            shm_fcntl_lock(fd, F_WRLCK, UNIX_SHM_DMS, 1)?;
            // Truncate to 3 bytes to signal a fresh initialisation.
            shm_file.set_len(3).map_err(|_| SQLITE_IOERR_SHMOPEN)?;
            // Fall through to take the shared read lock below.
        }
        _ => {
            // F_RDLCK: another process already holds the DMS shared lock.
            // Jump directly to taking our own shared lock.
        }
    }

    // Take a shared read lock on the DMS byte.
    shm_fcntl_lock(fd, F_RDLCK, UNIX_SHM_DMS, 1)
}

/// Open (or join) the per-inode `ShmNode` and attach a `ShmConn` to `handle`.
/// Replicates the `unixOpenSharedMemory` call path in os_unix.c (lines 4952-5085).
///
/// Idempotent: returns immediately if the handle already has a `ShmConn`.
fn open_or_join_shm(handle: &mut ShellFileHandle) -> VfsResult<()> {
    if handle.shm_conn.is_some() {
        return Ok(());
    }

    let db_path = handle.db_path.as_deref().ok_or(SQLITE_IOERR)?;
    let shm_path = format!("{db_path}-shm");

    // Acquire InodeState lock and get-or-create the ShmNode.
    // Mutex ordering: InodeState first, then ShmNode — maintained throughout.
    let node: Arc<Mutex<ShmNode>> = {
        let mut inode_state = handle.inode.lock().unwrap();

        if let Some(existing) = &inode_state.shm_node {
            // Another connection in this process already opened the shm file.
            let node = existing.clone();
            node.lock().unwrap().n_ref += 1;
            node
        } else {
            // First connection: open the shm file via the shell VFS helper.
            // The helper sends the fd back via SCM_RIGHTS; mmap works on it normally.
            let shm_path_clone = shm_path.clone();
            let client = get_shell_client();
            let (shm_file, is_readonly) = block_on_shell(async move {
                // Try read+write with O_CREAT first.
                match client
                    .open_options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&shm_path_clone)
                    .await
                {
                    Ok(f) => Ok((f.try_into_std().await.map_err(|_| SQLITE_CANTOPEN)?, false)),
                    Err(_) => {
                        // Fall back to read-only (e.g. permissions or read-only mount).
                        let f = client
                            .open_options()
                            .read(true)
                            .open(&shm_path_clone)
                            .await
                            .map_err(|e| map_vfs_err(e, SQLITE_CANTOPEN, SQLITE_IOERR_SHMOPEN))?;
                        Ok::<_, i32>((f.try_into_std().await.map_err(|_| SQLITE_CANTOPEN)?, true))
                    }
                }
            })?;

            let mut inner = ShmNode {
                shm_file: Some(shm_file),
                shm_path,
                sz_region: 0,
                regions_per_map: 1,
                region_ptrs: Vec::new(),
                mmap_groups: Vec::new(),
                a_lock: [0; SQLITE_SHM_NLOCK],
                n_ref: 1,
                is_readonly,
                is_unlocked: false,
            };

            // Acquire DMS lock.  SQLITE_READONLY_CANTINIT is not fatal here:
            // it sets is_unlocked=true and the lock will be retried on the first
            // shm_map call (same behaviour as unixOpenSharedMemory).
            let rc = lock_shm_dms(inner.shm_file.as_ref().unwrap(), inner.is_readonly);
            if let Err(e) = rc {
                if e == SQLITE_READONLY_CANTINIT {
                    inner.is_unlocked = true;
                } else {
                    return Err(e);
                }
            }

            let node = Arc::new(Mutex::new(inner));
            inode_state.shm_node = Some(node.clone());
            node
        }
    };

    handle.shm_conn = Some(ShmConn {
        shared_mask: 0,
        excl_mask: 0,
        node,
    });
    Ok(())
}

/// Disconnect `handle` from its `ShmNode`, releasing any per-slot fcntl locks
/// still held by this connection.  When the last connection disconnects, the
/// `ShmNode` is removed from the inode table; if nothing else holds a reference
/// `Drop<ShmNode>` runs immediately (munmap + close).
///
/// Safe to call when `shm_conn` is `None`.
fn disconnect_shm(handle: &mut ShellFileHandle) {
    let Some(conn) = handle.shm_conn.take() else {
        return;
    };

    // Release per-slot fcntl locks under the ShmNode mutex.
    // Errors are ignored (best-effort) because we are cleaning up.
    let n_ref_after = {
        let mut node = conn.node.lock().unwrap();
        if let Some(shm_file) = &node.shm_file {
            let fd = shm_file.as_raw_fd();
            for i in 0..SQLITE_SHM_NLOCK {
                let bit = 1u16 << i;
                if conn.shared_mask & bit != 0 {
                    node.a_lock[i] -= 1;
                    if node.a_lock[i] == 0 {
                        let _ = fcntl_unlock(fd, UNIX_SHM_BASE + i as off_t, 1);
                    }
                }
                if conn.excl_mask & bit != 0 {
                    node.a_lock[i] = 0;
                    let _ = fcntl_unlock(fd, UNIX_SHM_BASE + i as off_t, 1);
                }
            }
        }
        node.n_ref -= 1;
        node.n_ref
    };
    // ShmNode mutex released here — before we take InodeState below.
    // (Mutex ordering: InodeState → ShmNode; we must never hold ShmNode while
    // acquiring InodeState.)

    if n_ref_after == 0 {
        // Last connection: remove from inode table so future opens start fresh.
        let mut inode_state = handle.inode.lock().unwrap();
        inode_state.shm_node = None;
        // inode_state drops here, then conn (our Arc clone) drops at scope end.
    }
    // conn drops here, releasing its Arc reference.
}

// ── impl Vfs ───────────────────────────────────────────────────────────────────

impl Vfs for ShellVfs {
    type Handle = ShellFileHandle;

    fn open(&self, path: Option<&str>, opts: OpenOpts) -> VfsResult<Self::Handle> {
        let client = get_shell_client();
        let db_path = path.map(|p| p.to_string());

        let readonly = matches!(opts.mode(), OpenMode::ReadOnly);

        let (std_file, delete_on_close) = if let Some(named_path) = path {
            // Named file open: honor DELETEONCLOSE and the requested open mode.
            let path = named_path.to_string();
            let delete_on_close = opts.delete_on_close().then(|| path.clone());
            let file = block_on_shell(async move {
                let mut open_opts = client.open_options();
                match opts.mode() {
                    OpenMode::ReadOnly => {
                        open_opts.read(true);
                    }
                    OpenMode::ReadWrite { create } => {
                        open_opts.read(true).write(true);
                        match create {
                            CreateMode::None => {}
                            CreateMode::Create => {
                                open_opts.create(true);
                            }
                            CreateMode::MustCreate => {
                                open_opts.create(true).create_new(true);
                            }
                        }
                    }
                }
                let tokio_file = open_opts
                    .open(&path)
                    .await
                    .map_err(|e| map_vfs_err(e, SQLITE_CANTOPEN, SQLITE_IOERR))?;
                tokio_file.try_into_std().await.map_err(|_| SQLITE_CANTOPEN)
            })?;
            (file, delete_on_close)
        } else {
            // Temporary file: SQLite passes no path and always expects the file to be deleted
            // when the handle is closed.  Generate a unique name and open it exclusively.
            let temp_path = gen_temp_path();
            let tp = temp_path.clone();
            let file = block_on_shell(async move {
                let tokio_file = client
                    .open_options()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&tp)
                    .await
                    .map_err(|e| map_vfs_err(e, SQLITE_CANTOPEN, SQLITE_IOERR))?;
                tokio_file.try_into_std().await.map_err(|_| SQLITE_CANTOPEN)
            })?;
            (file, Some(temp_path))
        };

        // Identify the file by device+inode for the global inode table.
        let meta = std_file.metadata().map_err(|_| SQLITE_IOERR_FSTAT)?;
        let inode_id = InodeId {
            dev: meta.dev(),
            ino: meta.ino(),
        };

        // Look up or create the InodeState for this inode, then increment n_ref.
        let inode = {
            let mut table = INODE_TABLE.lock().unwrap();
            let arc = table.entry(inode_id).or_insert_with(|| {
                Arc::new(Mutex::new(InodeState {
                    n_shared: 0,
                    n_lock: 0,
                    max_lock: LockLevel::Unlocked,
                    pending_files: Vec::new(),
                    n_ref: 0,
                    shm_node: None,
                }))
            });
            arc.lock().unwrap().n_ref += 1;
            arc.clone()
        };

        Ok(ShellFileHandle {
            file: std_file,
            inode_id,
            inode,
            lock_level: LockLevel::Unlocked,
            readonly,
            delete_on_close,
            db_path,
            shm_conn: None,
        })
    }

    fn delete(&self, path: &str) -> VfsResult<()> {
        let path = path.to_string();
        let client = get_shell_client();
        block_on_shell(async move {
            client
                .remove(Utf8TypedPath::Unix(Utf8UnixPath::new(&path)), false, false)
                .await
                .map_err(|e| map_vfs_err(e, SQLITE_IOERR_DELETE_NOENT, SQLITE_IOERR_DELETE))
        })
    }

    fn access(&self, path: &str, flags: AccessFlags) -> VfsResult<bool> {
        let path = path.to_string();
        let client = get_shell_client();

        // Map sqlite_plugin AccessFlags to nix::unistd::AccessFlags
        let mode = match flags {
            AccessFlags::Exists => dolang_shell_vfs::AccessFlags::F_OK,
            AccessFlags::Read => dolang_shell_vfs::AccessFlags::R_OK,
            AccessFlags::ReadWrite => {
                dolang_shell_vfs::AccessFlags::R_OK | dolang_shell_vfs::AccessFlags::W_OK
            }
        };

        block_on_shell(async move {
            match client.access(&path, mode).await {
                Ok(_) => Ok(true),
                Err(_) => Ok(false),
            }
        })
    }

    fn file_size(&self, handle: &mut Self::Handle) -> VfsResult<usize> {
        handle
            .file
            .metadata()
            .map(|m| m.len() as usize)
            .map_err(|_| SQLITE_IOERR_FSTAT)
    }

    fn truncate(&self, handle: &mut Self::Handle, size: usize) -> VfsResult<()> {
        handle
            .file
            .set_len(size as u64)
            .map_err(|_| SQLITE_IOERR_TRUNCATE)
    }

    fn write(&self, handle: &mut Self::Handle, offset: usize, data: &[u8]) -> VfsResult<usize> {
        handle
            .file
            .write_at(data, offset as u64)
            .map_err(|_| SQLITE_IOERR_WRITE)
    }

    fn read(&self, handle: &mut Self::Handle, offset: usize, buf: &mut [u8]) -> VfsResult<usize> {
        handle
            .file
            .read_at(buf, offset as u64)
            .map_err(|_| SQLITE_IOERR_READ)
    }

    /// Replicates `unixLock` in os_unix.c.
    fn lock(&self, handle: &mut Self::Handle, desired: LockLevel) -> VfsResult<()> {
        // Idempotent: already at this level or higher.
        if handle.lock_level >= desired {
            return Ok(());
        }

        let fd = handle.file.as_raw_fd();
        let mut inode_state = handle.inode.lock().unwrap();

        // Intra-process conflict check (mirrors os_unix.c lines 1967-1972):
        // If this handle's level differs from the inode's current max AND someone holds
        // PENDING or higher (or we want more than SHARED), another handle has a conflicting lock.
        if handle.lock_level != inode_state.max_lock
            && (inode_state.max_lock >= LockLevel::Pending || desired > LockLevel::Shared)
        {
            return Err(SQLITE_BUSY);
        }

        // SHARED piggyback (mirrors os_unix.c lines 1978-1987):
        // Another handle in this process already holds SHARED or RESERVED via fcntl —
        // we can share the existing read lock without another syscall.
        if desired == LockLevel::Shared
            && (inode_state.max_lock == LockLevel::Shared
                || inode_state.max_lock == LockLevel::Reserved)
        {
            inode_state.n_shared += 1;
            inode_state.n_lock += 1;
            handle.lock_level = LockLevel::Shared;
            return Ok(());
        }

        // Acquire a lock on PENDING_BYTE (mirrors os_unix.c lines 1996-2011):
        //   - Read lock when acquiring SHARED: prevents new exclusive-PENDING holders from
        //     blocking us while we grab the SHARED range.
        //   - Write lock when escalating RESERVED → EXCLUSIVE: blocks new SHARED acquisitions
        //     so existing holders can drain.
        if desired == LockLevel::Shared
            || (desired == LockLevel::Exclusive && handle.lock_level == LockLevel::Reserved)
        {
            let lock_type = if desired == LockLevel::Shared {
                F_RDLCK
            } else {
                F_WRLCK
            };
            fcntl_set_lock(fd, lock_type, PENDING_BYTE, 1)?;
            if desired == LockLevel::Exclusive {
                // Record the PENDING state so a retry sees we already hold PENDING_BYTE.
                handle.lock_level = LockLevel::Pending;
                inode_state.max_lock = LockLevel::Pending;
            }
        }

        if desired == LockLevel::Shared {
            // Acquire the read lock on the SHARED range (mirrors os_unix.c lines 2018-2050).
            // Always release the temporary PENDING_BYTE lock, even if the SHARED lock fails.
            let lock_res = fcntl_set_lock(fd, F_RDLCK, SHARED_FIRST, SHARED_SIZE);
            let unlock_res = fcntl_unlock(fd, PENDING_BYTE, 1);
            lock_res?; // return SHARED-lock error first (matching SQLite's priority)
            unlock_res?;
            inode_state.n_shared = 1;
            inode_state.n_lock += 1;
            inode_state.max_lock = LockLevel::Shared;
        } else if desired == LockLevel::Exclusive && inode_state.n_shared > 1 {
            // Other in-process handles still hold SHARED; cannot upgrade to EXCLUSIVE yet.
            // We retain PENDING_BYTE write lock to block new SHARED acquisitions.
            // The caller (SQLite) will retry lock(EXCLUSIVE) after the busy-handler.
            return Err(SQLITE_BUSY);
        } else {
            // RESERVED, or EXCLUSIVE with all in-process SHARED locks cleared.
            match desired {
                LockLevel::Reserved => {
                    fcntl_set_lock(fd, F_WRLCK, RESERVED_BYTE, 1)?;
                }
                LockLevel::Exclusive => {
                    fcntl_set_lock(fd, F_WRLCK, SHARED_FIRST, SHARED_SIZE)?;
                }
                // PENDING is an internal intermediate state; SQLite never requests it directly.
                _ => {}
            }
        }

        handle.lock_level = desired;
        inode_state.max_lock = desired;
        Ok(())
    }

    /// Replicates `posixUnlock` in os_unix.c (lines 2145-2296).
    fn unlock(&self, handle: &mut Self::Handle, desired: LockLevel) -> VfsResult<()> {
        if handle.lock_level <= desired {
            return Ok(());
        }
        let fd = handle.file.as_raw_fd();
        let mut inode_state = handle.inode.lock().unwrap();
        unlock_inner(fd, &mut handle.lock_level, &mut inode_state, desired)
    }

    /// Check whether any connection (in-process or inter-process) holds a RESERVED or higher lock.
    /// Replicates `unixCheckReservedLock` in os_unix.c (lines ~1820-1860).
    fn check_reserved_lock(&self, handle: &mut ShellFileHandle) -> VfsResult<bool> {
        let fd = handle.file.as_raw_fd();
        let inode = handle.inode.lock().unwrap();

        // Intra-process check: another handle in this process holds RESERVED or higher.
        if inode.max_lock >= LockLevel::Reserved {
            return Ok(true);
        }

        // Inter-process check via F_GETLK on RESERVED_BYTE.
        let mut flock = libc::flock {
            l_type: F_WRLCK,
            l_whence: libc::SEEK_SET as c_short,
            l_start: RESERVED_BYTE,
            l_len: 1,
            l_pid: 0,
        };
        let ret = unsafe { libc::fcntl(fd, libc::F_GETLK, &mut flock) };
        if ret != 0 {
            return Err(SQLITE_IOERR);
        }
        Ok(flock.l_type != F_UNLCK)
    }

    fn sync(&self, handle: &mut Self::Handle) -> VfsResult<()> {
        let fd = handle.file.as_raw_fd();
        // fdatasync is Linux-specific; fall back to fsync on other platforms.
        #[cfg(target_os = "linux")]
        let ret = unsafe { libc::fdatasync(fd) };
        #[cfg(not(target_os = "linux"))]
        let ret = unsafe { libc::fsync(fd) };
        if ret == 0 {
            Ok(())
        } else {
            Err(SQLITE_IOERR_FSYNC)
        }
    }

    /// Replicates `unixClose` in os_unix.c (lines 2362-2392).
    fn close(&self, mut handle: Self::Handle) -> VfsResult<()> {
        // Defensively disconnect shm if xShmUnmap was not called (e.g. on error paths).
        disconnect_shm(&mut handle);

        let ShellFileHandle {
            file,
            inode_id,
            inode,
            mut lock_level,
            delete_on_close,
            ..
        } = handle;
        let fd = file.as_raw_fd();

        // 1. Unlock to UNLOCKED.  Save the error rather than returning early so that
        //    inode table cleanup always runs — matching SQLite's own unixClose, which
        //    also ignores unlock errors to avoid leaking the inode entry.
        let unlock_err = {
            let mut inode_state = inode.lock().unwrap();
            unlock_inner(fd, &mut lock_level, &mut inode_state, LockLevel::Unlocked).err()
        };

        // 2. Acquire INODE_TABLE then inode lock (maintain ordering: table → inode).
        let mut table = INODE_TABLE.lock().unwrap();
        let mut inode_state = inode.lock().unwrap();

        // 3. Defer the fd close if other handles still hold locks.
        //    Closing any fd to an inode releases ALL process-wide POSIX fcntl locks on it,
        //    which would silently destroy locks held by other handles on the same inode.
        if inode_state.n_lock > 0 {
            inode_state.pending_files.push(file);
        }
        // else: `file` drops at the end of this function, closing the fd safely.

        // 4. Decrement the reference count and remove the inode entry when it reaches zero.
        inode_state.n_ref -= 1;
        let should_remove = inode_state.n_ref == 0;
        drop(inode_state); // release inode lock before potentially dropping the last Arc
        if should_remove {
            table.remove(&inode_id);
        }
        drop(table);

        // 5. Delete temp/delete-on-close files.  Like SQLite's unixClose, we ignore errors
        //    here: a failed unlink is non-fatal and the file will be cleaned up eventually.
        if let Some(path) = delete_on_close {
            let client = get_shell_client();
            let _ = block_on_shell(async move {
                client
                    .remove(Utf8TypedPath::Unix(Utf8UnixPath::new(&path)), false, false)
                    .await
                    .map_err(|_| SQLITE_IOERR)
            });
        }

        // 6. Return any unlock error saved in step 1.
        match unlock_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn canonical_path<'a>(&self, path: Cow<'a, str>) -> VfsResult<Cow<'a, str>> {
        Ok(path)
    }

    fn sector_size(&self, handle: &mut Self::Handle) -> VfsResult<i32> {
        Ok(unsafe { sector_size_for_fd(handle.file.as_raw_fd()) })
    }

    fn device_characteristics(&self, handle: &mut Self::Handle) -> VfsResult<i32> {
        Ok(unsafe { device_characteristics_for_fd(handle.file.as_raw_fd()) })
    }

    /// Map WAL-index region `region_idx` into memory.
    /// Replicates `unixShmMap` in os_unix.c (lines 5106-5239).
    fn shm_map(
        &self,
        handle: &mut Self::Handle,
        region_idx: usize,
        region_size: usize,
        extend: bool,
    ) -> VfsResult<Option<NonNull<u8>>> {
        // Acquire fence: see all writes committed before the preceding shm barrier.
        fence(Ordering::Acquire);

        // Lazily open the shm connection on the first map call.
        open_or_join_shm(handle)?;

        let conn = handle.shm_conn.as_ref().unwrap();
        let mut node = conn.node.lock().unwrap();

        // Re-acquire DMS if a previous attempt was deferred (readonly_cantinit).
        if node.is_unlocked {
            let is_readonly = node.is_readonly;
            let shm_file = node.shm_file.as_ref().unwrap();
            lock_shm_dms(shm_file, is_readonly)?;
            node.is_unlocked = false;
        }

        // On the first call, record the region size and compute the mmap grouping.
        if node.sz_region == 0 {
            let pgsz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
            node.sz_region = region_size;
            // Map in groups of (pgsz / sz_region) to keep all mmap calls page-aligned.
            // For the common case (pgsz=4096 < sz_region=32768) this is 1.
            node.regions_per_map = if pgsz >= region_size {
                pgsz / region_size
            } else {
                1
            };
        }

        // Round needed count up to the next regions_per_map boundary (one mmap group).
        let rpm = node.regions_per_map;
        let needed = ((region_idx / rpm) + 1) * rpm;

        if node.region_ptrs.len() <= region_idx {
            let shm_file = node.shm_file.as_ref().unwrap();
            let needed_bytes = needed * region_size;

            let file_size = shm_file.metadata().map_err(|_| SQLITE_IOERR_SHMSIZE)?.len() as usize;

            if file_size < needed_bytes {
                if !extend || node.is_readonly {
                    return Ok(None);
                }
                // Extend by writing one byte per 4096-byte block in the new range.
                // This forces the OS to allocate the pages immediately, reducing the
                // chance of SIGBUS on later access (matches os_unix.c lines 5192-5207).
                const EXT_BLK: u64 = 4096;
                let start_blk = file_size as u64 / EXT_BLK;
                let end_blk = needed_bytes as u64 / EXT_BLK;
                for blk in start_blk..end_blk {
                    shm_file
                        .write_at(&[0u8], blk * EXT_BLK + EXT_BLK - 1)
                        .map_err(|_| SQLITE_IOERR_SHMSIZE)?;
                }
            }

            // mmap one group at a time until region_ptrs covers region_idx.
            let fd = shm_file.as_raw_fd();
            let prot = if node.is_readonly {
                libc::PROT_READ
            } else {
                libc::PROT_READ | libc::PROT_WRITE
            };

            while node.region_ptrs.len() < needed {
                let group_start = node.region_ptrs.len(); // always a multiple of rpm
                let file_offset = (group_start * region_size) as off_t;
                let mmap_len = rpm * region_size;

                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        mmap_len,
                        prot,
                        libc::MAP_SHARED,
                        fd,
                        file_offset,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    return Err(SQLITE_IOERR_SHMMAP);
                }
                node.mmap_groups.push((ptr as *mut u8, mmap_len));
                for i in 0..rpm {
                    node.region_ptrs
                        .push(unsafe { (ptr as *mut u8).add(i * region_size) });
                }
            }
        }

        let ptr = node.region_ptrs[region_idx];
        Ok(Some(NonNull::new(ptr).unwrap()))
    }

    /// Acquire or release a WAL-index lock.
    /// Replicates `unixShmLock` in os_unix.c (lines 5284-5476).
    ///
    /// `a_lock[i]` encodes intra-process state: 0=unlocked, +N=N shared holders
    /// (piggybacked on one fcntl read lock), -1=exclusive.
    fn shm_lock(
        &self,
        handle: &mut Self::Handle,
        offset: u32,
        count: u32,
        mode: ShmLockMode,
    ) -> VfsResult<()> {
        fence(Ordering::Acquire);

        let conn = handle.shm_conn.as_mut().ok_or(SQLITE_IOERR_SHMLOCK)?;
        let offset = offset as usize;
        let count = count as usize;
        let mask: u16 = ((1u32 << (offset + count)) - (1 << offset)) as u16;

        let mut node = conn.node.lock().unwrap();
        let fd = node
            .shm_file
            .as_ref()
            .ok_or(SQLITE_IOERR_SHMLOCK)?
            .as_raw_fd();

        match mode {
            ShmLockMode::UnlockShared => {
                // SQLite always unlocks one slot at a time for shared locks.
                debug_assert_eq!(count, 1);
                debug_assert_eq!(conn.shared_mask & mask, mask);
                node.a_lock[offset] -= 1;
                if node.a_lock[offset] == 0 {
                    // We were the last shared holder; release the fcntl lock.
                    fcntl_unlock(fd, UNIX_SHM_BASE + offset as off_t, 1)?;
                }
                conn.shared_mask &= !mask;
            }

            ShmLockMode::UnlockExclusive => {
                debug_assert_eq!(conn.excl_mask & mask, mask);
                // Unlock the entire range in one syscall.
                fcntl_unlock(fd, UNIX_SHM_BASE + offset as off_t, count as off_t)?;
                for i in offset..offset + count {
                    node.a_lock[i] = 0;
                }
                conn.excl_mask &= !mask;
            }

            ShmLockMode::LockShared => {
                // SQLite always acquires one slot at a time for shared locks.
                debug_assert_eq!(count, 1);
                if node.a_lock[offset] < 0 {
                    // An exclusive lock is held by another connection in this process.
                    return Err(SQLITE_BUSY);
                }
                if node.a_lock[offset] == 0 {
                    // No existing holder; take the fcntl read lock.
                    shm_fcntl_lock(fd, F_RDLCK, UNIX_SHM_BASE + offset as off_t, 1)?;
                }
                // else: piggyback on an existing shared lock — no syscall needed.
                node.a_lock[offset] += 1;
                conn.shared_mask |= mask;
            }

            ShmLockMode::LockExclusive => {
                // All slots in range must be completely unlocked (intra-process check).
                for i in offset..offset + count {
                    if node.a_lock[i] != 0 {
                        return Err(SQLITE_BUSY);
                    }
                }
                // Acquire the exclusive fcntl write lock on all slots in one call.
                shm_fcntl_lock(fd, F_WRLCK, UNIX_SHM_BASE + offset as off_t, count as off_t)?;
                for i in offset..offset + count {
                    node.a_lock[i] = -1;
                }
                conn.excl_mask |= mask;
            }
        }
        Ok(())
    }

    /// Issue a full memory barrier.
    /// Replicates `unixShmBarrier` in os_unix.c which calls `sqlite3MemoryBarrier()`
    /// (a compiler + hardware barrier).  `SeqCst` is the strongest Rust fence and
    /// implies both a release and an acquire barrier on all architectures.
    fn shm_barrier(&self, _handle: &mut Self::Handle) {
        fence(Ordering::SeqCst);
    }

    /// Disconnect from the WAL shared-memory, optionally deleting the `-shm` file.
    /// Replicates `unixShmUnmap` in os_unix.c (lines 5503-5546).
    fn shm_unmap(&self, handle: &mut Self::Handle, delete: bool) -> VfsResult<()> {
        // Capture state before disconnect_shm clears shm_conn.
        let shm_path = handle
            .shm_conn
            .as_ref()
            .map(|c| c.node.lock().unwrap().shm_path.clone());
        let is_last = handle
            .shm_conn
            .as_ref()
            .map(|c| c.node.lock().unwrap().n_ref == 1)
            .unwrap_or(false);

        disconnect_shm(handle);

        // Optionally delete the `-shm` file after the last connection releases it.
        // Errors are ignored (matching unixShmUnmap which doesn't report unlink errors).
        if delete
            && is_last
            && let Some(path) = shm_path
        {
            let client = get_shell_client();
            let _ = block_on_shell(async move {
                client
                    .remove(Utf8TypedPath::Unix(Utf8UnixPath::new(&path)), false, false)
                    .await
                    .map_err(|_| SQLITE_IOERR)
            });
        }
        Ok(())
    }
}

// ── VFS registration ───────────────────────────────────────────────────────────

/// Register the shell VFS with SQLite.
///
/// Idempotent: safe to call from multiple VMs in the same process — the VFS is
/// registered only once, and subsequent calls return `Ok(())` immediately.
pub(crate) fn register_vfs() -> Result<(), SqliteErr> {
    static REGISTERED: OnceLock<Result<(), SqliteErr>> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        register_static(
            CString::new("dolang-shell").unwrap(),
            ShellVfs,
            RegisterOpts {
                make_default: false,
            },
        )
        .map(|_| ())
    })
}
