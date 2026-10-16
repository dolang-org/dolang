use std::collections::HashMap;
use std::{io, path::Path};

#[cfg(any(windows, target_os = "linux"))]
use crate::metadata::AttrFlags;
#[cfg(any(unix, windows))]
use crate::process::{ProcessControl, Signal, TerminationPolicy};
use crate::test_temp::tempdir;
use crate::{
    Vfs,
    error::ErrorKind,
    file::{CopyDest, CopyMode, File, FileLockBehavior, FileLockMode, FileLockRange},
    metadata::{FileType, MetadataPatch},
};
#[cfg(windows)]
use dolang_winterop::security::SecInfo;

fn typed(path: &Path) -> crate::path::Path<'_> {
    crate::path::Path::new(path.to_str().unwrap(), crate::path::Kind::native())
}

#[cfg(any(windows, target_os = "linux"))]
fn attr_patch(flag: AttrFlags, value: bool) -> MetadataPatch {
    let mut patch = MetadataPatch::new();
    patch.attribute(flag, Some(value));
    patch
}

fn typed_str(path: &str) -> crate::path::Path<'_> {
    crate::path::Path::new(path, crate::path::Kind::native())
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn rename_can_refuse_to_replace_destination() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let from = dir.path().join("rename-from");
    let to = dir.path().join("rename-to");
    tokio::fs::write(&from, b"from").await.unwrap();
    tokio::fs::write(&to, b"to").await.unwrap();

    let error = direct
        .rename(typed(&from), typed(&to), false)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(tokio::fs::read(&from).await.unwrap(), b"from");
    assert_eq!(tokio::fs::read(&to).await.unwrap(), b"to");

    tokio::fs::remove_file(&to).await.unwrap();
    direct
        .rename(typed(&from), typed(&to), false)
        .await
        .unwrap();
    assert!(!from.exists());
    assert_eq!(tokio::fs::read(&to).await.unwrap(), b"from");
}

#[cfg(target_os = "freebsd")]
#[tokio::test]
async fn rename_without_replacement_is_unsupported() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let from = dir.path().join("rename-from");
    let to = dir.path().join("rename-to");
    tokio::fs::write(&from, b"from").await.unwrap();

    let error = direct
        .rename(typed(&from), typed(&to), false)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(tokio::fs::read(&from).await.unwrap(), b"from");
    assert!(!to.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn rename_replaces_an_open_destination() {
    use std::{io::Read as _, os::windows::fs::OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    if dolang_winterop::is_wine() {
        return;
    }

    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let from = dir.path().join("rename-from");
    let to = dir.path().join("rename-to");
    tokio::fs::write(&from, b"from").await.unwrap();
    tokio::fs::write(&to, b"to").await.unwrap();
    let mut old_destination = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&to)
        .unwrap();

    direct.rename(typed(&from), typed(&to), true).await.unwrap();
    assert_eq!(tokio::fs::read(&to).await.unwrap(), b"from");
    let mut old_contents = String::new();
    old_destination.read_to_string(&mut old_contents).unwrap();
    assert_eq!(old_contents, "to");
}

#[cfg(unix)]
async fn wait_for_pid(path: &Path) -> libc::pid_t {
    for _ in 0..200 {
        if let Ok(pid) = tokio::fs::read_to_string(path)
            .await
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().map_err(io::Error::other))
        {
            return pid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("child did not write {}", path.display());
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    (unsafe { libc::kill(pid, 0) }) == 0
}

#[cfg(unix)]
#[tokio::test]
async fn background_termination_signals_the_process_group() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let pid_path = dir.path().join("descendant.pid");
    let script = format!("sleep 60 & echo $! > '{}'; wait", pid_path.display());
    let mut command = direct.command(crate::path::Path::unix("sh"));
    command
        .arg("-c")
        .arg(&script)
        .process_control(ProcessControl::Background)
        .termination_policy(TerminationPolicy::new(
            Signal::Term,
            std::time::Duration::from_secs(1),
            true,
        ));
    let child = command.spawn().await.unwrap();
    let descendant = wait_for_pid(&pid_path).await;

    assert!(child.terminate().await.unwrap().is_some());
    for _ in 0..100 {
        if !process_exists(descendant) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("background descendant {descendant} survived group termination");
}

#[cfg(unix)]
#[tokio::test]
async fn force_false_orphans_the_background_process_group() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let pid_path = dir.path().join("group.pid");
    let script = format!(
        "trap '' TERM; echo $$ > '{}'; sleep 60 & wait",
        pid_path.display()
    );
    let mut command = direct.command(crate::path::Path::unix("sh"));
    command
        .arg("-c")
        .arg(&script)
        .process_control(ProcessControl::Background)
        .termination_policy(TerminationPolicy::new(
            Signal::Term,
            std::time::Duration::from_millis(50),
            false,
        ));
    let child = command.spawn().await.unwrap();
    let group = wait_for_pid(&pid_path).await;

    assert_eq!(child.terminate().await.unwrap(), None);
    assert!(process_exists(group));
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
}

#[cfg(windows)]
async fn assert_windows_termination(control: ProcessControl) {
    let direct = Vfs::direct().unwrap();
    let mut command = direct.command(crate::path::Path::windows("cmd"));
    command
        .arg("/C")
        .arg("ping -n 60 127.0.0.1 >nul")
        .process_control(control)
        .termination_policy(TerminationPolicy::new(
            Signal::Term,
            std::time::Duration::from_millis(50),
            true,
        ));
    let child = command.spawn().await.unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.terminate())
        .await
        .unwrap()
        .unwrap();
    assert!(status.is_some());
}

#[cfg(windows)]
#[tokio::test]
async fn windows_foreground_process_group_can_be_terminated() {
    assert_windows_termination(ProcessControl::Foreground).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_background_job_can_be_terminated() {
    assert_windows_termination(ProcessControl::Background).await;
}

fn lock_request(
    start: u64,
    end: Option<u64>,
    mode: FileLockMode,
    behavior: FileLockBehavior,
) -> (FileLockRange, FileLockMode, FileLockBehavior) {
    (FileLockRange::new(start, end).unwrap(), mode, behavior)
}

trait FileLockTestExt {
    async fn lock_request(
        &self,
        request: (FileLockRange, FileLockMode, FileLockBehavior),
    ) -> crate::error::Result<Option<crate::file::FileLock>>;
}

impl FileLockTestExt for File {
    async fn lock_request(
        &self,
        (range, mode, behavior): (FileLockRange, FileLockMode, FileLockBehavior),
    ) -> crate::error::Result<Option<crate::file::FileLock>> {
        self.lock(range, mode, behavior).await
    }
}

async fn open_lock_file(direct: &Vfs, path: &Path) -> File {
    direct
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(typed(path))
        .await
        .unwrap()
}

#[cfg(not(target_os = "freebsd"))]
#[tokio::test]
async fn byte_range_locks_contend_and_release() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("locks");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;

    let mut exclusive = first
        .lock_request(lock_request(
            0,
            Some(10),
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(
        second
            .lock_request(lock_request(
                0,
                Some(10),
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap()
            .is_none()
    );
    let mut adjacent = second
        .lock_request(lock_request(
            10,
            Some(20),
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    adjacent.release().await.unwrap();
    exclusive.release().await.unwrap();
    assert!(
        second
            .lock_request(lock_request(
                0,
                Some(10),
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap()
            .is_some()
    );
}

// Whole-file locks work everywhere on Unix, FreeBSD's `flock` included, and the
// child inherits this very descriptor there, so it would share the lock on
// every one of these platforms if the handoff did not lift it first.
#[cfg(unix)]
#[tokio::test]
async fn handing_a_file_to_a_child_releases_its_locks() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("closed-locks");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;

    let mut held = first
        .lock_request(lock_request(
            0,
            None,
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap()
        .unwrap();
    // The description outlives this handle in the child, so closing our end
    // would not lift the lock; the handoff has to unlock in band, exactly as
    // `close` does.
    let handed_over = first.into_stdio_send(0).await.unwrap();

    assert!(
        second
            .lock_request(lock_request(
                0,
                None,
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap()
            .is_some()
    );
    // Already released above; releasing again must be a no-op rather than an
    // error, since the script never asked for the lock to come off.
    held.release().await.unwrap();
    drop(handed_over);
}

#[cfg(target_os = "freebsd")]
#[tokio::test]
async fn byte_range_locks_are_rejected() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("byte-range-locks");
    let file = open_lock_file(&direct, &path).await;

    for (start, end) in [(0, Some(10)), (1, None)] {
        let error = file
            .lock_request(lock_request(
                start,
                end,
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
}

#[tokio::test]
async fn shared_locks_and_same_handle_overlap_rules() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("shared-locks");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;
    let third = open_lock_file(&direct, &path).await;

    let _first_shared = first
        .lock_request(lock_request(
            0,
            None,
            FileLockMode::Shared,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap()
        .unwrap();
    let _second_shared = second
        .lock_request(lock_request(
            0,
            None,
            FileLockMode::Shared,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(
        third
            .lock_request(lock_request(
                0,
                None,
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        first
            .lock_request(lock_request(
                0,
                None,
                FileLockMode::Shared,
                FileLockBehavior::Try,
            ))
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn finite_empty_range_is_rejected() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty-lock");
    let first = open_lock_file(&direct, &path).await;

    let error = first
        .lock_request(lock_request(
            4,
            Some(4),
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(windows)]
#[tokio::test]
async fn finite_empty_range_uses_native_windows_behavior() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty-lock");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;

    let error = first
        .lock_request(lock_request(
            0,
            Some(0),
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

    let _empty = first
        .lock_request(lock_request(
            4,
            Some(4),
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap()
        .unwrap();

    let mut same = first
        .lock_request(lock_request(
            4,
            Some(4),
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    same.release().await.unwrap();

    assert!(
        second
            .lock_request(lock_request(
                0,
                None,
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap()
            .is_none()
    );

    let mut same = second
        .lock_request(lock_request(
            4,
            Some(4),
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    same.release().await.unwrap();

    let mut ending_at_offset = second
        .lock_request(lock_request(
            0,
            Some(4),
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    ending_at_offset.release().await.unwrap();

    let mut starting_at_offset = second
        .lock_request(lock_request(
            4,
            Some(8),
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    starting_at_offset.release().await.unwrap();

    let mut open_ended_at_offset = second
        .lock_request(lock_request(
            4,
            None,
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap()
        .unwrap();
    open_ended_at_offset.release().await.unwrap();

    let error = second
        .lock_request(lock_request(
            u64::MAX,
            None,
            FileLockMode::Exclusive,
            FileLockBehavior::Try,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

    assert!(
        second
            .lock_request(lock_request(
                3,
                Some(5),
                FileLockMode::Exclusive,
                FileLockBehavior::Try,
            ))
            .await
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
fn failing_exit_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "exit 42"])
}

#[cfg(windows)]
fn failing_exit_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "exit 42"])
}

#[cfg(unix)]
fn env_forwarding_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", r#"test "$TEST_VAR" = value"#])
}

#[cfg(unix)]
fn successful_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "exit 0"])
}

#[cfg(windows)]
fn env_forwarding_command() -> (&'static str, [&'static str; 2]) {
    (
        "cmd",
        ["/C", r#"if "%TEST_VAR%"=="value" exit 0 else exit 1"#],
    )
}

#[cfg(windows)]
fn successful_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "exit 0"])
}

#[tokio::test]
async fn direct_open_options_round_trip() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");

    let mut options = direct.open_options();
    let mut file = options
        .write(true)
        .create(true)
        .truncate(true)
        .open(typed(&path))
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut file, b"hello")
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::flush(&mut file).await.unwrap();
    drop(file);

    let contents = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(contents, "hello");
}

/// A FIFO has no seekable offset, so `pread`/`pwrite` fail on it with
/// `ESPIPE` where plain `read`/`write` succeed. Streaming a non-regular file
/// has to keep working, since `open` reaches FIFOs, terminals and sockets just
/// as readily as it reaches regular files.
#[cfg(unix)]
#[tokio::test]
async fn direct_streams_a_non_seekable_file() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("fifo");
    nix::unistd::mkfifo(path.as_path(), nix::sys::stat::Mode::S_IRWXU).unwrap();

    let mut options = direct.open_options();
    // Opening read-write keeps the FIFO from blocking for a peer.
    let mut file = options
        .read(true)
        .write(true)
        .open(typed(&path))
        .await
        .unwrap();

    tokio::io::AsyncWriteExt::write_all(&mut file, b"through the pipe")
        .await
        .unwrap();

    let mut buf = [0u8; 16];
    tokio::io::AsyncReadExt::read_exact(&mut file, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf, b"through the pipe");

    file.close().await.unwrap();
}

/// Positional I/O names the offset it acts on and leaves the stream cursor
/// alone, in both directions.
#[tokio::test]
async fn direct_positional_io_ignores_the_cursor() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("positional.bin");
    std::fs::write(&path, b"0123456789").unwrap();

    let mut options = direct.open_options();
    let mut file = options
        .read(true)
        .write(true)
        .open(typed(&path))
        .await
        .unwrap();

    // Move the stream cursor somewhere unrelated first.
    assert_eq!(
        tokio::io::AsyncSeekExt::seek(&mut file, io::SeekFrom::Start(7))
            .await
            .unwrap(),
        7
    );

    let mut buf = bytes::BytesMut::with_capacity(4);
    assert_eq!(file.read_at(&mut buf, 2).await.unwrap(), 4);
    assert_eq!(&buf[..], b"2345");

    assert_eq!(
        file.write_at(bytes::Bytes::from_static(b"ab"), 0)
            .await
            .unwrap(),
        2
    );

    // The cursor is exactly where the seek left it.
    assert_eq!(
        tokio::io::AsyncSeekExt::stream_position(&mut file)
            .await
            .unwrap(),
        7
    );

    file.close().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"ab23456789");
}

/// Reading past the end reports end of file by returning no bytes, and a
/// short read at the boundary returns only what exists.
#[tokio::test]
async fn direct_read_at_reports_short_reads_and_eof() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("short.bin");
    std::fs::write(&path, b"abcdef").unwrap();

    let mut options = direct.open_options();
    let file = options.read(true).open(typed(&path)).await.unwrap();

    let mut buf = bytes::BytesMut::with_capacity(64);
    assert_eq!(file.read_at(&mut buf, 4).await.unwrap(), 2);
    assert_eq!(&buf[..], b"ef");

    // Reads append into the buffer's spare capacity rather than replacing
    // what is there, so a recycled buffer has to be cleared first.
    let before = buf.len();
    assert_eq!(file.read_at(&mut buf, 99).await.unwrap(), 0);
    assert_eq!(buf.len(), before, "a read past the end adds nothing");

    buf.clear();
    assert_eq!(
        file.read_at(&mut buf, 99).await.unwrap(),
        0,
        "end of file is a zero-length transfer"
    );
    assert!(buf.is_empty());

    file.close().await.unwrap();
}

/// The borrowed-destination read agrees with the owned-buffer one on counts and
/// end of file, and reports only what it actually filled.
#[tokio::test]
async fn direct_read_at_into_fills_the_front_of_the_destination() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("into.bin");
    std::fs::write(&path, b"abcdef").unwrap();

    let mut options = direct.open_options();
    let file = options.read(true).open(typed(&path)).await.unwrap();

    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 64];
    let read = file.read_at_into(&mut buf, 4).await.unwrap();
    assert_eq!(read, 2);
    // Only the reported prefix may be read back; the rest is still
    // uninitialized and stays that way.
    let filled: Vec<u8> = buf[..read]
        .iter()
        .map(|byte| unsafe { byte.assume_init() })
        .collect();
    assert_eq!(filled, b"ef");

    assert_eq!(
        file.read_at_into(&mut buf, 99).await.unwrap(),
        0,
        "end of file is a zero-length transfer"
    );

    let mut empty: [std::mem::MaybeUninit<u8>; 0] = [];
    assert_eq!(
        file.read_at_into(&mut empty, 0).await.unwrap(),
        0,
        "nowhere to put anything is a zero-length transfer, not an error"
    );

    file.close().await.unwrap();
}

/// The borrowed-source write agrees with the owned one, and refuses an explicit
/// offset on an append handle for the same reason `write_at` does.
#[tokio::test]
async fn direct_write_at_from_borrows_its_source() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("from.bin");
    std::fs::write(&path, b"......").unwrap();

    let mut options = direct.open_options();
    let file = options
        .read(true)
        .write(true)
        .open(typed(&path))
        .await
        .unwrap();

    let source = b"xy".to_vec();
    assert_eq!(file.write_at_from(&source, 2).await.unwrap(), 2);
    assert_eq!(std::fs::read(&path).unwrap(), b"..xy..");
    file.close().await.unwrap();

    let mut options = direct.open_options();
    let appender = options
        .append(true)
        .write(true)
        .open(typed(&path))
        .await
        .unwrap();
    let error = appender.write_at_from(&source, 0).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    appender.close().await.unwrap();
}

/// A handoff the handle is too busy for must hand the handle back intact, so a
/// caller can retry once the outstanding work finishes.
#[tokio::test]
async fn a_busy_handoff_returns_the_handle() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("busy.bin");
    std::fs::write(&path, b"abcdef").unwrap();

    let mut options = direct.open_options();
    let file = options.read(true).open(typed(&path)).await.unwrap();

    // A positional operation takes its own reference to the descriptor when it
    // is created, so holding the future without polling it is enough to make
    // the handle non-exclusive.
    let mut buf = bytes::BytesMut::with_capacity(8);
    let pending = file.read_at(&mut buf, 0);

    let failed = file.into_stdio_send(0).await.unwrap_err();
    assert_eq!(failed.error().kind(), crate::error::ErrorKind::ResourceBusy);
    let file = failed.into_handle();

    // Nothing was surrendered, so the handle still works and the retry lands
    // once the outstanding operation is gone.
    assert_eq!(pending.await.unwrap(), 6);
    assert_eq!(&buf[..], b"abcdef");
    drop(file.into_stdio_send(0).await.unwrap());
}

/// The point of the positional API: many operations in flight on one handle,
/// with no serialization between them.
#[tokio::test]
async fn direct_positional_reads_run_concurrently() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("windows.bin");
    let payload: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    std::fs::write(&path, &payload).unwrap();

    let mut options = direct.open_options();
    let file = options.read(true).open(typed(&path)).await.unwrap();

    // Issue every window before awaiting any of them. This only compiles
    // because the operations take `&self` and their futures do not borrow the
    // handle — only their own buffer, which is why each window needs one.
    let mut bufs: Vec<_> = (0..8).map(|_| bytes::BytesMut::with_capacity(8)).collect();
    let pending: Vec<_> = bufs
        .iter_mut()
        .enumerate()
        .map(|(i, buf)| file.read_at(buf, i as u64 * 8))
        .collect();

    for future in pending {
        assert_eq!(future.await.unwrap(), 8);
    }
    let seen: Vec<u8> = bufs.iter().flat_map(|buf| buf.iter().copied()).collect();
    assert_eq!(seen, payload);

    file.close().await.unwrap();
}

/// An append handle has no meaningful notion of "write here", so asking for
/// one is refused rather than silently appending — which is what the kernel
/// would do with the offset thrown away.
#[tokio::test]
async fn direct_write_at_is_rejected_on_an_append_handle() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("append.log");
    std::fs::write(&path, b"start").unwrap();

    let mut options = direct.open_options();
    let file = options.append(true).open(typed(&path)).await.unwrap();

    let error = file
        .write_at(bytes::Bytes::from_static(b"x"), 0)
        .await
        .expect_err("positional write on an append handle");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    // The atomic append path still works, and reports where it landed.
    let (written, end) = file
        .append(bytes::Bytes::from_static(b"more"))
        .await
        .unwrap();
    assert_eq!(written, 4);
    assert_eq!(end, 9);

    file.close().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"startmore");
}

/// Closing while an operation is still outstanding consumes the handle and
/// lets cleanup finish asynchronously, but says so — the same contract the
/// remote backend has always had for an opaque file still in use.
#[tokio::test]
async fn direct_close_reports_busy_with_an_operation_outstanding() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("busy.bin");
    std::fs::write(&path, vec![7u8; 1024]).unwrap();

    let mut options = direct.open_options();
    let file = options.read(true).open(typed(&path)).await.unwrap();

    let mut buf = bytes::BytesMut::with_capacity(1024);
    let outstanding = file.read_at(&mut buf, 0);
    let error = file
        .close()
        .await
        .expect_err("close with an operation outstanding");
    assert_eq!(error.kind(), crate::error::ErrorKind::ResourceBusy);

    // The detached operation still completes against the descriptor it holds.
    assert_eq!(outstanding.await.unwrap(), 1024);
    assert_eq!(buf.len(), 1024);
}

#[tokio::test]
async fn direct_symlink_metadata_and_read_link() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    tokio::fs::write(&target, "hello").await.unwrap();

    direct
        .symlink(typed_str(""), typed(&target), typed(&link))
        .await
        .unwrap();

    let metadata = direct.symlink_metadata(typed(&link)).await.unwrap();
    assert_eq!(metadata.file_type(), FileType::Symlink);
    assert_eq!(
        direct.read_link(typed(&link)).await.unwrap().as_str(),
        target.to_str().unwrap()
    );
}

#[tokio::test]
async fn direct_copy_symlink_preserves_link() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    let copied = dir.path().join("copied.txt");
    tokio::fs::write(&target, "hello").await.unwrap();

    direct
        .symlink(typed_str(""), typed(&target), typed(&link))
        .await
        .unwrap();
    direct
        .copy(typed(&link), typed(&copied), false)
        .await
        .unwrap();

    let metadata = direct.symlink_metadata(typed(&copied)).await.unwrap();
    assert_eq!(metadata.file_type(), FileType::Symlink);
    assert_eq!(
        direct.read_link(typed(&copied)).await.unwrap().as_str(),
        target.to_str().unwrap()
    );
}

#[tokio::test]
async fn direct_hard_link_round_trip() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    tokio::fs::write(&target, "hello").await.unwrap();

    direct
        .hard_link(typed(&target), typed(&link))
        .await
        .unwrap();

    assert_eq!(tokio::fs::read_to_string(&link).await.unwrap(), "hello");
}

#[cfg(windows)]
#[tokio::test]
async fn direct_metadata_windows_attributes() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("readonly.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let mut permissions = tokio::fs::metadata(&path).await.unwrap().permissions();
    permissions.set_readonly(true);
    tokio::fs::set_permissions(&path, permissions)
        .await
        .unwrap();

    let metadata = direct.metadata(typed(&path)).await.unwrap();

    assert!(metadata.windows().is_some());
    let attrs = metadata.win_attrs().unwrap();
    assert_ne!(attrs, 0);
    assert_ne!(attrs & 0x0000_0001, 0);
    assert_ne!(attrs & 0x1, 0);
}

#[tokio::test]
async fn direct_fs_metadata_basic() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("fsmeta.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let metadata = direct.fs_metadata(typed(&path), true).await.unwrap();
    assert!(metadata.capacity() > 0);
    assert!(metadata.free() > 0);
    assert!(metadata.available() > 0);
    assert!(metadata.block_size() > 0 || cfg!(windows));
}

#[tokio::test]
async fn direct_file_fs_metadata_basic() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("fsmeta-file.txt");
    tokio::fs::write(&path, "hello").await.unwrap();
    let file = direct
        .open_options()
        .read(true)
        .open(typed(&path))
        .await
        .unwrap();

    let metadata = file.fs_metadata().await.unwrap();
    assert!(metadata.capacity() > 0);
    assert!(metadata.free() > 0);
    assert!(metadata.available() > 0);
}

#[cfg(windows)]
#[tokio::test]
async fn direct_security_descriptor_path_and_file() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("security.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let mask = SecInfo::OWNER | SecInfo::GROUP | SecInfo::DACL;
    let descriptor = direct.sec_desc(typed(&path), mask, true).await.unwrap();
    assert_eq!(descriptor.mask(), mask);
    assert!(descriptor.owner_loaded());
    assert!(descriptor.owner().is_some());
    assert!(descriptor.group_loaded());
    assert!(descriptor.dacl_loaded());

    let dacl = direct
        .sec_desc(typed(&path), SecInfo::DACL, true)
        .await
        .unwrap();
    // Writing the file's own DACL back must succeed: the handle is opened with
    // exactly the rights that write needs, and the ACEs are the ones Windows
    // just handed us. Tolerating PermissionDenied here is what let a missing
    // READ_CONTROL on the handle go unnoticed — the assertion below was never
    // reached, because every DACL write took the error branch.
    direct
        .update_sec_desc(typed(&path), &dacl, true)
        .await
        .unwrap();
    let round_trip = direct
        .sec_desc(typed(&path), SecInfo::DACL, true)
        .await
        .unwrap();
    assert_eq!(round_trip.dacl(), dacl.dacl());

    let file = direct
        .open_options()
        .read(true)
        .open(typed(&path))
        .await
        .unwrap();
    let file_descriptor = file.sec_desc(SecInfo::OWNER).await.unwrap();
    assert!(file_descriptor.owner().is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_security_descriptors_are_unsupported() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("security.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let error = direct
        .sec_desc(
            typed(&path),
            dolang_winterop::security::SecInfo::empty(),
            true,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(unix)]
#[tokio::test]
async fn direct_update_metadata_rejects_created_timestamp() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("timestamps.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let err = direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch::new().with_created(1_000_000_000),
        )
        .await
        .unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(windows)]
#[tokio::test]
async fn direct_windows_attrs() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("attrs.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::READONLY, true),
        )
        .await
        .unwrap();

    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_ne!(attrs & 0x1, 0);

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::READONLY, false),
        )
        .await
        .unwrap();

    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_eq!(attrs & 0x1, 0);

    if dolang_winterop::is_wine() {
        return;
    }

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::COMPRESSED, true),
        )
        .await
        .unwrap();

    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_ne!(attrs & 0x800, 0);

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::COMPRESSED, false),
        )
        .await
        .unwrap();

    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_eq!(attrs & 0x800, 0);

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::SPARSE, true),
        )
        .await
        .unwrap();
    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_ne!(attrs & 0x200, 0);

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::SPARSE, false),
        )
        .await
        .unwrap();
    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_eq!(attrs & 0x200, 0);
}

#[cfg(windows)]
#[tokio::test]
async fn direct_windows_streams() {
    if dolang_winterop::is_wine() {
        return;
    }

    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("streams.txt");
    let stream_path = dir.path().join("streams.txt:zone");
    tokio::fs::write(&path, "base").await.unwrap();
    tokio::fs::write(&stream_path, "stream").await.unwrap();

    let file = direct
        .open_options()
        .read(true)
        .open(typed(&path))
        .await
        .unwrap();
    let streams = file.streams().await.unwrap();
    assert!(streams.iter().any(|entry| {
        entry.name().is_empty()
            && entry.stream_type() == "DATA"
            && entry.size() == 4
            && entry.alloc_size() >= entry.size()
    }));
    assert!(streams.iter().any(|entry| {
        entry.name() == "zone"
            && entry.stream_type() == "DATA"
            && entry.size() == 6
            && entry.alloc_size() >= entry.size()
    }));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_linux_attrs() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("attrs.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let _attrs = match direct.metadata(typed(&path)).await {
        Ok(metadata) => metadata.linux_attrs().unwrap(),
        Err(err)
            if matches!(
                err.raw_os_error(),
                Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL)
            ) =>
        {
            return;
        }
        Err(err) => panic!("attrs failed: {err}"),
    };

    if let Err(err) = direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::NO_DUMP, true),
        )
        .await
    {
        if matches!(
            err.raw_os_error(),
            Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL | libc::EPERM)
        ) || err.kind() == io::ErrorKind::PermissionDenied
        {
            return;
        }
        panic!("update_metadata failed: {err}");
    }

    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .linux_attrs()
        .unwrap();
    assert_ne!(attrs & 0x40, 0);

    direct
        .update_metadata(
            &[typed(&path).to_path_buf()],
            attr_patch(AttrFlags::NO_DUMP, false),
        )
        .await
        .unwrap();
    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .linux_attrs()
        .unwrap();
    assert_eq!(attrs & 0x40, 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_metadata_handles_unix_socket_without_inode_attrs() {
    use std::os::unix::net::UnixListener;

    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("metadata.sock");
    let _listener = UnixListener::bind(&path).unwrap();

    let metadata = direct.metadata(typed(&path)).await.unwrap();
    assert_eq!(metadata.file_type(), FileType::Socket);
    assert_eq!(metadata.linux_attrs(), None);
}

#[tokio::test]
async fn direct_copy_move_and_glob() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let nested = src.join("nested");
    let copied = dir.path().join("copied");
    let moved = dir.path().join("moved");

    tokio::fs::create_dir_all(&nested).await.unwrap();
    tokio::fs::write(nested.join("file.txt"), "hello")
        .await
        .unwrap();

    direct
        .copy(typed(&src), typed(&copied), true)
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(copied.join("nested").join("file.txt"))
            .await
            .unwrap(),
        "hello"
    );

    direct
        .move_(typed(&copied), typed(&moved), true)
        .await
        .unwrap();
    assert!(!copied.exists());

    let matches = direct
        .glob("**/*.txt", typed(dir.path()), false, None)
        .await
        .unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(
        matches
            .iter()
            .filter(|path| path.file_name().is_some_and(|name| name == "file.txt"))
            .count(),
        2
    );
}

#[tokio::test]
async fn direct_remove_dir_ignore_prunes_empty_branches() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(root.join("keep").join("child"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(root.join("prune").join("leaf"))
        .await
        .unwrap();
    tokio::fs::write(root.join("keep").join("file.txt"), "hello")
        .await
        .unwrap();

    direct.remove_dir(typed(&root), true, true).await.unwrap();

    assert!(root.exists());
    assert!(root.join("keep").exists());
    assert!(!root.join("prune").exists());
}

#[tokio::test]
async fn direct_basic_spawn() {
    let direct = Vfs::direct().unwrap();
    let (program, args) = successful_command();
    let mut command = direct.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn direct_spawn_failure() {
    let direct = Vfs::direct().unwrap();
    let result = direct
        .command(typed_str("nonexistent_command_12345"))
        .spawn()
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn direct_exit_code() {
    let direct = Vfs::direct().unwrap();
    let (program, args) = failing_exit_command();
    let mut command = direct.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(!status.success());
    assert_eq!(status.code(), Some(42));
}

#[tokio::test]
async fn direct_env_vars() {
    let direct = Vfs::direct().unwrap();
    let (program, args) = env_forwarding_command();
    let mut command = direct.command(typed_str(program));
    command.arg(args[0]).arg(args[1]).env("TEST_VAR", "value");
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_home_dir_prefers_absolute_home_override() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([(String::from("HOME"), Some(String::from("/tmp/test-home")))]);

    let path = direct
        .well_known_path(crate::path::WellKnownPath::HomeDir, None, &env)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-home");
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_temp_dir_prefers_tmpdir_override() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([(String::from("TMPDIR"), Some(String::from("/tmp/test-temp")))]);

    let path = direct
        .well_known_path(crate::path::WellKnownPath::TempDir, None, &env)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-temp");
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_temp_dir_falls_back_to_tmp() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([(String::from("TMPDIR"), None)]);

    let path = direct
        .well_known_path(crate::path::WellKnownPath::TempDir, None, &env)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(path.as_str(), "/tmp");
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_home_dir_rejects_relative_home_override() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([(String::from("HOME"), Some(String::from("relative-home")))]);

    let err = direct
        .well_known_path(crate::path::WellKnownPath::HomeDir, None, &env)
        .await
        .unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn direct_well_known_cache_dir_prefers_xdg_override() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([
        (
            String::from("XDG_CACHE_HOME"),
            Some(String::from("/tmp/test-cache")),
        ),
        (String::from("HOME"), Some(String::from("/tmp/test-home"))),
    ]);

    let path = direct
        .well_known_path(crate::path::WellKnownPath::CacheDir, None, &env)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-cache");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn direct_well_known_cache_dir_falls_back_to_home() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([
        (String::from("HOME"), Some(String::from("/tmp/test-home"))),
        (String::from("XDG_CACHE_HOME"), None),
    ]);

    let path = direct
        .well_known_path(crate::path::WellKnownPath::CacheDir, None, &env)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-home/.cache");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn direct_well_known_cache_dir_uses_macos_convention() {
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([
        (String::from("HOME"), Some(String::from("/tmp/test-home"))),
        (
            String::from("XDG_CACHE_HOME"),
            Some(String::from("/tmp/test-cache")),
        ),
    ]);

    let path = direct
        .well_known_path(crate::path::WellKnownPath::CacheDir, None, &env)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-home/Library/Caches");
}

/// Opens `path`, creating it with `content`, for reading and writing.
async fn open_rw(vfs: &Vfs, path: &Path, content: &[u8]) -> File {
    std::fs::write(path, content).unwrap();
    vfs.open_options()
        .read(true)
        .write(true)
        .open(typed(path))
        .await
        .unwrap()
}

#[tokio::test]
async fn copy_data_moves_a_range_between_two_files() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("src.bin");
    let dst_path = dir.path().join("dst.bin");
    let src = open_rw(&direct, &src_path, b"0123456789").await;
    let dst = open_rw(&direct, &dst_path, b"XXXXXXXXXX").await;

    assert_eq!(
        src.copy_data(&dst, 2, CopyDest::At(3), Some(3), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        3
    );
    // A length past the end of the source is short, not an error.
    assert_eq!(
        src.copy_data(&dst, 8, CopyDest::At(0), Some(100), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        2
    );
    // An empty request transfers nothing.
    assert_eq!(
        src.copy_data(&dst, 0, CopyDest::At(0), Some(0), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        0
    );
    dst.close().await.unwrap();

    // `CopyDest::Append` goes through the handle's append behavior, so it
    // belongs to a handle opened for appending. No length at all means "to
    // end of source".
    let appending = direct
        .open_options()
        .append(true)
        .open(typed(&dst_path))
        .await
        .unwrap();
    let appended = src
        .copy_data(&appending, 9, CopyDest::Append, None, CopyMode::Auto)
        .await
        .unwrap();
    assert_eq!(appended.count, 1);
    assert_eq!(appended.destination_end, Some(11));

    src.close().await.unwrap();
    appending.close().await.unwrap();
    assert_eq!(std::fs::read(&dst_path).unwrap(), b"89X234XXXX9");
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
#[tokio::test]
async fn positional_copy_replaces_prefilled_bytes_with_sparse_holes() {
    use std::os::unix::fs::{FileExt as _, MetadataExt as _};

    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("sparse-src.bin");
    let raw_src = std::fs::File::create(&src_path).unwrap();
    raw_src.write_all_at(&vec![b'A'; 4096], 4096).unwrap();
    raw_src.write_all_at(&vec![b'B'; 4096], 12 * 1024).unwrap();
    raw_src.set_len(20 * 1024).unwrap();

    let src = direct
        .open_options()
        .read(true)
        .open(typed(&src_path))
        .await
        .unwrap();

    for (name, mode) in [("auto", CopyMode::Auto), ("never", CopyMode::Never)] {
        let dst_path = dir.path().join(format!("sparse-dst-{name}.bin"));
        // Shorter than the copied logical end so the trailing source hole must
        // extend the destination after replacing its existing prefix.
        std::fs::write(&dst_path, vec![0xaa; 22 * 1024]).unwrap();
        let dst = direct
            .open_options()
            .read(true)
            .write(true)
            .open(typed(&dst_path))
            .await
            .unwrap();
        let result = src
            .copy_data(&dst, 0, CopyDest::At(4096), Some(20 * 1024), mode)
            .await
            .unwrap();
        assert_eq!(result.count, 20 * 1024);
        dst.close().await.unwrap();

        let copied = std::fs::read(&dst_path).unwrap();
        assert_eq!(&copied[..4096], &vec![0xaa; 4096]);
        assert_eq!(&copied[4096..8192], &vec![0; 4096]);
        assert_eq!(&copied[8192..12_288], &vec![b'A'; 4096]);
        assert_eq!(&copied[12_288..16_384], &vec![0; 4096]);
        assert_eq!(&copied[16_384..20_480], &vec![b'B'; 4096]);
        assert_eq!(&copied[20_480..24_576], &vec![0; 4096]);
        assert_eq!(copied.len(), 24_576);

        let metadata = std::fs::metadata(&dst_path).unwrap();
        if raw_src.metadata().unwrap().blocks() * 512 < 20 * 1024 {
            // Filesystems that expose source extents should leave at least
            // some of the replaced range unallocated when punching works.
            assert!(metadata.blocks() * 512 < metadata.len());
        }
    }
    src.close().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_positional_copy_preserves_sparse_holes_without_marking_destination() {
    use std::os::windows::fs::FileExt as _;

    if dolang_winterop::is_wine() {
        return;
    }
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("sparse-src.bin");
    let dst_path = dir.path().join("dense-dst.bin");
    std::fs::write(&src_path, []).unwrap();
    direct
        .update_metadata(
            &[typed(&src_path).to_path_buf()],
            attr_patch(AttrFlags::SPARSE, true),
        )
        .await
        .unwrap();
    let raw_src = std::fs::OpenOptions::new()
        .write(true)
        .open(&src_path)
        .unwrap();
    raw_src.seek_write(&vec![b'A'; 4096], 4096).unwrap();
    raw_src.seek_write(&vec![b'B'; 4096], 12 * 1024).unwrap();
    raw_src.set_len(20 * 1024).unwrap();
    std::fs::write(&dst_path, vec![0xaa; 22 * 1024]).unwrap();

    let src = direct
        .open_options()
        .read(true)
        .open(typed(&src_path))
        .await
        .unwrap();
    let dst = direct
        .open_options()
        .read(true)
        .write(true)
        .open(typed(&dst_path))
        .await
        .unwrap();
    let result = src
        .copy_data(
            &dst,
            0,
            CopyDest::At(4096),
            Some(20 * 1024),
            CopyMode::Never,
        )
        .await
        .unwrap();
    assert_eq!(result.count, 20 * 1024);
    src.close().await.unwrap();
    dst.close().await.unwrap();

    let copied = std::fs::read(&dst_path).unwrap();
    assert_eq!(&copied[..4096], &vec![0xaa; 4096]);
    assert_eq!(&copied[4096..8192], &vec![0; 4096]);
    assert_eq!(&copied[8192..12_288], &vec![b'A'; 4096]);
    assert_eq!(&copied[12_288..16_384], &vec![0; 4096]);
    assert_eq!(&copied[16_384..20_480], &vec![b'B'; 4096]);
    assert_eq!(&copied[20_480..24_576], &vec![0; 4096]);
    assert_eq!(copied.len(), 24_576);
    let attrs = direct
        .metadata(typed(&dst_path))
        .await
        .unwrap()
        .win_attrs()
        .unwrap();
    assert_eq!(attrs & 0x200, 0);
}

#[tokio::test]
async fn copy_data_is_bounded_and_callers_can_loop_short_results() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("big-src.bin");
    let dst_path = dir.path().join("big-dst.bin");
    // Deliberately not a multiple of the chunk size, so the final round is a
    // partial one.
    let content: Vec<u8> = (0..2_500_123u32).map(|i| (i % 251) as u8).collect();
    let src = open_rw(&direct, &src_path, &content).await;
    let dst = open_rw(&direct, &dst_path, b"").await;

    let first = src
        .copy_data(&dst, 0, CopyDest::At(0), None, CopyMode::Auto)
        .await
        .unwrap();
    assert_eq!(first.count, 2 * 1024 * 1024);
    let second = src
        .copy_data(
            &dst,
            first.count,
            CopyDest::At(first.count),
            None,
            CopyMode::Auto,
        )
        .await
        .unwrap();
    assert_eq!(first.count + second.count, content.len() as u64);
    assert_eq!(
        src.copy_data(
            &dst,
            content.len() as u64,
            CopyDest::At(content.len() as u64),
            None,
            CopyMode::Auto,
        )
        .await
        .unwrap()
        .count,
        0
    );

    src.close().await.unwrap();
    dst.close().await.unwrap();
    assert_eq!(std::fs::read(&dst_path).unwrap(), content);
}

#[tokio::test]
async fn copy_data_rejects_overlapping_regions_of_one_file() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let path = dir.path().join("overlap.bin");
    let first = open_rw(&direct, &path, b"0123456789").await;
    let second = direct
        .open_options()
        .read(true)
        .write(true)
        .open(typed(&path))
        .await
        .unwrap();

    // Two distinct handles, one file: the identity check is what catches this,
    // not the handles being the same object.
    let error = first
        .copy_data(&second, 0, CopyDest::At(4), Some(8), CopyMode::Auto)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);

    // Disjoint regions of the same file are fine.
    assert_eq!(
        first
            .copy_data(&second, 0, CopyDest::At(6), Some(3), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        3
    );

    first.close().await.unwrap();
    second.close().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"0123450129");
}

#[tokio::test]
async fn copy_data_refuses_to_promise_block_sharing() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("mode-src.bin");
    let dst_path = dir.path().join("mode-dst.bin");
    let src = open_rw(&direct, &src_path, b"0123456789").await;
    let dst = open_rw(&direct, &dst_path, b"").await;

    // Required cloning is conditional on the filesystem. A supported clone
    // copies the full range; an incapable pair reports `Unsupported`.
    match src
        .copy_data(&dst, 0, CopyDest::At(0), Some(10), CopyMode::Require)
        .await
    {
        Ok(result) => assert_eq!(result.count, 10),
        Err(error) => assert_eq!(error.kind(), ErrorKind::Unsupported),
    }
    dst.set_size(0).await.unwrap();
    // Requiring a clone at EOF is an empty success, and append cannot promise
    // block sharing even when the filesystem otherwise supports it.
    assert_eq!(
        src.copy_data(&dst, 10, CopyDest::At(0), None, CopyMode::Require)
            .await
            .unwrap()
            .count,
        0
    );
    let append = direct
        .open_options()
        .append(true)
        .open(typed(&dst_path))
        .await
        .unwrap();
    assert_eq!(
        src.copy_data(&append, 0, CopyDest::Append, Some(4), CopyMode::Require)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    for mode in [CopyMode::Auto, CopyMode::Never] {
        assert_eq!(
            src.copy_data(&dst, 0, CopyDest::At(0), Some(4), mode)
                .await
                .unwrap()
                .count,
            4
        );
    }

    src.close().await.unwrap();
    dst.close().await.unwrap();
    append.close().await.unwrap();
    assert_eq!(std::fs::read(&dst_path).unwrap(), b"0123");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn direct_well_known_data_and_state_dirs() {
    use crate::path::WellKnownPath::{DataDir, DataLocalDir, StateDir};
    let direct = Vfs::direct().unwrap();
    for (key, variable, fallback) in [
        (DataDir, "XDG_DATA_HOME", ".local/share"),
        (DataLocalDir, "XDG_DATA_HOME", ".local/share"),
        (StateDir, "XDG_STATE_HOME", ".local/state"),
    ] {
        for (value, expected) in [
            (None, format!("/tmp/test-home/{fallback}")),
            (Some("/tmp/override"), "/tmp/override".into()),
        ] {
            let env = HashMap::from([
                ("HOME".into(), Some("/tmp/test-home".into())),
                (variable.into(), value.map(str::to_owned)),
            ]);
            for app in [None, Some("My App")] {
                let path = direct
                    .well_known_path(key, app, &env)
                    .await
                    .unwrap()
                    .unwrap();
                let expected = match app {
                    Some(app) => format!("{expected}/{app}"),
                    None => expected.clone(),
                };
                assert_eq!(path.as_str(), expected);
            }
        }
        for value in ["relative", ""] {
            let env = HashMap::from([(variable.into(), Some(value.into()))]);
            let error = direct.well_known_path(key, None, &env).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
#[tokio::test]
async fn direct_well_known_data_dirs_and_absent_state() {
    use crate::path::WellKnownPath::{DataDir, DataLocalDir, StateDir};
    let direct = Vfs::direct().unwrap();
    let env = HashMap::from([
        ("HOME".into(), Some("/tmp/test-home".into())),
        ("XDG_DATA_HOME".into(), Some("relative-ignored".into())),
        ("XDG_STATE_HOME".into(), Some("relative-ignored".into())),
    ]);
    for key in [DataDir, DataLocalDir] {
        let base = direct
            .well_known_path(key, None, &env)
            .await
            .unwrap()
            .unwrap();
        let scoped = direct
            .well_known_path(key, Some("My App"), &env)
            .await
            .unwrap()
            .unwrap();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(base.as_str(), "/tmp/test-home/Library/Application Support");
            assert_eq!(scoped, base.join("My App"));
        }
        #[cfg(windows)]
        {
            assert!(base.is_absolute());
            assert_eq!(scoped, base.join("My App").join("data"));
        }
    }
    for app in [None, Some("My App")] {
        assert_eq!(
            direct.well_known_path(StateDir, app, &env).await.unwrap(),
            None
        );
    }
}
