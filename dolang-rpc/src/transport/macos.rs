//! Closes the SCM_RIGHTS `FD_CLOEXEC` race described in issue #384.
//!
//! macOS's `recvmsg` has no `MSG_CMSG_CLOEXEC` equivalent, so `unix::recv_once`
//! sets close-on-exec on each received descriptor after the fact via
//! `ioctl(FIOCLEX)`. Between `recvmsg` returning and that `ioctl` completing,
//! a concurrent `fork()` or `posix_spawn`/`posix_spawnp` elsewhere in the
//! process (including code we don't control) can inherit the descriptor.
//! `pthread_atfork` alone doesn't cover this: it never fires for
//! `posix_spawn`-based spawns, which is the path `std::process::Command`
//! takes on macOS whenever it can. So this module serializes descriptor
//! receipt against every spawn in the process:
//!
//! - [`ReadGuard`] is held by `recv_once` across `recvmsg` and the `FIOCLEX`
//!   loop (read mode; concurrent receives don't conflict with each other).
//! - `pthread_atfork` takes the write lock in `prepare` and releases it in
//!   both `parent` and `child`, covering explicit `fork()`.
//! - `posix_spawn`/`posix_spawnp` are interposed process-wide via
//!   `DYLD_INTERPOSE`, taking the write lock around the real call, covering
//!   `Command::spawn()` and any other spawn anywhere in the process.

#[cfg(feature = "macos-spawn-interpose")]
mod guard {
    use std::cell::UnsafeCell;
    use std::ffi::{CStr, c_void};
    use std::hint::black_box;
    use std::sync::atomic::{AtomicPtr, Ordering};

    struct RawRwLock(UnsafeCell<libc::pthread_rwlock_t>);
    // SAFETY: pthread_rwlock_t is designed to be shared across threads; all
    // access goes through pthread_rwlock_* calls, which perform their own
    // synchronization.
    unsafe impl Sync for RawRwLock {}

    static SPAWN_GUARD: RawRwLock = RawRwLock(UnsafeCell::new(libc::PTHREAD_RWLOCK_INITIALIZER));

    /// Held in read mode around `recvmsg` and the `FIOCLEX` fixup loop.
    /// Interposed `posix_spawn`/`posix_spawnp` calls and `fork()` take the
    /// write side, so a descriptor received while a guard is live cannot
    /// yet have been passed to a spawn when it isn't CLOEXEC.
    pub(crate) struct ReadGuard(());

    impl ReadGuard {
        pub(crate) fn acquire() -> Self {
            // SAFETY: SPAWN_GUARD is statically initialized and valid for
            // the lifetime of the process; rdlock is safe to call
            // concurrently from multiple threads.
            let rc = unsafe { libc::pthread_rwlock_rdlock(SPAWN_GUARD.0.get()) };
            debug_assert_eq!(rc, 0, "pthread_rwlock_rdlock failed: {rc}");
            ReadGuard(())
        }
    }

    impl Drop for ReadGuard {
        fn drop(&mut self) {
            // SAFETY: balances the rdlock taken in `acquire`, on the same
            // thread, before this guard is dropped.
            let rc = unsafe { libc::pthread_rwlock_unlock(SPAWN_GUARD.0.get()) };
            debug_assert_eq!(rc, 0, "pthread_rwlock_unlock failed: {rc}");
        }
    }

    extern "C" fn spawn_guard_prepare() {
        // SAFETY: called by libc immediately before fork(); takes the write
        // lock so no recvmsg/FIOCLEX window can be mid-flight across the
        // fork.
        let rc = unsafe { libc::pthread_rwlock_wrlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(
            rc, 0,
            "pthread_rwlock_wrlock failed in atfork prepare: {rc}"
        );
    }

    extern "C" fn spawn_guard_parent() {
        // SAFETY: releases the write lock taken in `spawn_guard_prepare`,
        // running in the parent immediately after fork() returns.
        let rc = unsafe { libc::pthread_rwlock_unlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(rc, 0, "pthread_rwlock_unlock failed in atfork parent: {rc}");
    }

    extern "C" fn spawn_guard_child() {
        // SAFETY: fork() duplicates the lock's "held for write" state, but
        // only the calling thread survives into the child, so no other
        // thread will ever call the matching unlock -- this releases it
        // here instead. This prepare/parent/child pairing is the documented
        // idiom for making a pthread lock usable again immediately after
        // fork() in both branches.
        let rc = unsafe { libc::pthread_rwlock_unlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(rc, 0, "pthread_rwlock_unlock failed in atfork child: {rc}");
    }

    type PosixSpawnFn = unsafe extern "C" fn(
        *mut libc::pid_t,
        *const libc::c_char,
        *const libc::posix_spawn_file_actions_t,
        *const libc::posix_spawnattr_t,
        *const *mut libc::c_char,
        *const *mut libc::c_char,
    ) -> libc::c_int;

    static REAL_POSIX_SPAWN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static REAL_POSIX_SPAWNP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    fn resolve(cache: &AtomicPtr<c_void>, name: &CStr) -> PosixSpawnFn {
        let cached = cache.load(Ordering::Acquire);
        let resolved = if cached.is_null() {
            // SAFETY: RTLD_NEXT plus a NUL-terminated symbol name is
            // dlsym's documented contract; posix_spawn/posix_spawnp are
            // always present in libSystem on any supported macOS target.
            let resolved = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
            assert!(
                !resolved.is_null(),
                "failed to resolve real {name:?} via dlsym"
            );
            // Racy-init is fine: dlsym is safe to call redundantly from a
            // concurrent race, and both threads would resolve the same
            // address.
            cache.store(resolved, Ordering::Release);
            resolved
        } else {
            cached
        };
        // SAFETY: `resolved` is the libc-provided posix_spawn(p) symbol,
        // whose signature matches PosixSpawnFn exactly per libc's own
        // prototype.
        unsafe { std::mem::transmute::<*mut c_void, PosixSpawnFn>(resolved) }
    }

    unsafe extern "C" fn interposed_posix_spawn(
        pid: *mut libc::pid_t,
        path: *const libc::c_char,
        file_actions: *const libc::posix_spawn_file_actions_t,
        attrp: *const libc::posix_spawnattr_t,
        argv: *const *mut libc::c_char,
        envp: *const *mut libc::c_char,
    ) -> libc::c_int {
        let real = resolve(&REAL_POSIX_SPAWN, c"posix_spawn");
        // SAFETY: SPAWN_GUARD is process-wide and statically initialized;
        // wrlock serializes this spawn against any in-flight recvmsg +
        // FIOCLEX window (readers) for the duration of the real call.
        let rc = unsafe { libc::pthread_rwlock_wrlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(
            rc, 0,
            "pthread_rwlock_wrlock failed in interposed posix_spawn: {rc}"
        );
        // SAFETY: forwarding the caller's exact arguments to the real
        // posix_spawn resolved above; signatures match by construction.
        let result = unsafe { real(pid, path, file_actions, attrp, argv, envp) };
        // SAFETY: balances the wrlock above; same thread, same lock.
        let rc = unsafe { libc::pthread_rwlock_unlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(
            rc, 0,
            "pthread_rwlock_unlock failed in interposed posix_spawn: {rc}"
        );
        result
    }

    unsafe extern "C" fn interposed_posix_spawnp(
        pid: *mut libc::pid_t,
        file: *const libc::c_char,
        file_actions: *const libc::posix_spawn_file_actions_t,
        attrp: *const libc::posix_spawnattr_t,
        argv: *const *mut libc::c_char,
        envp: *const *mut libc::c_char,
    ) -> libc::c_int {
        let real = resolve(&REAL_POSIX_SPAWNP, c"posix_spawnp");
        // SAFETY: see interposed_posix_spawn above.
        let rc = unsafe { libc::pthread_rwlock_wrlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(
            rc, 0,
            "pthread_rwlock_wrlock failed in interposed posix_spawnp: {rc}"
        );
        // SAFETY: see interposed_posix_spawn above.
        let result = unsafe { real(pid, file, file_actions, attrp, argv, envp) };
        // SAFETY: see interposed_posix_spawn above.
        let rc = unsafe { libc::pthread_rwlock_unlock(SPAWN_GUARD.0.get()) };
        debug_assert_eq!(
            rc, 0,
            "pthread_rwlock_unlock failed in interposed posix_spawnp: {rc}"
        );
        result
    }

    #[repr(C)]
    struct Interpose {
        replacement: *const c_void,
        replacee: *const c_void,
    }
    // SAFETY: these are raw function pointers, read only by dyld at load
    // time (via the section they're placed in) and by `black_box` below;
    // nothing mutates them.
    unsafe impl Sync for Interpose {}

    #[used]
    #[unsafe(link_section = "__DATA,__interpose")]
    static INTERPOSE_POSIX_SPAWN: Interpose = Interpose {
        replacement: interposed_posix_spawn as *const c_void,
        replacee: libc::posix_spawn as *const c_void,
    };

    #[used]
    #[unsafe(link_section = "__DATA,__interpose")]
    static INTERPOSE_POSIX_SPAWNP: Interpose = Interpose {
        replacement: interposed_posix_spawnp as *const c_void,
        replacee: libc::posix_spawnp as *const c_void,
    };

    #[ctor::ctor]
    fn root_interpose_and_register_atfork() {
        // `#[used]` alone only survives LLVM's own DCE, not ld64's
        // `-dead_strip` (on by default for macOS binaries), and a
        // `.cargo/config.toml` `-no_dead_strip` rustflag can't be relied on
        // since this crate is meant to be embedded: config.toml resolves
        // from the *consuming* workspace, not this crate's tree. Rooting
        // these reads inside a `#[ctor]`-registered function works because
        // entries in `__DATA,__mod_init_func` are never dead-stripped by
        // ld64 (the same reason C++ global constructors survive); the
        // black_box calls stop LLVM from proving the reads are dead and
        // folding them away before object code is emitted.
        black_box(&INTERPOSE_POSIX_SPAWN as *const Interpose);
        black_box(&INTERPOSE_POSIX_SPAWNP as *const Interpose);

        // SAFETY: registers process-wide atfork handlers exactly once, at
        // load time (ctors run before main()); prepare/parent/child only
        // call pthread_rwlock_*, which is async-signal-safe and does not
        // allocate.
        let rc = unsafe {
            libc::pthread_atfork(
                Some(spawn_guard_prepare),
                Some(spawn_guard_parent),
                Some(spawn_guard_child),
            )
        };
        assert_eq!(rc, 0, "pthread_atfork registration failed: {rc}");
    }

    #[cfg(all(test, target_os = "macos", feature = "macos-spawn-interpose"))]
    pub(super) fn spawn_guard_for_test() -> *mut libc::pthread_rwlock_t {
        SPAWN_GUARD.0.get()
    }
}

#[cfg(feature = "macos-spawn-interpose")]
pub(crate) use guard::ReadGuard;

/// No-op fallback when the guard feature is disabled: `recv_once` still
/// calls `ReadGuard::acquire()` unconditionally, so this keeps `unix.rs`
/// free of a second `#[cfg(feature = ...)]` branch.
#[cfg(not(feature = "macos-spawn-interpose"))]
pub(crate) struct ReadGuard;

#[cfg(not(feature = "macos-spawn-interpose"))]
impl ReadGuard {
    pub(crate) fn acquire() -> Self {
        ReadGuard
    }
}

#[cfg(all(test, target_os = "macos", feature = "macos-spawn-interpose"))]
mod tests {
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use bytes::{Bytes, BytesMut};
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use tokio::sync::Mutex;

    use crate::transport::{Receiver, RecvFrame, SendFrame, Sender, unix};

    /// Serializes the tests in this module: they all contend on the
    /// process-wide `SPAWN_GUARD`, and the test harness runs tests on
    /// parallel threads in one process.
    static TEST_LOCK: Mutex<()> = Mutex::const_new(());

    async fn transfer_one_fd() -> std::os::fd::OwnedFd {
        let (left, right) = UnixStream::pair().unwrap();
        let (mut sender, _) = unix::unix(left).unwrap();
        let (_, mut receiver) = unix::unix(right).unwrap();
        receiver.set_max_fds_per_fragment(1);
        let (read_fd, _write_fd) = nix::unistd::pipe().unwrap();
        let mut frame = sender.send();
        assert_eq!(frame.attach_fds(std::slice::from_ref(&read_fd)).unwrap(), 1);
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        let mut buf = BytesMut::with_capacity(1);
        while buf.is_empty() {
            frame.recv(&mut buf).await.unwrap();
        }
        frame.drain_fds().pop().unwrap()
    }

    /// A background thread hammering `Command::spawn()` for the duration of
    /// a test, to force the FD-receipt / spawn race window to actually be
    /// contended rather than hoping for a lucky interleaving.
    struct SpawnPressure {
        stop: Arc<AtomicBool>,
        handles: Vec<std::thread::JoinHandle<()>>,
    }

    impl SpawnPressure {
        fn start(threads: usize) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let handles = (0..threads)
                .map(|_| {
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            let _ = std::process::Command::new("/usr/bin/true").status();
                        }
                    })
                })
                .collect();
            Self { stop, handles }
        }
    }

    impl Drop for SpawnPressure {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for handle in self.handles.drain(..) {
                let _ = handle.join();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn received_descriptors_stay_close_on_exec_under_spawn_pressure() {
        let _serial = TEST_LOCK.lock().await;
        let _pressure = SpawnPressure::start(4);
        for _ in 0..200 {
            let received = transfer_one_fd().await;
            let flags = fcntl(received.as_fd(), FcntlArg::F_GETFD).unwrap();
            assert!(FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn spawned_children_do_not_inherit_received_descriptors() {
        let _serial = TEST_LOCK.lock().await;
        let _pressure = SpawnPressure::start(4);
        for _ in 0..50 {
            let received = transfer_one_fd().await;
            let fd_number = received.as_raw_fd();
            // macOS exposes a per-process /dev/fd, unlike Linux's
            // /proc/self/fd; if the guard failed to prevent inheritance,
            // this same numeric fd would exist and be usable in the child
            // too.
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("test -e /dev/fd/{fd_number}"))
                .status()
                .unwrap();
            assert!(
                !status.success(),
                "descriptor {fd_number} leaked into a spawned child"
            );
        }
    }

    #[test]
    fn lock_is_usable_in_both_branches_after_fork() {
        let _serial = TEST_LOCK.blocking_lock();
        // SAFETY: the child does nothing but a trylock/exit, so no
        // allocation or non-async-signal-safe work happens between fork
        // and exit even though other harness threads may exist.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child: the lock must not be stuck write-locked from the
            // child's perspective after the atfork child handler ran.
            let rc =
                unsafe { libc::pthread_rwlock_trywrlock(super::guard::spawn_guard_for_test()) };
            // SAFETY: exiting immediately via _exit avoids running any
            // Rust destructors or unwinding across the fork in the child.
            unsafe { libc::_exit(if rc == 0 { 0 } else { 1 }) };
        }
        let mut status = 0;
        loop {
            // SAFETY: waiting on the pid we just forked.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited == pid {
                break;
            }
            let err = std::io::Error::last_os_error();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::Interrupted,
                "waitpid failed: {err}"
            );
        }
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);

        // Parent: the lock must also be usable again immediately.
        let rc = unsafe { libc::pthread_rwlock_trywrlock(super::guard::spawn_guard_for_test()) };
        assert_eq!(rc, 0);
        unsafe { libc::pthread_rwlock_unlock(super::guard::spawn_guard_for_test()) };
    }
}
