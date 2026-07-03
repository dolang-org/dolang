#![deny(warnings)]

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io;

use dolang_shell_vfs::{Child, Command, Direct, FileType, OpenOptions, Vfs};
use tempfile::tempdir;

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
        .open(&path)
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

    direct.symlink(&target, &link).await.unwrap();

    let metadata = direct.symlink_metadata(&link).await.unwrap();
    assert_eq!(metadata.file_type, FileType::Symlink);
    assert_eq!(direct.read_link(&link).await.unwrap(), target);
}

#[tokio::test]
async fn direct_hard_link_round_trip() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    tokio::fs::write(&target, "hello").await.unwrap();

    direct.hard_link(&target, &link).await.unwrap();

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

    let metadata = direct.metadata(&path).await.unwrap();

    assert_ne!(metadata.win_attrs, 0);
    assert_ne!(metadata.win_attrs & 0x0000_0001, 0);
    assert_eq!(metadata.attrs().readonly, Some(true));
}

#[cfg(unix)]
#[tokio::test]
async fn direct_set_times_rejects_created_timestamp() {
    let direct = Direct::default();
    let dir = tempdir().unwrap();
    let path = dir.path().join("timestamps.txt");
    tokio::fs::write(&path, "hello").await.unwrap();

    let err = direct
        .set_times(&path, None, None, Some((1, 0)))
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
        .set_attrs(
            &path,
            dolang_shell_vfs::Attrs {
                readonly: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let attrs = direct.attrs(&path, true).await.unwrap();
    assert_eq!(attrs.readonly, Some(true));

    direct
        .set_attrs(
            &path,
            dolang_shell_vfs::Attrs {
                readonly: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let attrs = direct.attrs(&path, true).await.unwrap();
    assert_eq!(attrs.readonly, Some(false));

    if is_wine() {
        return;
    }

    direct
        .set_attrs(
            &path,
            dolang_shell_vfs::Attrs {
                compressed: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let attrs = direct.attrs(&path, true).await.unwrap();
    assert_eq!(attrs.compressed, Some(true));

    direct
        .set_attrs(
            &path,
            dolang_shell_vfs::Attrs {
                compressed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let attrs = direct.attrs(&path, true).await.unwrap();
    assert_eq!(attrs.compressed, Some(false));
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

    let file = direct.open_options().read(true).open(&path).await.unwrap();
    let streams = direct.file_streams(&file).await.unwrap();
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

    let attrs = match direct.attrs(&path, true).await {
        Ok(attrs) => attrs,
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
    assert!(attrs.unix_flags.is_some());
    assert!(attrs.immutable.is_some());
    assert!(attrs.append_only.is_some());
    assert!(attrs.no_dump.is_some());
    assert!(attrs.no_atime.is_some());

    if let Err(err) = direct
        .set_attrs(
            &path,
            dolang_shell_vfs::Attrs {
                no_dump: Some(true),
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
        panic!("set_attrs failed: {err}");
    }

    let attrs = direct.attrs(&path, true).await.unwrap();
    assert_eq!(attrs.no_dump, Some(true));

    direct
        .set_attrs(
            &path,
            dolang_shell_vfs::Attrs {
                no_dump: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let attrs = direct.attrs(&path, true).await.unwrap();
    assert_eq!(attrs.no_dump, Some(false));
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

    direct.copy(&src, &copied, true).await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(copied.join("nested").join("file.txt"))
            .await
            .unwrap(),
        "hello"
    );

    direct.move_(&copied, &moved, true).await.unwrap();
    assert!(!copied.exists());

    let matches = direct
        .glob("**/*.txt", dir.path(), false, None)
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

    direct.remove_dir(&root, true, true).await.unwrap();

    assert!(root.exists());
    assert!(root.join("keep").exists());
    assert!(!root.join("prune").exists());
}

#[tokio::test]
async fn direct_basic_spawn() {
    let direct = Direct::default();
    let (program, args) = successful_command();
    let mut command = direct.command(program);
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn direct_spawn_failure() {
    let direct = Direct::default();
    let result = direct.command("nonexistent_command_12345").spawn().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn direct_exit_code() {
    let direct = Direct::default();
    let (program, args) = failing_exit_command();
    let mut command = direct.command(program);
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
    let mut command = direct.command(program);
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
        .well_known_path(dolang_shell_vfs::WellKnownPath::HomeDir, &env)
        .await
        .unwrap();

    assert_eq!(path, std::path::Path::new("/tmp/test-home"));
}

#[cfg(unix)]
#[tokio::test]
async fn direct_well_known_home_dir_rejects_relative_home_override() {
    let direct = Direct::default();
    let env = HashMap::from([(String::from("HOME"), Some(String::from("relative-home")))]);

    let err = direct
        .well_known_path(dolang_shell_vfs::WellKnownPath::HomeDir, &env)
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
        .well_known_path(dolang_shell_vfs::WellKnownPath::CacheDir, &env)
        .await
        .unwrap();

    assert_eq!(path, std::path::Path::new("/tmp/test-cache"));
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
        .well_known_path(dolang_shell_vfs::WellKnownPath::CacheDir, &env)
        .await
        .unwrap();

    assert_eq!(path, std::path::Path::new("/tmp/test-home/.cache"));
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
        .well_known_path(dolang_shell_vfs::WellKnownPath::CacheDir, &env)
        .await
        .unwrap();

    assert_eq!(path, std::path::Path::new("/tmp/test-home/Library/Caches"));
}
