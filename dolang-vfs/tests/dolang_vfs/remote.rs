use std::{
    io::{self, SeekFrom},
    path::Path,
};

use bytes::{Bytes, BytesMut};

#[cfg(target_os = "linux")]
use dolang_vfs::file::XattrNamespace;
use dolang_vfs::{
    Vfs,
    directory::{DirEntry, ReadDir},
    error::{ErrorKind, Result as VfsResult},
    file::{CopyDest, CopyMode, File, FileLockBehavior, FileLockMode, FileLockRange},
    metadata::FileType,
    process::Command,
    server::Server,
    target::TargetInfo,
};
#[cfg(windows)]
use dolang_winterop::security::SecInfo;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::support;

async fn connected_pair() -> (Vfs, tokio::task::JoinHandle<VfsResult<()>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    // `Server::new` itself now runs the RPC handshake, so it must be driven
    // concurrently with the client's own construction below rather than
    // completed first — otherwise each side blocks waiting for the other.
    let task = tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
    (Vfs::new(client_stream).await.unwrap(), task)
}

async fn connected_split_pair() -> (Vfs, tokio::task::JoinHandle<VfsResult<()>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_stream);
    let (server_reader, server_writer) = tokio::io::split(server_stream);
    let task = tokio::spawn(async move {
        Server::new_split(server_reader, server_writer)
            .await
            .unwrap()
            .serve()
            .await
    });
    (
        Vfs::new_split(client_reader, client_writer).await.unwrap(),
        task,
    )
}

async fn stop_pair(vfs: Vfs, server: tokio::task::JoinHandle<VfsResult<()>>) {
    vfs.stop().await.unwrap();
    server.await.unwrap().unwrap();
}

#[cfg(not(windows))]
#[tokio::test]
async fn windows_admin_reports_unsupported_from_non_windows_backend() {
    let (client, server_task) = connected_pair().await;
    let cwd = dolang_vfs::path::Path::windows(r"C:\");
    let error = client
        .windows_admin(cwd, std::collections::HashMap::new(), true)
        .await
        .err()
        .expect("non-Windows backend unexpectedly opened an administrator VFS");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    stop_pair(client, server_task).await;
}

#[cfg(unix)]
async fn socket_server(path: &std::path::Path) -> tokio::task::JoinHandle<VfsResult<()>> {
    let server = Server::bind(path).await.unwrap();
    tokio::spawn(server.accept())
}

fn typed_str(path: &str) -> dolang_vfs::path::Path<'_> {
    dolang_vfs::path::Path::new(path, dolang_vfs::path::Kind::native())
}

#[cfg(unix)]
fn successful_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "exit 0"])
}

#[cfg(windows)]
fn successful_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "exit 0"])
}

#[cfg(unix)]
fn failing_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "exit 42"])
}

#[cfg(windows)]
fn failing_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "exit 42"])
}

#[cfg(unix)]
fn stdin_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "read line; test \"$line\" = remote-input"])
}

#[cfg(windows)]
fn stdin_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "findstr remote-input"])
}

#[cfg(unix)]
fn stdout_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "printf remote-stdout"])
}

#[cfg(windows)]
fn stdout_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "echo remote-stdout"])
}

#[cfg(unix)]
fn stderr_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "echo remote-stderr >&2"])
}

#[cfg(windows)]
fn stderr_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "echo remote-stderr 1>&2"])
}

#[cfg(unix)]
fn long_running_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "sleep 60"])
}

#[cfg(windows)]
fn long_running_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "ping -n 60 127.0.0.1 >nul"])
}

#[cfg(unix)]
fn cat_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "cat"])
}

#[cfg(windows)]
fn cat_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "more"])
}

#[cfg(unix)]
fn stdout_reader_command() -> (&'static str, [&'static str; 2]) {
    ("sh", ["-c", "read line; test \"$line\" = remote-stdout"])
}

#[cfg(windows)]
fn stdout_reader_command() -> (&'static str, [&'static str; 2]) {
    ("cmd", ["/C", "findstr remote-stdout"])
}

fn command_with_args<'a>(vfs: &'a Vfs, command: (&str, [&str; 2])) -> Command<'a> {
    let (program, args) = command;
    let mut command = vfs.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    command
}

#[cfg(unix)]
#[tokio::test]
async fn opaque_session_chains_to_unix_vfs() {
    let temp = tempdir().unwrap();
    let socket = temp.path().join("inner.sock");
    let inner_task = socket_server(&socket).await;
    let (outer, outer_task) = connected_pair().await;

    let socket = dolang_vfs::path::PathBuf::from_native(socket).unwrap();
    let inner = outer.unix_socket(socket.to_path(), None).await.unwrap();
    assert_eq!(inner.target(), &dolang_vfs::target::TargetInfo::current());

    let dir = dolang_vfs::path::PathBuf::from_native(temp.path().join("through-chain")).unwrap();
    inner.create_dir(dir.to_path(), false).await.unwrap();
    let file_path = dir.join("file");
    let mut options = inner.open_options();
    options.write(true).create_new(true);
    let mut file = options.open(file_path.to_path()).await.unwrap();
    file.write_all(b"chained").await.unwrap();
    file.close().await.unwrap();
    let mut entries = inner.read_dir(dir.to_path()).await.unwrap();
    assert_eq!(
        entries.next_entry().await.unwrap().unwrap().file_name(),
        "file"
    );

    let (program, args) = successful_command();
    let mut command = inner.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    inner.stop().await.unwrap();
    inner_task.await.unwrap().unwrap();
    assert_eq!(outer.target(), &dolang_vfs::target::TargetInfo::current());
    stop_pair(outer, outer_task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn opaque_session_supports_multiple_vfs_hops() {
    let temp = tempdir().unwrap();
    let middle_socket = temp.path().join("middle.sock");
    let inner_socket = temp.path().join("inner.sock");
    let middle_task = socket_server(&middle_socket).await;
    let inner_task = socket_server(&inner_socket).await;
    let (outer, outer_task) = connected_pair().await;

    let middle_path = dolang_vfs::path::PathBuf::from_native(middle_socket).unwrap();
    let inner_path = dolang_vfs::path::PathBuf::from_native(inner_socket).unwrap();
    let middle = outer
        .unix_socket(middle_path.to_path(), None)
        .await
        .unwrap();
    let inner = middle
        .unix_socket(inner_path.to_path(), None)
        .await
        .unwrap();
    assert_eq!(inner.target(), &dolang_vfs::target::TargetInfo::current());

    inner.stop().await.unwrap();
    inner_task.await.unwrap().unwrap();
    middle.stop().await.unwrap();
    middle_task.await.unwrap().unwrap();
    stop_pair(outer, outer_task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn outer_teardown_does_not_stop_retained_vfs_daemon() {
    let temp = tempdir().unwrap();
    let socket = temp.path().join("inner.sock");
    let inner_task = socket_server(&socket).await;
    let (outer, outer_task) = connected_pair().await;

    let socket_path = dolang_vfs::path::PathBuf::from_native(socket.clone()).unwrap();
    let inner = outer
        .unix_socket(socket_path.to_path(), None)
        .await
        .unwrap();
    drop(inner);
    stop_pair(outer, outer_task).await;

    let direct = Vfs::connect(&socket).await.unwrap();
    assert!(direct.cwd().is_absolute());
    stop_pair(direct, inner_task).await;
}

#[tokio::test]
async fn path_operations_work_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    assert_eq!(client.target(), &TargetInfo::current());

    let temp = tempdir().unwrap();
    let first = dolang_vfs::path::PathBuf::from_native(temp.path().join("first")).unwrap();
    let second = dolang_vfs::path::PathBuf::from_native(temp.path().join("second")).unwrap();

    client.create_dir(first.to_path(), false).await.unwrap();
    assert_eq!(
        client.metadata(first.to_path()).await.unwrap().file_type(),
        FileType::Dir
    );
    client
        .rename(first.to_path(), second.to_path(), true)
        .await
        .unwrap();
    assert!(
        client
            .canonicalize(second.to_path())
            .await
            .unwrap()
            .is_absolute()
    );
    client
        .remove_dir(second.to_path(), false, false)
        .await
        .unwrap();

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn rename_replace_flag_works_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let first_native = temp.path().join("first");
    let second_native = temp.path().join("second");
    let first = dolang_vfs::path::PathBuf::from_native(first_native.clone()).unwrap();
    let second = dolang_vfs::path::PathBuf::from_native(second_native.clone()).unwrap();
    tokio::fs::write(&first_native, b"first").await.unwrap();
    tokio::fs::write(&second_native, b"second").await.unwrap();

    let error = client
        .rename(first.to_path(), second.to_path(), false)
        .await
        .unwrap_err();
    #[cfg(target_os = "freebsd")]
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    #[cfg(not(target_os = "freebsd"))]
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(tokio::fs::read(&first_native).await.unwrap(), b"first");
    assert_eq!(tokio::fs::read(&second_native).await.unwrap(), b"second");

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn query_and_stop_work_over_split_streams() {
    let (client, server_task) = connected_split_pair().await;
    assert_eq!(client.target(), &TargetInfo::current());
    assert_eq!(client.target(), &TargetInfo::current());
    assert!(client.cwd().is_absolute());
    stop_pair(client, server_task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unix_identity_lookup_works_over_rpc() {
    use nix::unistd::{getegid, geteuid};

    let (client, server_task) = connected_pair().await;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let user = client.user_name(uid).await.unwrap();
    let group = client.group_name(gid).await.unwrap();
    assert_eq!(client.user_id(&user).await.unwrap(), uid);
    assert_eq!(client.group_id(&group).await.unwrap(), gid);
    assert_eq!(
        client
            .user_id("dolang-user-that-does-not-exist")
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn null_stdio_processes_work_over_generic_stream() {
    let (client, server_task) = connected_pair().await;

    let mut child = command_with_args(&client, successful_command())
        .spawn()
        .await
        .unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
    assert_eq!(child.wait().await.unwrap(), status);
    assert_eq!(child.terminate().await.unwrap(), Some(status));

    let mut child = command_with_args(&client, failing_command())
        .spawn()
        .await
        .unwrap();
    let status = child.wait().await.unwrap();
    assert!(!status.success());
    assert_eq!(status.code(), Some(42));

    let result = client
        .command(typed_str("nonexistent_command_12345"))
        .spawn()
        .await;
    assert!(result.is_err());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn opaque_pipe_transfers_bytes_and_reports_eof() {
    let (client, server_task) = connected_pair().await;
    let (mut send, mut recv) = client.pipe(None).await.unwrap();

    send.write_all(b"remote pipe").await.unwrap();
    send.shutdown().await.unwrap();
    send.shutdown().await.unwrap();

    let mut data = Vec::new();
    recv.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"remote pipe");

    // Stopping drains outstanding endpoints, so release them first.
    drop(recv);
    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn opaque_pipe_clones_have_independent_ownership() {
    let (client, server_task) = connected_pair().await;
    let (mut send, mut recv) = client.pipe(None).await.unwrap();
    let mut clone = send.try_clone().await.unwrap();

    send.shutdown().await.unwrap();
    clone.write_all(b"from clone").await.unwrap();
    clone.shutdown().await.unwrap();

    let mut data = Vec::new();
    recv.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"from clone");

    // Stopping drains outstanding endpoints, so release them first.
    drop(recv);
    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn opaque_pipe_reports_broken_pipe_after_receiver_drop() {
    let (client, server_task) = connected_pair().await;
    let (mut send, recv) = client.pipe(None).await.unwrap();
    drop(recv);

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match send.write_all(&[0; 4096]).await {
                Ok(()) => tokio::task::yield_now().await,
                Err(error) => break error,
            }
        }
    })
    .await
    .expect("remote receiver close did not reach the server");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    // Stopping drains outstanding endpoints, so release them first.
    drop(send);
    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn opaque_pipe_write_reports_broken_pipe_after_session_stop() {
    let (client, server_task) = connected_pair().await;
    let (mut send, recv) = client.pipe(None).await.unwrap();
    drop(recv);
    stop_pair(client, server_task).await;

    let error = send.write_all(b"after stop").await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn opaque_pipe_flush_reports_broken_pipe_after_transport_loss() {
    use tokio::time::{Duration, timeout};

    let (client, server_task) = connected_pair().await;
    let (mut send, _recv) = client.pipe(None).await.unwrap();
    // A partial trailer write leaves an RPC outstanding for flush to finish.
    let data = vec![0; 4 * 1024 * 1024];
    let written = timeout(Duration::from_secs(5), send.write(&data))
        .await
        .unwrap()
        .unwrap();
    assert!(written > 0 && written < data.len());
    server_task.abort();
    assert!(server_task.await.unwrap_err().is_cancelled());

    let error = timeout(Duration::from_secs(5), send.flush())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    client.abort().await;
}

#[tokio::test]
async fn opaque_pipe_connects_remote_children_without_client_relay() {
    let (client, server_task) = connected_pair().await;
    let (send, recv) = client.pipe(None).await.unwrap();

    let mut producer = command_with_args(&client, stdout_command());
    producer.stdout(send).unwrap();
    let mut consumer = command_with_args(
        &client,
        if cfg!(windows) {
            ("cmd", ["/C", "findstr remote-stdout"])
        } else {
            ("sh", ["-c", "read line; test \"$line\" = remote-stdout"])
        },
    );
    consumer.stdin(recv).unwrap();

    let mut consumer = consumer.spawn().await.unwrap();
    let mut producer = producer.spawn().await.unwrap();
    assert!(producer.wait().await.unwrap().success());
    assert!(consumer.wait().await.unwrap().success());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn retained_files_can_be_used_for_remote_stdio() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let stdin_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdin")).unwrap();
    let stdout_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdout")).unwrap();
    let stderr_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stderr")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = options.open(stdin_path.to_path()).await.unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();
    let mut command = command_with_args(&client, stdin_command());
    command.stdin(support::stdio_recv(stdin).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout = options.open(stdout_path.to_path()).await.unwrap();
    let mut command = command_with_args(&client, stdout_command());
    command.stdout(support::stdio_send(stdout).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stderr = options.open(stderr_path.to_path()).await.unwrap();
    let mut command = command_with_args(&client, stderr_command());
    command.stderr(support::stdio_send(stderr).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.read(true);
    let mut stdout = options.open(stdout_path.to_path()).await.unwrap();
    let mut stderr = options.open(stderr_path.to_path()).await.unwrap();
    let mut stdout_data = String::new();
    let mut stderr_data = String::new();
    stdout.read_to_string(&mut stdout_data).await.unwrap();
    stderr.read_to_string(&mut stderr_data).await.unwrap();
    assert_eq!(stdout_data.trim_end(), "remote-stdout");
    assert_eq!(stderr_data.trim_end(), "remote-stderr");

    stop_pair(client, server_task).await;
}

// Relaying stdin can cause an wedged tokio blocking reader thread;
// this test is ignored to prevent it from non-deterministically hanging
#[tokio::test]
#[ignore]
async fn inherited_stdio_is_relayed_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let mut command = command_with_args(&client, successful_command());
    command.stdin_inherit().unwrap();
    command.stdout_inherit().unwrap();
    command.stderr_inherit_stdout().unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut command = command_with_args(&client, successful_command());
    command.stderr_inherit().unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut command = command_with_args(&client, successful_command());
    command.stdout_inherit_stderr().unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut command = client.command(typed_str("nonexistent_command_12345"));
    command.stdin_inherit().unwrap();
    command.stdout_inherit().unwrap();
    assert!(command.spawn().await.is_err());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn direct_file_relays_as_remote_process_stdin() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = client.clone();
    let direct = Vfs::direct().unwrap();
    let temp = tempdir().unwrap();
    let stdin_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdin")).unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = options.open(stdin_path.to_path()).await.unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();

    let mut command = command_with_args(&remote_vfs, stdin_command());
    command.stdin(support::stdio_recv(stdin).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn remote_file_relays_as_direct_process_stdin() {
    let (client, server_task) = connected_pair().await;
    let direct_vfs = Vfs::direct().unwrap();
    let temp = tempdir().unwrap();
    let stdin_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdin")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = options.open(stdin_path.to_path()).await.unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();

    let mut command = command_with_args(&direct_vfs, stdin_command());
    command.stdin(support::stdio_recv(stdin).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn remote_process_stdout_relays_into_direct_file() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = client.clone();
    let direct = Vfs::direct().unwrap();
    let temp = tempdir().unwrap();
    let stdout_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdout")).unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout = options.open(stdout_path.to_path()).await.unwrap();

    let mut command = command_with_args(&remote_vfs, stdout_command());
    command.stdout(support::stdio_send(stdout).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = direct.open_options();
    options.read(true);
    let mut stdout = options.open(stdout_path.to_path()).await.unwrap();
    let mut stdout_data = String::new();
    stdout.read_to_string(&mut stdout_data).await.unwrap();
    assert_eq!(stdout_data.trim_end(), "remote-stdout");

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn pipe_relays_between_two_remote_sessions() {
    let (first, first_server) = connected_pair().await;
    let (second, second_server) = connected_pair().await;
    let first_vfs = first.clone();
    let second_vfs = second.clone();
    let (send, recv) = first.pipe(None).await.unwrap();

    let mut producer = command_with_args(&first_vfs, stdout_command());
    producer.stdout(send).unwrap();
    let mut consumer = command_with_args(&second_vfs, stdout_reader_command());
    consumer.stdin(recv).unwrap();

    let mut consumer = consumer.spawn().await.unwrap();
    let mut producer = producer.spawn().await.unwrap();
    assert!(producer.wait().await.unwrap().success());
    assert!(consumer.wait().await.unwrap().success());

    first.stop().await.unwrap();
    second.stop().await.unwrap();
    first_server.await.unwrap().unwrap();
    second_server.await.unwrap().unwrap();
}

#[tokio::test]
async fn file_relays_between_two_remote_sessions() {
    let (first, first_server) = connected_pair().await;
    let (second, second_server) = connected_pair().await;
    let second_vfs = second.clone();
    let temp = tempdir().unwrap();
    let stdin_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdin")).unwrap();
    let stdout_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdout")).unwrap();

    let mut options = first.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = options.open(stdin_path.to_path()).await.unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();
    let mut command = command_with_args(&second_vfs, stdin_command());
    command.stdin(support::stdio_recv(stdin).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = first.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout = options.open(stdout_path.to_path()).await.unwrap();
    let mut command = command_with_args(&second_vfs, stdout_command());
    command.stdout(support::stdio_send(stdout).await).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = first.open_options();
    options.read(true);
    let mut stdout = options.open(stdout_path.to_path()).await.unwrap();
    let mut stdout_data = String::new();
    stdout.read_to_string(&mut stdout_data).await.unwrap();
    assert_eq!(stdout_data.trim_end(), "remote-stdout");

    first.stop().await.unwrap();
    second.stop().await.unwrap();
    first_server.await.unwrap().unwrap();
    second_server.await.unwrap().unwrap();
}

#[tokio::test]
async fn pipeline_relays_across_three_domains() {
    let (a, a_server) = connected_pair().await;
    let (b, b_server) = connected_pair().await;
    let a_vfs = a.clone();
    let b_vfs = b.clone();
    let direct = Vfs::direct().unwrap();
    let temp = tempdir().unwrap();
    let stdin_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdin")).unwrap();
    let stdout_path = dolang_vfs::path::PathBuf::from_native(temp.path().join("stdout")).unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin_file = options.open(stdin_path.to_path()).await.unwrap();
    stdin_file.write_all(b"remote-stdout\n").await.unwrap();
    stdin_file.seek(SeekFrom::Start(0)).await.unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout_file = options.open(stdout_path.to_path()).await.unwrap();

    let (mid_send, mid_recv) = a.pipe(None).await.unwrap();

    let mut stage_a = command_with_args(&a_vfs, cat_command());
    stage_a
        .stdin(support::stdio_recv(stdin_file).await)
        .unwrap();
    stage_a.stdout(mid_send).unwrap();

    let mut stage_b = command_with_args(&b_vfs, cat_command());
    stage_b.stdin(mid_recv).unwrap();
    stage_b
        .stdout(support::stdio_send(stdout_file).await)
        .unwrap();

    let run = async {
        let mut stage_b = stage_b.spawn().await.unwrap();
        let mut stage_a = stage_a.spawn().await.unwrap();
        assert!(stage_a.wait().await.unwrap().success());
        assert!(stage_b.wait().await.unwrap().success());
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .unwrap();

    let mut options = direct.open_options();
    options.read(true);
    let mut stdout_read = options.open(stdout_path.to_path()).await.unwrap();
    let mut data = String::new();
    stdout_read.read_to_string(&mut data).await.unwrap();
    assert_eq!(data.trim_end(), "remote-stdout");

    a.stop().await.unwrap();
    b.stop().await.unwrap();
    a_server.await.unwrap().unwrap();
    b_server.await.unwrap().unwrap();
}

#[tokio::test]
async fn same_domain_pipe_stays_direct_through_any_vfs() {
    let (client, server_task) = connected_pair().await;
    let vfs = client.clone();
    let (send, recv) = vfs.pipe(None).await.unwrap();

    let mut producer = command_with_args(&vfs, stdout_command());
    producer.stdout(send).unwrap();
    let mut consumer = command_with_args(&vfs, stdout_reader_command());
    consumer.stdin(recv).unwrap();

    let mut consumer = consumer.spawn().await.unwrap();
    let mut producer = producer.spawn().await.unwrap();
    assert!(producer.wait().await.unwrap().success());
    assert!(consumer.wait().await.unwrap().success());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn cross_domain_stdin_relay_is_aborted_on_terminate() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = client.clone();
    let direct = Vfs::direct().unwrap();
    let (mut send, recv) = direct.pipe(None).await.unwrap();

    let mut command = command_with_args(&remote_vfs, long_running_command());
    command.stdin(recv).unwrap();
    let child = command.spawn().await.unwrap();

    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.terminate())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.unwrap().success());

    // The relay's stdin task was aborted on terminate; poll until further
    // writes observe a broken pipe rather than hanging forever.
    let error = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match send.write_all(b"more-data\n").await {
                Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                Err(error) => break error,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn cross_domain_stdio_cleans_up_after_launch_failure() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = client.clone();
    let direct = Vfs::direct().unwrap();
    let (_send, recv) = direct.pipe(None).await.unwrap();

    let mut command = remote_vfs.command(typed_str("nonexistent_command_12345"));
    command.stdin(recv).unwrap();
    assert!(command.spawn().await.is_err());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn dropping_any_child_aborts_cross_domain_relay() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = client.clone();
    let direct = Vfs::direct().unwrap();
    let (mut send, recv) = direct.pipe(None).await.unwrap();

    let mut command = command_with_args(&remote_vfs, long_running_command());
    command.stdin(recv).unwrap();
    let child = command.spawn().await.unwrap();
    drop(child);

    let error = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match send.write_all(b"more-data\n").await {
                Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                Err(error) => break error,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn remote_process_can_be_terminated() {
    let (client, server_task) = connected_pair().await;
    let child = command_with_args(&client, long_running_command())
        .spawn()
        .await
        .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.terminate())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.unwrap().success());

    stop_pair(client, server_task).await;
}

async fn collect_entries(mut read_dir: ReadDir) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    while let Some(entry) = read_dir.next_entry().await.unwrap() {
        entries.push(entry);
    }
    assert!(read_dir.next_entry().await.unwrap().is_none());
    assert!(read_dir.next_entry().await.unwrap().is_none());
    entries.sort_by(|left, right| left.file_name().cmp(right.file_name()));
    entries
}

#[tokio::test]
async fn directory_enumeration_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let direct = Vfs::direct().unwrap();
    let temp = tempdir().unwrap();

    let empty = temp.path().join("empty");
    let small = temp.path().join("small");
    let mixed = temp.path().join("mixed");
    let exact_page = temp.path().join("exact-page");
    let multiple_pages = temp.path().join("multiple-pages");
    std::fs::create_dir(&empty).unwrap();
    std::fs::create_dir(&small).unwrap();
    std::fs::create_dir(&mixed).unwrap();
    std::fs::create_dir(&exact_page).unwrap();
    std::fs::create_dir(&multiple_pages).unwrap();
    std::fs::write(small.join("only.txt"), "one").unwrap();
    std::fs::write(mixed.join("file.txt"), "file").unwrap();
    std::fs::create_dir(mixed.join("directory")).unwrap();
    for index in 0..64 {
        std::fs::write(exact_page.join(format!("entry-{index:03}")), []).unwrap();
    }
    for index in 0..129 {
        std::fs::write(multiple_pages.join(format!("entry-{index:03}")), []).unwrap();
    }

    for path in [&empty, &small, &mixed, &exact_page, &multiple_pages] {
        let path = dolang_vfs::path::PathBuf::from_native(path.to_path_buf()).unwrap();
        let remote = collect_entries(client.read_dir(path.to_path()).await.unwrap()).await;
        let local = collect_entries(direct.read_dir(path.to_path()).await.unwrap()).await;
        assert_eq!(remote, local);
    }

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn regular_file_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("file")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = options.open(path.to_path()).await.unwrap();

    file.write_all(b"abcdef").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), 6);
    assert!(file.fs_metadata().await.unwrap().capacity() > 0);

    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    let mut prefix = [0; 4];
    file.read_exact(&mut prefix).await.unwrap();
    assert_eq!(&prefix, b"abcd");
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    let mut oversized = [0; 64];
    assert_eq!(file.read(&mut oversized).await.unwrap(), 6);
    assert_eq!(&oversized[..6], b"abcdef");
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    let mut data = Vec::new();
    file.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abcdef");

    let mut file = file.try_into_std().await.unwrap_err();
    assert_eq!(file.metadata().await.unwrap().len(), 6);

    file.set_size(3).await.unwrap();
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    data.clear();
    file.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abc");
    file.close().await.unwrap();

    // Making a stdio endpoint and dropping it unused must leave the peer
    // undisturbed. It needs a handle of its own, since the handoff consumes the
    // one it is given.
    let mut options = client.open_options();
    options.read(true);
    let handoff = options.open(path.to_path()).await.unwrap();
    drop(support::stdio_recv(handoff).await);

    stop_pair(client, server_task).await;
}

// The tests below pin behavior that the move to positional I/O must either
// preserve or deliberately change. They exist so that a change in any of them
// shows up as a test diff rather than as silent drift.

/// `close` must succeed even when a read left trailer bytes undelivered.
///
/// `RemoteFile::cancel_pending` currently reaches quiescence by draining the
/// remainder to EOF, because the server holds the file until the trailer's
/// terminal fragment commits. If that handshake changes, this is the test that
/// should catch it.
#[tokio::test]
async fn remote_file_closes_with_a_read_trailer_outstanding() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("partial")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = options.open(path.to_path()).await.unwrap();

    // Large enough that one `poll_read` is unlikely to consume the whole
    // trailer, so the close path has something left to reconcile.
    let payload = vec![0x5Au8; 512 * 1024];
    file.write_all(&payload).await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);

    let mut buf = vec![0u8; payload.len()];
    let read = file.read(&mut buf).await.unwrap();
    assert!(read > 0, "expected some bytes before closing");

    file.close().await.unwrap();

    stop_pair(client, server_task).await;
}

/// Append-mode writes land at the end regardless of where the cursor is.
///
/// The remote backend really does set `O_APPEND` (the open request carries the
/// flag and the server replays it), so a seek before a write must not move
/// where the bytes go. Positional writes cannot honor an offset on such a
/// handle, which is why they will be rejected outright.
#[tokio::test]
async fn remote_append_writes_ignore_the_cursor() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("appended")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = options.open(path.to_path()).await.unwrap();
    file.write_all(b"first").await.unwrap();
    file.flush().await.unwrap();
    file.close().await.unwrap();

    let mut options = client.open_options();
    options.read(true).append(true);
    let mut file = options.open(path.to_path()).await.unwrap();
    // Rewinding must not make the write overwrite "first".
    file.seek(SeekFrom::Start(0)).await.unwrap();
    file.write_all(b"second").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), 11);
    file.close().await.unwrap();

    assert_eq!(
        std::fs::read(temp.path().join("appended")).unwrap(),
        b"firstsecond"
    );

    stop_pair(client, server_task).await;
}

/// An opaque handle has no local descriptor to surrender, from the moment it
/// is opened. `dolang-ext-sqlite` depends on this failing rather than
/// panicking or hanging, and on the handle surviving the refusal.
#[tokio::test]
async fn remote_try_into_std_fails_immediately_after_open() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("opaque")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let file = options.open(path.to_path()).await.unwrap();

    let file = file.try_into_std().await.unwrap_err();
    // The handle is still usable after the refusal.
    file.close().await.unwrap();

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn remote_file_locks_round_trip() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("locks")).unwrap();
    let mut options = client.open_options();
    options.read(true).write(true).create(true);
    let first = options.open(path.to_path()).await.unwrap();
    let second = options.open(path.to_path()).await.unwrap();
    let range = FileLockRange::to_eof(0);
    let mode = FileLockMode::Exclusive;
    let behavior = FileLockBehavior::Try;

    let mut lock = first.lock(range, mode, behavior).await.unwrap().unwrap();
    assert!(second.lock(range, mode, behavior).await.unwrap().is_none());
    lock.release().await.unwrap();
    let mut lock = second
        .lock(range, mode, behavior)
        .await
        .unwrap()
        .expect("lock acquired");
    lock.release().await.unwrap();

    // A lock dropped without an explicit release still unlocks, by way of a
    // fire-and-forget request naming the lock's own handle. Nothing orders that
    // against this task, so poll for the effect rather than assume it landed.
    let lock = first
        .lock(range, mode, behavior)
        .await
        .unwrap()
        .expect("lock acquired");
    assert!(second.lock(range, mode, behavior).await.unwrap().is_none());
    drop(lock);
    let mut reacquired = None;
    for _ in 0..200 {
        if let Some(lock) = second.lock(range, mode, behavior).await.unwrap() {
            reacquired = Some(lock);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    reacquired
        .expect("dropping a lock releases it")
        .release()
        .await
        .unwrap();

    first.close().await.unwrap();
    second.close().await.unwrap();
    stop_pair(client, server_task).await;
}

#[cfg(windows)]
#[tokio::test]
async fn security_descriptor_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("security")).unwrap();
    std::fs::write(path.to_path().as_str(), "hello").unwrap();

    let descriptor = client
        .sec_desc(path.to_path(), SecInfo::OWNER | SecInfo::DACL, true)
        .await
        .unwrap();
    assert!(descriptor.owner().is_some());
    assert!(descriptor.dacl_loaded());
    let dacl = client
        .sec_desc(path.to_path(), SecInfo::DACL, true)
        .await
        .unwrap();
    // Must succeed rather than be tolerated — see the note in `direct.rs`'s
    // `direct_security_descriptor_path_and_file`.
    client
        .update_sec_desc(path.to_path(), &dacl, true)
        .await
        .unwrap();
    let round_trip = client
        .sec_desc(path.to_path(), SecInfo::DACL, true)
        .await
        .unwrap();
    assert_eq!(round_trip.dacl(), dacl.dacl());

    let mut options = client.open_options();
    options.read(true);
    let file = options.open(path.to_path()).await.unwrap();
    assert!(
        file.sec_desc(SecInfo::OWNER)
            .await
            .unwrap()
            .owner()
            .is_some()
    );
    file.close().await.unwrap();

    stop_pair(client, server_task).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn regular_file_xattrs_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("file")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let file = options.open(path.to_path()).await.unwrap();

    file.set_xattr("remote", Some("user"), b"value")
        .await
        .unwrap();
    assert_eq!(file.xattr("remote", Some("user")).await.unwrap(), b"value");
    assert!(
        file.xattrs(XattrNamespace::Any)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.name() == "remote" && entry.namespace() == Some("user"))
    );
    file.remove_xattr("remote", Some("user")).await.unwrap();
    assert!(file.xattr("remote", Some("user")).await.is_err());
    file.close().await.unwrap();

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn stop_drains_pipe_receivers_without_waiting_for_senders() {
    use tokio::time::{Duration, sleep, timeout};

    let (client, server_task) = connected_pair().await;

    let (mut send, mut recv) = client.pipe(None).await.unwrap();

    let stopping = client.clone();
    let mut stop = tokio::spawn(async move { stopping.stop().await });

    // The stop must not complete while a receiver is still outstanding.
    sleep(Duration::from_millis(200)).await;
    assert!(
        !stop.is_finished(),
        "stop completed while a pipe receiver was still open"
    );

    // Traffic through those endpoints keeps working during the drain.
    send.write_all(b"drained").await.unwrap();
    let mut buf = [0; 7];
    recv.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"drained");

    // New endpoints are refused, though.
    let error = client.pipe(None).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotConnected);

    // Keep the sender alive: only the receiver should hold up shutdown.
    drop(recv);

    timeout(Duration::from_secs(5), &mut stop)
        .await
        .expect("stop did not complete after the receiver was closed")
        .unwrap()
        .expect("stop should succeed");
    drop(send);
    drop(client);
    server_task.await.unwrap().unwrap();
}

/// Relative and end-relative seeks resolve against a cursor this side owns.
///
/// The protocol carries no seek: `Start` and `Current` are arithmetic here, and
/// only `End` asks the peer anything. This pins that all three agree with what
/// a kernel cursor would have reported, including that a short read advances
/// the cursor by the bytes actually delivered rather than the bytes requested.
#[tokio::test]
async fn remote_relative_seeks_track_the_client_cursor() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("cursor")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = options.open(path.to_path()).await.unwrap();

    file.write_all(b"0123456789").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(file.stream_position().await.unwrap(), 10);

    assert_eq!(file.seek(SeekFrom::End(0)).await.unwrap(), 10);
    assert_eq!(file.seek(SeekFrom::End(-4)).await.unwrap(), 6);
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).await.unwrap();
    assert_eq!(tail, b"6789");
    assert_eq!(file.stream_position().await.unwrap(), 10);

    assert_eq!(file.seek(SeekFrom::Start(2)).await.unwrap(), 2);
    assert_eq!(file.seek(SeekFrom::Current(3)).await.unwrap(), 5);
    assert_eq!(file.seek(SeekFrom::Current(-1)).await.unwrap(), 4);

    // A read that asks for more than is left must leave the cursor at EOF, not
    // past it.
    let mut oversized = [0u8; 64];
    let read = file.read(&mut oversized).await.unwrap();
    assert_eq!(&oversized[..read], &b"456789"[..read]);
    assert_eq!(file.stream_position().await.unwrap(), 4 + read as u64);

    // Seeking before the start is an error, and must not disturb the cursor.
    let position = file.stream_position().await.unwrap();
    assert!(file.seek(SeekFrom::Start(0)).await.is_ok());
    assert!(file.seek(SeekFrom::Current(-1)).await.is_err());
    assert_eq!(
        file.seek(SeekFrom::Start(position)).await.unwrap(),
        position
    );

    // Seeking past the end is legal and reads as EOF.
    assert_eq!(file.seek(SeekFrom::End(16)).await.unwrap(), 26);
    assert_eq!(file.read(&mut oversized).await.unwrap(), 0);

    file.close().await.unwrap();

    stop_pair(client, server_task).await;
}

/// Positional operations work over the wire and ignore the handle's cursor.
///
/// The cursor and the positional operations are two disjoint paths over one
/// file: neither may observe the other. This also pins that several reads can
/// be outstanding on one handle at once — the whole point of making the file
/// API positional, and impossible while the peer serialized every operation on
/// one handle behind a mutex.
#[tokio::test]
async fn remote_positional_io_is_independent_of_the_cursor() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("positional")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = options.open(path.to_path()).await.unwrap();

    assert_eq!(
        file.write_at(Bytes::from_static(b"abcdefghij"), 0)
            .await
            .unwrap(),
        10
    );
    // The cursor never moved, so the stream still starts at the beginning.
    assert_eq!(file.stream_position().await.unwrap(), 0);

    let mut buf = BytesMut::with_capacity(4);
    assert_eq!(file.read_at(&mut buf, 4).await.unwrap(), 4);
    assert_eq!(&buf[..], b"efgh");
    // Reads append into spare capacity, so a cleared buffer recycles.
    buf.clear();
    buf.reserve(64);
    assert_eq!(file.read_at(&mut buf, 8).await.unwrap(), 2);
    assert_eq!(&buf[..], b"ij");

    // Past the end is end of file, not an error.
    buf.clear();
    assert_eq!(file.read_at(&mut buf, 100).await.unwrap(), 0);
    assert!(buf.is_empty());

    // Several reads in flight on one handle, issued before any is awaited.
    let mut bufs: Vec<_> = (0..5).map(|_| BytesMut::with_capacity(2)).collect();
    let reads: Vec<_> = bufs
        .iter_mut()
        .enumerate()
        .map(|(i, buf)| file.read_at(buf, i as u64 * 2))
        .collect();
    for read in reads {
        assert_eq!(read.await.unwrap(), 2);
    }
    let seen: Vec<u8> = bufs.iter().flat_map(|buf| buf.iter().copied()).collect();
    assert_eq!(seen, b"abcdefghij");

    // A positional write still leaves the cursor alone, and the cursor-based
    // path still sees what the positional one wrote.
    file.write_at(Bytes::from_static(b"XY"), 2).await.unwrap();
    let mut whole = Vec::new();
    file.read_to_end(&mut whole).await.unwrap();
    assert_eq!(whole, b"abXYefghij");

    file.close().await.unwrap();

    // `offset:` has no meaning on an append handle, so it is refused rather
    // than silently landing somewhere else.
    let mut options = client.open_options();
    options.read(true).append(true);
    let file = options.open(path.to_path()).await.unwrap();
    let error = file
        .write_at(Bytes::from_static(b"nope"), 0)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let (written, end) = file.append(Bytes::from_static(b"!!")).await.unwrap();
    assert_eq!((written, end), (2, 12));
    file.close().await.unwrap();

    stop_pair(client, server_task).await;
}

/// The borrowed-destination read is the one that lands the reply trailer
/// directly in the caller's storage. Exercise repeated reads because each read
/// is allowed to return fewer bytes than the destination can hold.
#[tokio::test]
async fn remote_read_at_into_lands_the_trailer_in_the_destination() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().join("into")).unwrap();

    let size = 16 * 1024;
    let source: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let file = options.open(path.to_path()).await.unwrap();
    // Written from borrowed storage rather than an owned `Bytes`: the trailer
    // only ever needed a slice.
    let mut written = 0;
    while written < size {
        written += file
            .write_at_from(&source[written..], written as u64)
            .await
            .unwrap();
    }
    let mut buf = vec![std::mem::MaybeUninit::<u8>::uninit(); size];
    let mut read = 0;
    while read < size {
        let count = file
            .read_at_into(&mut buf[read..], read as u64)
            .await
            .unwrap();
        assert_ne!(count, 0, "read reached EOF before the advertised file size");
        read += count;
    }
    let filled: Vec<u8> = buf
        .iter()
        .map(|byte| unsafe { byte.assume_init() })
        .collect();
    assert_eq!(filled, source);

    assert_eq!(
        file.read_at_into(&mut buf, size as u64).await.unwrap(),
        0,
        "past the end is a zero-length transfer"
    );

    file.close().await.unwrap();
    stop_pair(client, server_task).await;
}

/// A filesystem error part way through a read must reach the caller as itself,
/// not as the broken-pipe abort an abandoned trailer would produce.
#[cfg(unix)]
#[tokio::test]
async fn remote_read_failure_reports_its_error_kind() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    // A directory opens read-only on unix but cannot be read from, which is a
    // failure of the read itself rather than of opening or of the transport.
    let path = dolang_vfs::path::PathBuf::from_native(temp.path().to_path_buf()).unwrap();

    let mut options = client.open_options();
    options.read(true);
    let mut file = options.open(path.to_path()).await.unwrap();

    let mut buf = BytesMut::with_capacity(64);
    let error = file.read_at(&mut buf, 0).await.unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::BrokenPipe);
    // A failed read gives the buffer back rather than consuming it: there is
    // nothing wrong with the allocation, only with the read.
    assert_eq!(buf.capacity(), 64);
    assert_eq!(error.kind(), io::ErrorKind::IsADirectory);

    let mut buf = [0u8; 64];
    let error = file.read(&mut buf).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::IsADirectory);

    file.close().await.unwrap();
    stop_pair(client, server_task).await;
}

/// Opens a file on `vfs` for reading and writing, seeded with `content`.
async fn seeded_remote(vfs: &Vfs, temp: &Path, name: &str, content: &[u8]) -> File {
    let path = dolang_vfs::path::PathBuf::from_native(temp.join(name)).unwrap();
    let file = vfs
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path.to_path())
        .await
        .unwrap();
    if !content.is_empty() {
        assert_eq!(
            file.write_at(Bytes::copy_from_slice(content), 0)
                .await
                .unwrap(),
            content.len()
        );
    }
    file
}

/// Two opaque handles on one session take the target-side route: the server
/// runs the copy between its own files and only the count comes back.
#[tokio::test]
async fn remote_copy_data_runs_on_the_far_side() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let src = seeded_remote(&client, temp.path(), "src", b"0123456789").await;
    let dst = seeded_remote(&client, temp.path(), "dst", b"XXXXXXXXXX").await;

    assert_eq!(
        src.copy_data(&dst, 2, CopyDest::At(3), Some(3), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        3
    );
    // Neither cursor is consulted or moved: the server never sees one.
    assert_eq!(
        AsyncSeekExt::stream_position(&mut { src }).await.unwrap(),
        0
    );

    let mut buf = BytesMut::with_capacity(16);
    assert_eq!(dst.read_at(&mut buf, 0).await.unwrap(), 10);
    assert_eq!(&buf[..], b"XXX234XXXX");

    dst.close().await.unwrap();
    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn remote_append_copy_reports_the_actual_end() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let src = seeded_remote(&client, temp.path(), "append-src", b"0123456789").await;
    std::fs::write(temp.path().join("append-dst"), b"prefix").unwrap();
    let dst = client
        .open_options()
        .append(true)
        .open(
            dolang_vfs::path::PathBuf::from_native(temp.path().join("append-dst"))
                .unwrap()
                .to_path(),
        )
        .await
        .unwrap();

    let result = src
        .copy_data(&dst, 8, CopyDest::Append, Some(2), CopyMode::Auto)
        .await
        .unwrap();
    assert_eq!(result.count, 2);
    assert_eq!(result.destination_end, Some(8));

    src.close().await.unwrap();
    dst.close().await.unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("append-dst")).unwrap(),
        b"prefix89"
    );
    stop_pair(client, server_task).await;
}

/// More than one chunk, so the loop the server runs is exercised — and, on
/// this route, so is the fact that the bytes never cross the wire.
#[tokio::test]
async fn remote_copy_data_is_bounded_and_callers_can_loop() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let content: Vec<u8> = (0..2_500_123u32).map(|i| (i % 251) as u8).collect();
    let src = seeded_remote(&client, temp.path(), "big-src", &[]).await;
    for chunk in content.chunks(256 * 1024) {
        let at = (chunk.as_ptr() as usize - content.as_ptr() as usize) as u64;
        src.write_at_from(chunk, at).await.unwrap();
    }
    let dst = seeded_remote(&client, temp.path(), "big-dst", &[]).await;

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
    assert_eq!(dst.metadata().await.unwrap().len(), content.len() as u64);

    src.close().await.unwrap();
    dst.close().await.unwrap();
    assert_eq!(std::fs::read(temp.path().join("big-dst")).unwrap(), content);
    stop_pair(client, server_task).await;
}

/// Two registrations of one file have different citations, so the client
/// cannot tell they are the same. The server can, because both are local
/// there — which is the whole reason the check lives in `File::copy_data`.
#[tokio::test]
async fn remote_copy_data_rejects_overlap_the_client_cannot_see() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let first = seeded_remote(&client, temp.path(), "overlap", b"0123456789").await;
    // A second registration of the same file, left as it is rather than
    // truncated: two citations, one inode.
    let second = client
        .open_options()
        .read(true)
        .write(true)
        .open(
            dolang_vfs::path::PathBuf::from_native(temp.path().join("overlap"))
                .unwrap()
                .to_path(),
        )
        .await
        .unwrap();

    let error = first
        .copy_data(&second, 0, CopyDest::At(4), Some(8), CopyMode::Auto)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);

    // Disjoint regions of that same file still copy.
    assert_eq!(
        first
            .copy_data(&second, 0, CopyDest::At(6), Some(3), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        3
    );

    // A refusal that arrives as a structured error, not a decode failure: the
    // session is still usable afterwards.
    let error = first
        .copy_data(&second, 0, CopyDest::At(6), Some(3), CopyMode::Require)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);

    first.close().await.unwrap();
    second.close().await.unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("overlap")).unwrap(),
        b"0123450129"
    );
    stop_pair(client, server_task).await;
}

/// Handles from two different sessions have nothing shareable, so the copy
/// relays through this process rather than being refused the way a stdio
/// handoff is.
#[tokio::test]
async fn remote_copy_data_relays_across_sessions() {
    let (source_client, source_task) = connected_pair().await;
    let (dest_client, dest_task) = connected_split_pair().await;
    let temp = tempdir().unwrap();
    let src = seeded_remote(&source_client, temp.path(), "relay-src", b"0123456789").await;
    let dst = seeded_remote(&dest_client, temp.path(), "relay-dst", b"").await;

    assert_eq!(
        src.copy_data(&dst, 2, CopyDest::At(0), Some(5), CopyMode::Auto)
            .await
            .unwrap()
            .count,
        5
    );

    src.close().await.unwrap();
    dst.close().await.unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("relay-dst")).unwrap(),
        b"23456"
    );
    stop_pair(source_client, source_task).await;
    stop_pair(dest_client, dest_task).await;
}

/// One opaque handle and one local one — a pair a single session can hand out,
/// since `Open` answers with either — also relays.
#[tokio::test]
async fn remote_copy_data_relays_a_mixed_pair() {
    let (client, server_task) = connected_pair().await;
    let direct = Vfs::direct().unwrap();
    let temp = tempdir().unwrap();
    let src = seeded_remote(&client, temp.path(), "mixed-src", b"0123456789").await;
    let dst = seeded_remote(&direct, temp.path(), "mixed-dst", b"").await;

    assert_eq!(
        src.copy_data(&dst, 5, CopyDest::At(0), None, CopyMode::Auto)
            .await
            .unwrap()
            .count,
        5
    );

    src.close().await.unwrap();
    dst.close().await.unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("mixed-dst")).unwrap(),
        b"56789"
    );
    stop_pair(client, server_task).await;
}

/// The peer's session identity must differ from this process's own, even
/// though both are served by the same machine.
///
/// This is what makes the check in `open_process_info` meaningful: were the
/// identity per-host, a record captured from one agent would validate against
/// another agent's process table, which is a different namespace in every way
/// that matters (a rebooted machine recycles PIDs from the start).
#[tokio::test]
async fn session_identity_is_per_session_not_per_host() {
    let (client, server_task) = connected_pair().await;
    let direct = Vfs::direct().unwrap();
    assert_ne!(client.session(), direct.session());
    stop_pair(client, server_task).await;
}

/// The target's own PID travels with the rest of the session context, so a
/// client reports the agent serving it rather than the process asking. Both
/// ends live in this test binary, so the two agree here — what is under test is
/// that the query carries the server's answer at all, not a locally substituted
/// one.
#[tokio::test]
async fn query_reports_the_target_process_id() {
    let (client, server_task) = connected_pair().await;
    assert_eq!(Vfs::direct().unwrap().pid(), std::process::id());
    assert_eq!(client.pid(), std::process::id());
    let info = client.describe_process(client.pid()).await.unwrap();
    assert_eq!(info.pid(), client.pid());
    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn process_enumeration_round_trips_and_pages() {
    let (client, server_task) = connected_pair().await;

    let mut processes = client.processes().await.unwrap();
    let mut count = 0usize;
    let mut found = None;
    while let Some(info) = processes.next_entry().await.unwrap() {
        assert_eq!(info.session(), client.session());
        count += 1;
        if info.pid() == std::process::id() {
            found = Some(info);
        }
    }
    // Past the end stays past the end, and does not resurrect the cursor.
    assert!(processes.next_entry().await.unwrap().is_none());
    assert!(processes.next_entry().await.unwrap().is_none());

    let found = found.expect("the test process should be in the target's table");
    assert_eq!(found.pid(), std::process::id());
    // Any real system has more processes than one page holds, so reaching the
    // end at all means the paging loop ran more than once.
    assert!(count > 1, "expected more than one process, got {count}");

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn process_open_validates_identity_across_the_wire() {
    let (client, server_task) = connected_pair().await;

    let mut processes = client.processes().await.unwrap();
    let mut info = None;
    while let Some(entry) = processes.next_entry().await.unwrap() {
        if entry.pid() == std::process::id() {
            info = Some(entry);
            break;
        }
    }
    let info = info.expect("the test process should be in the target's table");
    drop(processes);

    let process = client.open_process_info(&info).await.unwrap();
    assert_eq!(process.pid(), info.pid());
    assert_eq!(
        process.info().await.unwrap().start_time(),
        info.start_time()
    );
    process.close().await.unwrap();

    // A record from a different session names a PID in a table this VFS knows
    // nothing about, so it is refused before the PID is ever used.
    let direct = Vfs::direct().unwrap();
    assert_eq!(
        direct.open_process_info(&info).await.unwrap_err().kind(),
        ErrorKind::InvalidInput
    );

    stop_pair(client, server_task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn process_wait_and_terminate_round_trip() {
    let (client, server_task) = connected_pair().await;

    // A process that will not exit on its own, opened by PID as a stranger
    // rather than through the child handle that spawned it.
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "while :; do sleep 1; done"])
        .spawn()
        .unwrap();
    let pid = child.id().expect("a spawned child has a PID");

    let process = client.open_process(pid).await.unwrap();
    let info = process.info().await.unwrap();
    assert_eq!(info.pid(), pid);
    assert!(info.identity().unwrap().is_some());

    process.terminate().await.unwrap();
    // Weaker than a child's status by design: a non-parent cannot reap, so
    // there is no code to report on Unix.
    assert_eq!(process.wait().await.unwrap().code(), None);

    let status = child.wait().await.unwrap();
    assert_eq!(status.code(), None, "expected termination by signal");

    stop_pair(client, server_task).await;
}

#[cfg(unix)]
#[tokio::test]
async fn process_kill_ends_a_process_that_ignores_sigterm() {
    use std::os::unix::process::ExitStatusExt as _;

    let (client, server_task) = connected_pair().await;

    // `trap '' TERM` makes `terminate` a no-op, which is the whole reason
    // `kill` is a separate operation on a Unix target.
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
        .spawn()
        .unwrap();
    let pid = child.id().expect("a spawned child has a PID");

    let process = client.open_process(pid).await.unwrap();
    process.kill().await.unwrap();
    process.wait().await.unwrap();

    let status = child.wait().await.unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    stop_pair(client, server_task).await;
}
