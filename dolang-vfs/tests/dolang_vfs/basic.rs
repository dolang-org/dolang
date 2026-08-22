#![cfg(unix)]
use dolang_rpc::auth::AuthKey;
use dolang_vfs::{
    Vfs,
    file::AccessFlags,
    metadata::{FileType, MetadataPatch, Mode},
    security::OwnershipIdentity,
    target::TargetInfo,
};
#[cfg(not(target_os = "macos"))]
use nix::unistd::getgroups;
use nix::unistd::{Group, User, getegid, geteuid, getgid, getuid};
use std::{os::fd::OwnedFd, path::Path};
use typed_path::{Utf8TypedPath, Utf8UnixPath};

use tempfile::tempdir;
use tokio::task::JoinHandle;

fn typed(path: &Path) -> Utf8TypedPath<'_> {
    Utf8TypedPath::Unix(Utf8UnixPath::new(path.to_str().unwrap()))
}

fn typed_str(path: &str) -> Utf8TypedPath<'_> {
    Utf8TypedPath::Unix(Utf8UnixPath::new(path))
}

async fn start_server(socket_path: &Path) -> JoinHandle<()> {
    let path = socket_path.to_path_buf();
    let server = dolang_vfs::server::Server::bind(&path).await.unwrap();
    tokio::spawn(async move {
        let _ = server.accept().await;
    })
}

async fn connect_client(socket_path: &Path) -> Vfs {
    Vfs::connect(socket_path).await.unwrap()
}

async fn stop_server(client: Vfs, server: JoinHandle<()>) {
    client.stop().await.unwrap();
    client.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn direct_query_reports_host_target() {
    let direct = Vfs::direct().unwrap();
    assert!(direct.env().next().is_some());
    assert!(direct.cwd().is_absolute());
    assert!(direct.current_exe().is_absolute());
    assert_eq!(direct.target(), &TargetInfo::current());
    let Some(security) = direct.security().unix() else {
        panic!("Unix query returned Windows security information");
    };
    assert_eq!(security.uid(), getuid().as_raw());
    assert_eq!(security.gid(), getgid().as_raw());
    assert_eq!(security.effective_uid(), geteuid().as_raw());
    assert_eq!(security.effective_gid(), getegid().as_raw());
    #[cfg(not(target_os = "macos"))]
    assert_eq!(
        security.group_ids(),
        getgroups()
            .unwrap()
            .into_iter()
            .map(|gid| gid.as_raw())
            .collect::<Vec<_>>()
    );
    #[cfg(target_os = "macos")]
    assert!(security.group_ids().contains(&getegid().as_raw()));
}

#[tokio::test]
async fn direct_resolves_unix_user_and_group_names() {
    let vfs = Vfs::direct().unwrap();
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let user = vfs.user_name(uid).await.unwrap();
    let group = vfs.group_name(gid).await.unwrap();
    assert_eq!(vfs.user_id(&user).await.unwrap(), uid);
    assert_eq!(vfs.group_id(&group).await.unwrap(), gid);

    assert_eq!(
        vfs.user_id("dolang-user-that-does-not-exist")
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        vfs.group_id("dolang-group-that-does-not-exist")
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[tokio::test]
async fn basic_spawn() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let mut command = client.command(typed_str("echo"));
    command.arg("hello");
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();

    assert!(status.success());
    assert_eq!(status.code(), Some(0));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn client_from_owned_fd() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server = dolang_vfs::server::Server::bind(&socket_path)
        .await
        .unwrap();
    let accept_task = tokio::spawn(async move {
        let _ = server.accept().await;
    });

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let fd: OwnedFd = stream.into_std().unwrap().into();
    let client = Vfs::from_owned_fd(fd).await.unwrap();

    let mut child = client.command(typed_str("true")).spawn().await.unwrap();
    let status = child.wait().await.unwrap();

    assert!(status.success());

    accept_task.abort();
    let _ = accept_task.await;
}

#[tokio::test]
async fn spawn_failure() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let result = client
        .command(typed_str("nonexistent_command_12345"))
        .spawn()
        .await;

    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn exit_code() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let mut command = client.command(typed_str("sh"));
    command.arg("-c").arg("exit 42");
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();

    assert!(!status.success());
    assert_eq!(status.code(), Some(42));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn env_vars() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let mut command = client.command(typed_str("sh"));
    command
        .arg("-c")
        .arg("echo $TEST_VAR")
        .env("TEST_VAR", "value");
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();

    assert!(status.success());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn copy_directory_requires_all() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let dst = dir.path().join("dst");

    let client = connect_client(&socket_path).await;
    let err = client
        .copy(typed(&src), typed(&dst), false)
        .await
        .unwrap_err();

    assert!(err.kind() == std::io::ErrorKind::IsADirectory || err.raw_os_error().is_some());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn copy_directory_all() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let src = dir.path().join("src");
    let nested = src.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("file.txt"), "hello").unwrap();
    let dst = dir.path().join("dst");

    let client = connect_client(&socket_path).await;
    client.copy(typed(&src), typed(&dst), true).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(dst.join("nested").join("file.txt")).unwrap(),
        "hello"
    );

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn move_directory_all() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let src = dir.path().join("src");
    let nested = src.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("file.txt"), "hello").unwrap();
    let dst = dir.path().join("dst");

    let client = connect_client(&socket_path).await;
    client.move_(typed(&src), typed(&dst), true).await.unwrap();

    assert!(!src.exists());
    assert_eq!(
        std::fs::read_to_string(dst.join("nested").join("file.txt")).unwrap(),
        "hello"
    );

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn remove_dir_all_removes_empty_dirs() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("a").join("b")).unwrap();

    let client = connect_client(&socket_path).await;
    client.remove_dir(typed(&root), true, false).await.unwrap();

    assert!(!root.exists());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn remove_dir_rejects_files_without_ignore() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a").join("file.txt"), "hello").unwrap();

    let client = connect_client(&socket_path).await;
    let err = client
        .remove_dir(typed(&root), true, false)
        .await
        .unwrap_err();

    assert!(err.kind() == std::io::ErrorKind::DirectoryNotEmpty || err.raw_os_error().is_some());
    assert!(root.exists());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn remove_dir_ignore_prunes_empty_branches() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("keep").join("child")).unwrap();
    std::fs::create_dir_all(root.join("prune").join("leaf")).unwrap();
    std::fs::write(root.join("keep").join("file.txt"), "hello").unwrap();

    let client = connect_client(&socket_path).await;
    client.remove_dir(typed(&root), true, true).await.unwrap();

    assert!(root.exists());
    assert!(root.join("keep").exists());
    assert!(!root.join("prune").exists());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn fd_passing() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let file = tempfile::NamedTempFile::new().unwrap();

    let client = connect_client(&socket_path).await;
    let output = client
        .open_options()
        .write(true)
        .open(file.path().to_str().unwrap().into())
        .await
        .unwrap();
    let mut command = client.command(typed_str("echo"));
    command
        .arg("hello_world")
        .stdout(crate::support::stdio_send(output).await)
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();

    assert!(status.success());

    drop(child);
    drop(client);

    let content = std::fs::read_to_string(file.path()).unwrap();
    assert_eq!(content.trim(), "hello_world");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn file_open_read() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello_world").unwrap();

    let client = connect_client(&socket_path).await;
    let file = client
        .open_options()
        .read(true)
        .open(test_file.to_str().unwrap().into())
        .await
        .unwrap();

    let mut contents = String::new();
    let mut std_file = file.try_into_std().await.unwrap();
    std::io::Read::read_to_string(&mut std_file, &mut contents).unwrap();
    assert_eq!(contents, "hello_world");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn file_open_write() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "initial").unwrap();

    let client = connect_client(&socket_path).await;
    let file = client
        .open_options()
        .write(true)
        .truncate(true)
        .open(test_file.to_str().unwrap().into())
        .await
        .unwrap();

    let mut std_file = file.try_into_std().await.unwrap();
    std::io::Write::write_all(&mut std_file, b"replaced").unwrap();
    drop(std_file);

    let contents = std::fs::read_to_string(&test_file).unwrap();
    assert_eq!(contents, "replaced");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn file_create() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("new_file.txt");
    assert!(!test_file.exists());

    let client = connect_client(&socket_path).await;
    let file = client
        .open_options()
        .write(true)
        .create(true)
        .open(test_file.to_str().unwrap().into())
        .await
        .unwrap();

    assert!(test_file.exists());

    let mut std_file = file.try_into_std().await.unwrap();
    std::io::Write::write_all(&mut std_file, b"created").unwrap();
    drop(std_file);

    let contents = std::fs::read_to_string(&test_file).unwrap();
    assert_eq!(contents, "created");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn file_create_new() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("new_file.txt");
    assert!(!test_file.exists());

    let client = connect_client(&socket_path).await;

    // First open with create_new should succeed
    let file = client
        .open_options()
        .write(true)
        .create_new(true)
        .open(test_file.to_str().unwrap().into())
        .await
        .unwrap();
    drop(file);

    assert!(test_file.exists());

    // Second open with create_new should fail (file exists)
    let result = client
        .open_options()
        .write(true)
        .create_new(true)
        .open(test_file.to_str().unwrap().into())
        .await;

    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn unix_vfs_connects_to_another_server() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("outer.sock");
    let inner_path = dir.path().join("inner.sock");
    let outer_task = start_server(&socket_path).await;
    let inner_task = start_server(&inner_path).await;

    let client = connect_client(&socket_path).await;
    let inner = client.unix_socket(typed(&inner_path), None).await.unwrap();
    assert_eq!(inner.target(), &TargetInfo::current());

    inner.stop().await.unwrap();
    drop(inner);
    inner_task.await.unwrap();
    stop_server(client, outer_task).await;
}

#[tokio::test]
async fn unix_vfs_connect_missing() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("outer.sock");
    let missing_path = dir.path().join("missing.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let result = client.unix_socket(typed(&missing_path), None).await;
    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn file_open_error() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("nonexistent.txt");

    let client = connect_client(&socket_path).await;
    let result = client
        .open_options()
        .read(true)
        .open(test_file.to_str().unwrap().into())
        .await;

    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn file_metadata() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello_world").unwrap();

    let client = connect_client(&socket_path).await;
    let metadata = client.metadata(typed(&test_file)).await.unwrap();

    assert_eq!(metadata.len(), 11);
    assert_eq!(metadata.file_type(), FileType::File);
    let unix = metadata.unix().unwrap();
    assert!(!unix.mode().is_empty());
    assert!(unix.ino() != 0);
    assert!(unix.nlink() > 0);

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn dir_metadata() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let subdir = dir.path().join("subdir");
    std::fs::create_dir(&subdir).unwrap();

    let client = connect_client(&socket_path).await;
    let metadata = client.metadata(typed(&subdir)).await.unwrap();

    assert_eq!(metadata.file_type(), FileType::Dir);
    let unix = metadata.unix().unwrap();
    assert!(!unix.mode().is_empty());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn fs_metadata_basic() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello_world").unwrap();

    let client = connect_client(&socket_path).await;
    let metadata = client.fs_metadata(typed(&test_file), true).await.unwrap();

    assert!(metadata.capacity() > 0);
    assert!(metadata.free() > 0);
    assert!(metadata.available() > 0);

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn hard_link_round_trip() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");
    std::fs::write(&target, "hello_world").unwrap();

    let client = connect_client(&socket_path).await;
    client
        .hard_link(typed(&target), typed(&link))
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&link).unwrap(), "hello_world");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn metadata_nonexistent() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let result = client.metadata(typed_str("nonexistent.txt")).await;

    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn set_metadata_by_numeric_id() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello_world").unwrap();

    let client = connect_client(&socket_path).await;
    client
        .set_metadata(
            &[typed(&test_file).to_path_buf()],
            MetadataPatch::new()
                .with_mode(Mode::from_bits_retain(0o600))
                .with_user(OwnershipIdentity::Id(getuid().as_raw()))
                .with_group(OwnershipIdentity::Id(getgid().as_raw())),
        )
        .await
        .unwrap();

    let metadata = client.metadata(typed(&test_file)).await.unwrap();
    let unix = metadata.unix().unwrap();
    assert_eq!(unix.uid(), getuid().as_raw());
    assert_eq!(unix.gid(), getgid().as_raw());
    assert_eq!(unix.mode().bits() & 0o777, 0o600);

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn set_metadata_by_name() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello_world").unwrap();

    let user = User::from_uid(getuid()).unwrap().unwrap();
    let group = Group::from_gid(getgid()).unwrap().unwrap();

    let client = connect_client(&socket_path).await;
    client
        .set_metadata(
            &[typed(&test_file).to_path_buf()],
            MetadataPatch::new()
                .with_user(OwnershipIdentity::Name(user.name))
                .with_group(OwnershipIdentity::Name(group.name)),
        )
        .await
        .unwrap();

    let metadata = client.metadata(typed(&test_file)).await.unwrap();
    let unix = metadata.unix().unwrap();
    assert_eq!(unix.uid(), getuid().as_raw());
    assert_eq!(unix.gid(), getgid().as_raw());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn set_metadata_follow_false_on_dangling_symlink() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let link_path = dir.path().join("dangling-link");
    std::os::unix::fs::symlink("missing-target", &link_path).unwrap();

    let client = connect_client(&socket_path).await;
    client
        .set_metadata(
            &[typed(&link_path).to_path_buf()],
            MetadataPatch::new()
                .with_group(OwnershipIdentity::Id(getgid().as_raw()))
                .with_follow_links(false),
        )
        .await
        .unwrap();

    let result = client
        .set_metadata(
            &[typed(&link_path).to_path_buf()],
            MetadataPatch::new().with_group(OwnershipIdentity::Id(getgid().as_raw())),
        )
        .await;
    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn set_metadata_unknown_user_errors() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello_world").unwrap();

    let client = connect_client(&socket_path).await;
    let result = client
        .set_metadata(
            &[typed(&test_file).to_path_buf()],
            MetadataPatch::new().with_user(OwnershipIdentity::Name(
                "__dolang_missing_user__".to_string(),
            )),
        )
        .await;
    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn access_existing_file() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello").unwrap();

    let client = connect_client(&socket_path).await;

    // Test existence check (F_OK)
    let result = client.access(typed(&test_file), AccessFlags::F_OK).await;
    assert!(result.is_ok(), "File should exist");

    // Test read permission (R_OK)
    let result = client.access(typed(&test_file), AccessFlags::R_OK).await;
    assert!(result.is_ok(), "File should be readable");

    // Test write permission (W_OK)
    let result = client.access(typed(&test_file), AccessFlags::W_OK).await;
    assert!(result.is_ok(), "File should be writable");

    // Test combined read and write
    let result = client
        .access(typed(&test_file), AccessFlags::R_OK | AccessFlags::W_OK)
        .await;
    assert!(result.is_ok(), "File should be readable and writable");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn access_nonexistent_file() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;

    // Test existence check on non-existent file
    let missing = dir.path().join("nonexistent.txt");
    let result = client.access(typed(&missing), AccessFlags::F_OK).await;
    assert!(
        result.is_err(),
        "Non-existent file should fail access check"
    );

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn glob_basic_matching() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    // Create test files
    std::fs::write(dir.path().join("file1.txt"), "content1").unwrap();
    std::fs::write(dir.path().join("file2.txt"), "content2").unwrap();
    std::fs::write(dir.path().join("file.rs"), "content3").unwrap();

    let client = connect_client(&socket_path).await;

    // Test glob matching *.txt files
    let paths = client
        .glob("*.txt", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "file1.txt"));
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "file2.txt"));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn glob_recursive() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    // Create nested directory structure
    let subdir = dir.path().join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(subdir.join("nested.txt"), "nested").unwrap();

    let client = connect_client(&socket_path).await;

    // Test recursive glob with **
    let paths = client
        .glob("**/*.txt", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "root.txt"));
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "nested.txt"));

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn glob_max_depth() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    // Create nested directory structure
    let level1 = dir.path().join("level1");
    let level2 = level1.join("level2");
    std::fs::create_dir_all(&level2).unwrap();
    std::fs::write(dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(level1.join("level1.txt"), "level1").unwrap();
    std::fs::write(level2.join("level2.txt"), "level2").unwrap();

    let client = connect_client(&socket_path).await;

    // Test glob with max_depth=1 (should only find root.txt)
    let paths = client
        .glob("**/*.txt", typed(dir.path()), false, Some(1))
        .await
        .unwrap();

    assert_eq!(paths.len(), 1);
    assert!(paths[0].file_name().unwrap() == "root.txt");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn glob_with_prefix() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    // Create test files in subdirectories
    let subdir1 = dir.path().join("subdir1");
    let subdir2 = dir.path().join("subdir2");
    std::fs::create_dir(&subdir1).unwrap();
    std::fs::create_dir(&subdir2).unwrap();
    std::fs::write(subdir1.join("file.txt"), "content1").unwrap();
    std::fs::write(subdir2.join("file.txt"), "content2").unwrap();

    let client = connect_client(&socket_path).await;

    // Test glob with prefix (should use partition to extract "subdir1/")
    let paths = client
        .glob("subdir1/*.txt", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert_eq!(paths.len(), 1);
    assert!(paths[0].file_name().unwrap() == "file.txt");
    assert!(paths[0].parent().unwrap().file_name().unwrap() == "subdir1");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn glob_no_matches() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    let client = connect_client(&socket_path).await;

    // Test glob with pattern that matches nothing
    let paths = client
        .glob("*.rs", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert!(paths.is_empty());

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn glob_invalid_pattern() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;

    // Test glob with invalid pattern (should return error)
    let result = client
        .glob("[invalid", typed(dir.path()), false, None)
        .await;

    assert!(result.is_err());

    server_task.abort();
    let _ = server_task.await;
}

// Tests for direct glob behavior (no server required)

#[tokio::test]
async fn glob_local_basic_matching() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();

    // Create test files
    std::fs::write(dir.path().join("file1.txt"), "content1").unwrap();
    std::fs::write(dir.path().join("file2.txt"), "content2").unwrap();
    std::fs::write(dir.path().join("file.rs"), "content3").unwrap();

    // Test glob matching *.txt files
    let paths = direct
        .glob("*.txt", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "file1.txt"));
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "file2.txt"));
}

#[tokio::test]
async fn glob_local_recursive() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();

    // Create nested directory structure
    let subdir = dir.path().join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(subdir.join("nested.txt"), "nested").unwrap();

    // Test recursive glob with **
    let paths = direct
        .glob("**/*.txt", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "root.txt"));
    assert!(paths.iter().any(|p| p.file_name().unwrap() == "nested.txt"));
}

#[tokio::test]
async fn glob_local_max_depth() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();

    // Create nested directory structure
    let level1 = dir.path().join("level1");
    let level2 = level1.join("level2");
    std::fs::create_dir_all(&level2).unwrap();
    std::fs::write(dir.path().join("root.txt"), "root").unwrap();
    std::fs::write(level1.join("level1.txt"), "level1").unwrap();
    std::fs::write(level2.join("level2.txt"), "level2").unwrap();

    // Test glob with max_depth=1 (should only find root.txt)
    let paths = direct
        .glob("**/*.txt", typed(dir.path()), false, Some(1))
        .await
        .unwrap();

    assert_eq!(paths.len(), 1);
    assert!(paths[0].file_name().unwrap() == "root.txt");
}

#[tokio::test]
async fn glob_local_no_matches() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();

    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    // Test glob with pattern that matches nothing
    let paths = direct
        .glob("*.rs", typed(dir.path()), false, None)
        .await
        .unwrap();

    assert!(paths.is_empty());
}

#[tokio::test]
async fn glob_local_invalid_pattern() {
    let direct = Vfs::direct().unwrap();
    let dir = tempdir().unwrap();

    // Test glob with invalid pattern (should return error)
    let result = direct
        .glob("[invalid", typed(dir.path()), false, None)
        .await;

    assert!(result.is_err());
}

// --- Pre-shared key authentication ---

const TEST_KEY: &[u8] = b"a-sufficiently-long-test-key";

async fn start_keyed_server(socket_path: &Path, key: &[u8]) -> JoinHandle<()> {
    let key = AuthKey::new(key).unwrap();
    let server = dolang_vfs::server::Server::bind_with_key(socket_path, Some(key))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = server.accept().await;
    })
}

#[tokio::test]
async fn keyed_client_connects_to_keyed_server() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_keyed_server(&socket_path, TEST_KEY).await;

    let client = Vfs::connect_with_key(&socket_path, Some(AuthKey::new(TEST_KEY).unwrap()))
        .await
        .unwrap();
    assert_eq!(client.target(), &TargetInfo::current());

    stop_server(client, server_task).await;
}

#[tokio::test]
async fn wrong_key_is_rejected_without_disturbing_the_server() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_keyed_server(&socket_path, TEST_KEY).await;

    let wrong = AuthKey::new(b"an-entirely-different-key").unwrap();
    let result = Vfs::connect_with_key(&socket_path, Some(wrong)).await;
    assert!(result.is_err(), "a client with the wrong key was accepted");

    // The rejected attempt must not have cost the real client anything: the
    // server is still listening and still accepts a correct key.
    let client = Vfs::connect_with_key(&socket_path, Some(AuthKey::new(TEST_KEY).unwrap()))
        .await
        .unwrap();
    assert_eq!(client.target(), &TargetInfo::current());

    stop_server(client, server_task).await;
}

#[tokio::test]
async fn unkeyed_client_is_rejected_by_a_keyed_server() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_keyed_server(&socket_path, TEST_KEY).await;

    let result = Vfs::connect(&socket_path).await;
    assert!(result.is_err(), "an unkeyed client was accepted");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn keyed_client_is_rejected_by_an_unkeyed_server() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server_task = start_server(&socket_path).await;

    let result = Vfs::connect_with_key(&socket_path, Some(AuthKey::new(TEST_KEY).unwrap())).await;
    assert!(
        result.is_err(),
        "a keyed client was accepted by an unkeyed server"
    );

    server_task.abort();
    let _ = server_task.await;
}
