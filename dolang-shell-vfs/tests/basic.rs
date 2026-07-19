#![deny(warnings)]
#![cfg(unix)]
use dolang_shell_vfs::{
    AccessFlags, Child, Client, Command, Direct, FileHandle, FileType, MetadataPatch,
    OwnershipIdentity, SecurityInfo, TargetInfo, Utf8TypedPath, Utf8UnixPath, Vfs,
};
#[cfg(not(target_os = "macos"))]
use nix::unistd::getgroups;
use nix::unistd::{Group, User, getegid, geteuid, getgid, getuid};
use std::{os::fd::OwnedFd, path::Path};

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
    let server = dolang_shell_vfs::Server::bind(&path).await.unwrap();
    tokio::spawn(async move {
        let _ = server.accept().await;
    })
}

async fn connect_client(socket_path: &Path) -> Client {
    Client::connect(socket_path).await.unwrap()
}

#[tokio::test]
async fn direct_query_reports_host_target() {
    let query = Direct::default().query().await.unwrap();
    assert!(!query.env.is_empty());
    assert!(query.cwd.is_absolute());
    assert!(query.current_exe.is_absolute());
    assert_eq!(query.target, TargetInfo::current());
    let SecurityInfo::Unix(security) = query.security else {
        panic!("Unix query returned Windows security information");
    };
    assert_eq!(security.uid, getuid().as_raw());
    assert_eq!(security.gid, getgid().as_raw());
    assert_eq!(security.euid, geteuid().as_raw());
    assert_eq!(security.egid, getegid().as_raw());
    #[cfg(not(target_os = "macos"))]
    assert_eq!(
        security.group_ids,
        getgroups()
            .unwrap()
            .into_iter()
            .map(|gid| gid.as_raw())
            .collect::<Vec<_>>()
    );
    #[cfg(target_os = "macos")]
    assert!(security.group_ids.contains(&getegid().as_raw()));
}

#[tokio::test]
async fn direct_resolves_unix_user_and_group_names() {
    let vfs = Direct::default();
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

    let server = dolang_shell_vfs::Server::bind(&socket_path).await.unwrap();
    let accept_task = tokio::spawn(async move {
        let _ = server.accept().await;
    });

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let fd: OwnedFd = stream.into_std().unwrap().into();
    let client = Client::try_from(fd).unwrap();

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
        .open(file.path())
        .await
        .unwrap();
    let mut command = client.command(typed_str("echo"));
    command
        .arg("hello_world")
        .stdout(output.to_stdio_send().await.unwrap())
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
        .open(&test_file)
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
        .open(&test_file)
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
        .open(&test_file)
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
        .open(&test_file)
        .await
        .unwrap();
    drop(file);

    assert!(test_file.exists());

    // Second open with create_new should fail (file exists)
    let result = client
        .open_options()
        .write(true)
        .create_new(true)
        .open(&test_file)
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
    let inner = client.unix_socket(typed(&inner_path)).await.unwrap();
    assert_eq!(inner.query().await.unwrap().target, TargetInfo::current());

    inner.as_client().unwrap().stop().await.unwrap();
    client.stop().await.unwrap();
    inner_task.await.unwrap();
    outer_task.await.unwrap();
}

#[tokio::test]
async fn unix_vfs_connect_missing() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("outer.sock");
    let missing_path = dir.path().join("missing.sock");

    let server_task = start_server(&socket_path).await;

    let client = connect_client(&socket_path).await;
    let result = client.unix_socket(typed(&missing_path)).await;
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
    let result = client.open_options().read(true).open(&test_file).await;

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

    assert_eq!(metadata.len, 11);
    assert_eq!(metadata.file_type, FileType::File);
    let unix = metadata.unix().unwrap();
    assert!(unix.mode != 0);
    assert!(unix.ino != 0);
    assert!(unix.nlink > 0);

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

    assert_eq!(metadata.file_type, FileType::Dir);
    let unix = metadata.unix().unwrap();
    assert!(unix.mode != 0);
    assert!(unix.mode != 0);

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

    assert!(metadata.capacity > 0);
    assert!(metadata.free > 0);
    assert!(metadata.available > 0);

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
            MetadataPatch {
                mode: Some(0o600),
                user: Some(OwnershipIdentity::Id(getuid().as_raw())),
                group: Some(OwnershipIdentity::Id(getgid().as_raw())),
                ..MetadataPatch::default()
            },
        )
        .await
        .unwrap();

    let metadata = client.metadata(typed(&test_file)).await.unwrap();
    let unix = metadata.unix().unwrap();
    assert_eq!(unix.uid, getuid().as_raw());
    assert_eq!(unix.gid, getgid().as_raw());
    assert_eq!(unix.mode & 0o777, 0o600);

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
            MetadataPatch {
                user: Some(OwnershipIdentity::Name(user.name)),
                group: Some(OwnershipIdentity::Name(group.name)),
                ..MetadataPatch::default()
            },
        )
        .await
        .unwrap();

    let metadata = client.metadata(typed(&test_file)).await.unwrap();
    let unix = metadata.unix().unwrap();
    assert_eq!(unix.uid, getuid().as_raw());
    assert_eq!(unix.gid, getgid().as_raw());

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
            MetadataPatch {
                group: Some(OwnershipIdentity::Id(getgid().as_raw())),
                follow: false,
                ..MetadataPatch::default()
            },
        )
        .await
        .unwrap();

    let result = client
        .set_metadata(
            &[typed(&link_path).to_path_buf()],
            MetadataPatch {
                group: Some(OwnershipIdentity::Id(getgid().as_raw())),
                ..MetadataPatch::default()
            },
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
            MetadataPatch {
                user: Some(OwnershipIdentity::Name(
                    "__dolang_missing_user__".to_string(),
                )),
                ..MetadataPatch::default()
            },
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
    let result = client.access(&test_file, AccessFlags::F_OK).await;
    assert!(result.is_ok(), "File should exist");

    // Test read permission (R_OK)
    let result = client.access(&test_file, AccessFlags::R_OK).await;
    assert!(result.is_ok(), "File should be readable");

    // Test write permission (W_OK)
    let result = client.access(&test_file, AccessFlags::W_OK).await;
    assert!(result.is_ok(), "File should be writable");

    // Test combined read and write
    let result = client
        .access(&test_file, AccessFlags::R_OK | AccessFlags::W_OK)
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
    let result = client
        .access(dir.path().join("nonexistent.txt"), AccessFlags::F_OK)
        .await;
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
    let direct = Direct::default();
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
    let direct = Direct::default();
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
    let direct = Direct::default();
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
    let direct = Direct::default();
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
    let direct = Direct::default();
    let dir = tempdir().unwrap();

    // Test glob with invalid pattern (should return error)
    let result = direct
        .glob("[invalid", typed(dir.path()), false, None)
        .await;

    assert!(result.is_err());
}
