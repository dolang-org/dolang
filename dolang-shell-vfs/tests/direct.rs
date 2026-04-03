#![deny(warnings)]

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

#[tokio::test]
async fn direct_open_options_round_trip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");

    let mut options = Direct.open_options();
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
    let dir = tempdir().unwrap();
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    tokio::fs::write(&target, "hello").await.unwrap();

    Direct.symlink(&target, &link).await.unwrap();

    let metadata = Direct.symlink_metadata(&link).await.unwrap();
    assert_eq!(metadata.file_type, FileType::Symlink);
    assert_eq!(Direct.read_link(&link).await.unwrap(), target);
}

#[tokio::test]
async fn direct_copy_move_and_glob() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let nested = src.join("nested");
    let copied = dir.path().join("copied");
    let moved = dir.path().join("moved");

    tokio::fs::create_dir_all(&nested).await.unwrap();
    tokio::fs::write(nested.join("file.txt"), "hello")
        .await
        .unwrap();

    Direct.copy(&src, &copied, true).await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(copied.join("nested").join("file.txt"))
            .await
            .unwrap(),
        "hello"
    );

    Direct.move_(&copied, &moved, true).await.unwrap();
    assert!(!copied.exists());

    let matches = Direct
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

    Direct.remove_dir(&root, true, true).await.unwrap();

    assert!(root.exists());
    assert!(root.join("keep").exists());
    assert!(!root.join("prune").exists());
}

#[tokio::test]
async fn direct_basic_spawn() {
    let (program, args) = successful_command();
    let mut command = Direct.command(program);
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn direct_spawn_failure() {
    let result = Direct.command("nonexistent_command_12345").spawn().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn direct_exit_code() {
    let (program, args) = failing_exit_command();
    let mut command = Direct.command(program);
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(!status.success());
    assert_eq!(status.code(), Some(42));
}

#[tokio::test]
async fn direct_env_vars() {
    let (program, args) = env_forwarding_command();
    let mut command = Direct.command(program);
    command.arg(args[0]).arg(args[1]).env("TEST_VAR", "value");
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
