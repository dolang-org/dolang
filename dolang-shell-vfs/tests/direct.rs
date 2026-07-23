#![deny(warnings)]

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io;
use std::path::Path;

#[cfg(any(windows, target_os = "linux"))]
use dolang_shell_vfs::{AttrFlags, AttrsPatch};
use dolang_shell_vfs::{
    Child, Command, Direct, FileHandle, FileLockBehavior, FileLockMode, FileLockRange,
    FileLockRequest, FileType, MetadataPatch, OpenOptions, Utf8TypedPath, Utf8UnixPath,
    Utf8WindowsPath, Vfs,
};
#[cfg(windows)]
use dolang_shell_vfs::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
};
use tempfile::tempdir;

fn typed(path: &Path) -> Utf8TypedPath<'_> {
    let path = path.to_str().unwrap();
    if cfg!(windows) {
        Utf8TypedPath::Windows(Utf8WindowsPath::new(path))
    } else {
        Utf8TypedPath::Unix(Utf8UnixPath::new(path))
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn attr_patch(flag: AttrFlags, value: bool) -> AttrsPatch {
    let mut patch = AttrsPatch::default();
    patch.update(flag, Some(value));
    patch
}

fn typed_str(path: &str) -> Utf8TypedPath<'_> {
    if cfg!(windows) {
        Utf8TypedPath::Windows(Utf8WindowsPath::new(path))
    } else {
        Utf8TypedPath::Unix(Utf8UnixPath::new(path))
    }
}

fn lock_request(
    start: u64,
    end: Option<u64>,
    mode: FileLockMode,
    behavior: FileLockBehavior,
) -> FileLockRequest {
    FileLockRequest {
        range: FileLockRange { start, end },
        mode,
        behavior,
    }
}

async fn open_lock_file(direct: &Direct, path: &Path) -> dolang_shell_vfs::DirectFile {
    direct
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(typed(path))
        .await
        .unwrap()
}

#[tokio::test]
async fn byte_range_locks_contend_and_release() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("locks");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;

    let mut exclusive = first
        .lock(lock_request(
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
            .lock(lock_request(
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
        .lock(lock_request(
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
            .lock(lock_request(
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

#[tokio::test]
async fn shared_locks_and_same_handle_overlap_rules() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("shared-locks");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;
    let third = open_lock_file(&direct, &path).await;

    let _first_shared = first
        .lock(lock_request(
            0,
            None,
            FileLockMode::Shared,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap()
        .unwrap();
    let _second_shared = second
        .lock(lock_request(
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
            .lock(lock_request(
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
            .lock(lock_request(
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
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty-lock");
    let first = open_lock_file(&direct, &path).await;

    let error = first
        .lock(lock_request(
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
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty-lock");
    let first = open_lock_file(&direct, &path).await;
    let second = open_lock_file(&direct, &path).await;

    let error = first
        .lock(lock_request(
            0,
            Some(0),
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

    let _empty = first
        .lock(lock_request(
            4,
            Some(4),
            FileLockMode::Exclusive,
            FileLockBehavior::Blocking,
        ))
        .await
        .unwrap()
        .unwrap();

    let mut same = first
        .lock(lock_request(
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
            .lock(lock_request(
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
        .lock(lock_request(
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
        .lock(lock_request(
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
        .lock(lock_request(
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
        .lock(lock_request(
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
        .lock(lock_request(
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
            .lock(lock_request(
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

#[cfg(windows)]
fn is_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::core::w;

    const WINE_GET_VERSION: &[u8] = b"wine_get_version\0";

    let ntdll = unsafe { GetModuleHandleW(w!("ntdll.dll")) };
    !ntdll.is_null() && unsafe { GetProcAddress(ntdll, WINE_GET_VERSION.as_ptr()) }.is_some()
}

#[tokio::test]
async fn direct_open_options_round_trip() {
    let direct = Direct::default();
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

#[tokio::test]
async fn direct_symlink_metadata_and_read_link() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    tokio::fs::write(&target, "hello").await.unwrap();

    direct
        .symlink(typed_str(""), typed(&target), typed(&link))
        .await
        .unwrap();

    let metadata = direct.symlink_metadata(typed(&link)).await.unwrap();
    assert_eq!(metadata.file_type, FileType::Symlink);
    assert_eq!(
        direct.read_link(typed(&link)).await.unwrap().as_str(),
        target.to_str().unwrap()
    );
}

#[tokio::test]
async fn direct_hard_link_round_trip() {
    let direct = Direct::default();
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
    let direct = Direct::default();
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
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("fsmeta.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let metadata = direct.fs_metadata(typed(&path), true).await.unwrap();
    assert!(metadata.capacity > 0);
    assert!(metadata.free > 0);
    assert!(metadata.available > 0);
    assert!(metadata.block_size > 0 || cfg!(windows));
}

#[tokio::test]
async fn direct_file_fs_metadata_basic() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("fsmeta-file.txt");
    tokio::fs::write(&path, "hello").await.unwrap();
    let mut file = direct
        .open_options()
        .read(true)
        .open(typed(&path))
        .await
        .unwrap();

    let metadata = file.fs_metadata().await.unwrap();
    assert!(metadata.capacity > 0);
    assert!(metadata.free > 0);
    assert!(metadata.available > 0);
}

#[cfg(windows)]
#[tokio::test]
async fn direct_security_descriptor_path_and_file() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("security.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let mask = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let descriptor = direct.sec_desc(typed(&path), mask, true).await.unwrap();
    assert_eq!(descriptor.mask(), mask);
    assert!(descriptor.owner_loaded());
    assert!(descriptor.owner().is_some());
    assert!(descriptor.group_loaded());
    assert!(descriptor.dacl_loaded());

    let dacl = direct
        .sec_desc(typed(&path), DACL_SECURITY_INFORMATION, true)
        .await
        .unwrap();
    match direct.set_sec_desc(typed(&path), &dacl, true).await {
        Ok(()) => {
            let round_trip = direct
                .sec_desc(typed(&path), DACL_SECURITY_INFORMATION, true)
                .await
                .unwrap();
            assert_eq!(round_trip.dacl(), dacl.dacl());
        }
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
    }

    let mut file = direct
        .open_options()
        .read(true)
        .open(typed(&path))
        .await
        .unwrap();
    let file_descriptor = file.sec_desc(OWNER_SECURITY_INFORMATION).await.unwrap();
    assert!(file_descriptor.owner().is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_security_descriptors_are_unsupported() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("security.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let error = direct.sec_desc(typed(&path), 0, true).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(unix)]
#[tokio::test]
async fn direct_set_metadata_rejects_created_timestamp() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("timestamps.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let err = direct
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                created: Some(1_000_000_000),
                ..MetadataPatch::default()
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(windows)]
#[tokio::test]
async fn direct_windows_attrs() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("attrs.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    direct
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                attrs: attr_patch(AttrFlags::READONLY, true),
                ..Default::default()
            },
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
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                attrs: attr_patch(AttrFlags::READONLY, false),
                ..Default::default()
            },
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

    if is_wine() {
        return;
    }

    direct
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                attrs: attr_patch(AttrFlags::COMPRESSED, true),
                ..Default::default()
            },
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
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                attrs: attr_patch(AttrFlags::COMPRESSED, false),
                ..Default::default()
            },
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
}

#[cfg(windows)]
#[tokio::test]
async fn direct_windows_streams() {
    if is_wine() {
        return;
    }

    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("streams.txt");
    let stream_path = dir.path().join("streams.txt:zone");
    tokio::fs::write(&path, "base").await.unwrap();
    tokio::fs::write(&stream_path, "stream").await.unwrap();

    let mut file = direct
        .open_options()
        .read(true)
        .open(typed(&path))
        .await
        .unwrap();
    let streams = file.streams().await.unwrap();
    assert!(streams.iter().any(|entry| {
        entry.name.is_empty()
            && entry.r#type == "DATA"
            && entry.size == 4
            && entry.alloc_size >= entry.size
    }));
    assert!(streams.iter().any(|entry| {
        entry.name == "zone"
            && entry.r#type == "DATA"
            && entry.size == 6
            && entry.alloc_size >= entry.size
    }));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_linux_attrs() {
    let direct = Direct::default();
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
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                attrs: attr_patch(AttrFlags::NO_DUMP, true),
                ..Default::default()
            },
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
        panic!("set_metadata failed: {err}");
    }

    let attrs = direct
        .metadata(typed(&path))
        .await
        .unwrap()
        .linux_attrs()
        .unwrap();
    assert_ne!(attrs & 0x40, 0);

    direct
        .set_metadata(
            &[typed(&path).to_path_buf()],
            MetadataPatch {
                attrs: attr_patch(AttrFlags::NO_DUMP, false),
                ..Default::default()
            },
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

    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("metadata.sock");
    let _listener = UnixListener::bind(&path).unwrap();

    let metadata = direct.metadata(typed(&path)).await.unwrap();
    assert_eq!(metadata.file_type, FileType::Socket);
    assert_eq!(metadata.linux_attrs(), None);
}

#[tokio::test]
async fn direct_copy_move_and_glob() {
    let direct = Direct::default();
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
    let direct = Direct::default();
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
    let direct = Direct::default();
    let (program, args) = successful_command();
    let mut command = direct.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn direct_spawn_failure() {
    let direct = Direct::default();
    let result = direct
        .command(typed_str("nonexistent_command_12345"))
        .spawn()
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn direct_exit_code() {
    let direct = Direct::default();
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
    let direct = Direct::default();
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
    let direct = Direct::default();
    let env = HashMap::from([(String::from("HOME"), Some(String::from("/tmp/test-home")))]);

    let path = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::HomeDir, None, &env)
        .await
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-home");
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_temp_dir_prefers_tmpdir_override() {
    let direct = Direct::default();
    let env = HashMap::from([(String::from("TMPDIR"), Some(String::from("/tmp/test-temp")))]);

    let path = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::TempDir, None, &env)
        .await
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-temp");
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_temp_dir_falls_back_to_tmp() {
    let direct = Direct::default();
    let env = HashMap::from([(String::from("TMPDIR"), None)]);

    let path = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::TempDir, None, &env)
        .await
        .unwrap();

    assert_eq!(path.as_str(), "/tmp");
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_home_dir_rejects_relative_home_override() {
    let direct = Direct::default();
    let env = HashMap::from([(String::from("HOME"), Some(String::from("relative-home")))]);

    let err = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::HomeDir, None, &env)
        .await
        .unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn direct_well_known_cache_dir_prefers_xdg_override() {
    let direct = Direct::default();
    let env = HashMap::from([
        (
            String::from("XDG_CACHE_HOME"),
            Some(String::from("/tmp/test-cache")),
        ),
        (String::from("HOME"), Some(String::from("/tmp/test-home"))),
    ]);

    let path = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::CacheDir, None, &env)
        .await
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-cache");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn direct_well_known_cache_dir_falls_back_to_home() {
    let direct = Direct::default();
    let env = HashMap::from([
        (String::from("HOME"), Some(String::from("/tmp/test-home"))),
        (String::from("XDG_CACHE_HOME"), None),
    ]);

    let path = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::CacheDir, None, &env)
        .await
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-home/.cache");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn direct_well_known_cache_dir_uses_macos_convention() {
    let direct = Direct::default();
    let env = HashMap::from([
        (String::from("HOME"), Some(String::from("/tmp/test-home"))),
        (
            String::from("XDG_CACHE_HOME"),
            Some(String::from("/tmp/test-cache")),
        ),
    ]);

    let path = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::CacheDir, None, &env)
        .await
        .unwrap();

    assert_eq!(path.as_str(), "/tmp/test-home/Library/Caches");
}
