#![deny(warnings)]

use std::io::{self, SeekFrom};

#[cfg(target_os = "linux")]
use dolang_shell_vfs::XattrNamespace;
use dolang_shell_vfs::{
    Child, Client, Command, DirEntry, Direct, FileHandle, FileType, OpenOptions, ReadDir, Server,
    Utf8TypedPath, Utf8UnixPath, Utf8WindowsPath, Vfs, typed_path,
};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

async fn connected_pair() -> (Client, tokio::task::JoinHandle<io::Result<()>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    let server = Server::new(server_stream);
    let task = tokio::spawn(server.serve());
    (Client::new(client_stream), task)
}

async fn connected_split_pair() -> (Client, tokio::task::JoinHandle<io::Result<()>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_stream);
    let (server_reader, server_writer) = tokio::io::split(server_stream);
    let server = Server::new_split(server_reader, server_writer);
    let task = tokio::spawn(server.serve());
    (Client::new_split(client_reader, client_writer), task)
}

fn typed_str(path: &str) -> Utf8TypedPath<'_> {
    if cfg!(windows) {
        Utf8TypedPath::Windows(Utf8WindowsPath::new(path))
    } else {
        Utf8TypedPath::Unix(Utf8UnixPath::new(path))
    }
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

fn command_with_args<'a>(
    client: &'a Client,
    command: (&str, [&str; 2]),
) -> dolang_shell_vfs::CommandBuilder<'a> {
    let (program, args) = command;
    let mut command = client.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    command
}

#[tokio::test]
async fn path_operations_work_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let query = client.query().await.unwrap();
    assert_eq!(query.target, dolang_shell_vfs::TargetInfo::current());

    let temp = tempdir().unwrap();
    let first = typed_path(temp.path().join("first")).unwrap();
    let second = typed_path(temp.path().join("second")).unwrap();

    client.create_dir(first.to_path(), false).await.unwrap();
    assert_eq!(
        client.metadata(first.to_path()).await.unwrap().file_type,
        FileType::Dir
    );
    client
        .rename(first.to_path(), second.to_path())
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn query_and_stop_work_over_split_streams() {
    let (client, server_task) = connected_split_pair().await;
    let query = client.query().await.unwrap();
    assert_eq!(query.target, dolang_shell_vfs::TargetInfo::current());
    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
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
    assert_eq!(child.terminate().await.unwrap(), status);

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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opaque_pipe_transfers_bytes_and_reports_eof() {
    let (client, server_task) = connected_pair().await;
    let (mut send, mut recv) = client.pipe().await.unwrap();

    send.write_all(b"remote pipe").await.unwrap();
    send.shutdown().await.unwrap();
    send.shutdown().await.unwrap();

    let mut data = Vec::new();
    recv.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"remote pipe");

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opaque_pipe_clones_have_independent_ownership() {
    let (client, server_task) = connected_pair().await;
    let (mut send, mut recv) = client.pipe().await.unwrap();
    let mut clone = send.try_clone().await.unwrap();

    send.shutdown().await.unwrap();
    clone.write_all(b"from clone").await.unwrap();
    clone.shutdown().await.unwrap();

    let mut data = Vec::new();
    recv.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"from clone");

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opaque_pipe_reports_broken_pipe_after_receiver_drop() {
    let (client, server_task) = connected_pair().await;
    let (mut send, recv) = client.pipe().await.unwrap();
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opaque_pipe_connects_remote_children_without_client_relay() {
    let (client, server_task) = connected_pair().await;
    let (send, recv) = client.pipe().await.unwrap();

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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn retained_files_can_be_used_for_remote_stdio() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let stdin_path = typed_path(temp.path().join("stdin")).unwrap();
    let stdout_path = typed_path(temp.path().join("stdout")).unwrap();
    let stderr_path = typed_path(temp.path().join("stderr")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = OpenOptions::open(&options, stdin_path.to_path())
        .await
        .unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();
    let mut command = command_with_args(&client, stdin_command());
    command.stdin(stdin.to_stdio_recv().await.unwrap()).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();
    let mut command = command_with_args(&client, stdout_command());
    command
        .stdout(stdout.to_stdio_send().await.unwrap())
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stderr = OpenOptions::open(&options, stderr_path.to_path())
        .await
        .unwrap();
    let mut command = command_with_args(&client, stderr_command());
    command
        .stderr(stderr.to_stdio_send().await.unwrap())
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.read(true);
    let mut stdout = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();
    let mut stderr = OpenOptions::open(&options, stderr_path.to_path())
        .await
        .unwrap();
    let mut stdout_data = String::new();
    let mut stderr_data = String::new();
    stdout.read_to_string(&mut stdout_data).await.unwrap();
    stderr.read_to_string(&mut stderr_data).await.unwrap();
    assert_eq!(stdout_data.trim_end(), "remote-stdout");
    assert_eq!(stderr_data.trim_end(), "remote-stderr");

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
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

    let mut command = client.command(typed_str("nonexistent_command_12345"));
    command.stdin_inherit().unwrap();
    command.stdout_inherit().unwrap();
    assert!(command.spawn().await.is_err());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opaque_stdio_is_rejected_by_a_different_client_session() {
    let (first, first_server) = connected_pair().await;
    let (second, second_server) = connected_pair().await;
    let (send, recv) = first.pipe().await.unwrap();

    let mut command = command_with_args(&second, successful_command());
    let error = command.stdout(send).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let mut command = command_with_args(&second, successful_command());
    let error = command.stdin(recv).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    first.stop().await.unwrap();
    second.stop().await.unwrap();
    first_server.await.unwrap().unwrap();
    second_server.await.unwrap().unwrap();
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
    assert!(!status.success());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
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
    let direct = Direct::default();
    let temp = tempdir().unwrap();

    let empty = temp.path().join("empty");
    let small = temp.path().join("small");
    let mixed = temp.path().join("mixed");
    std::fs::create_dir(&empty).unwrap();
    std::fs::create_dir(&small).unwrap();
    std::fs::create_dir(&mixed).unwrap();
    std::fs::write(small.join("only.txt"), "one").unwrap();
    std::fs::write(mixed.join("file.txt"), "file").unwrap();
    std::fs::create_dir(mixed.join("directory")).unwrap();

    for path in [&empty, &small, &mixed] {
        let path = typed_path(path.to_path_buf()).unwrap();
        let remote = collect_entries(client.read_dir(path.to_path()).await.unwrap()).await;
        let local = collect_entries(direct.read_dir(path.to_path()).await.unwrap()).await;
        assert_eq!(remote, local);
    }

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn regular_file_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("file")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = OpenOptions::open(&options, path.to_path()).await.unwrap();

    file.write_all(b"abcdef").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(file.metadata().await.unwrap().len, 6);
    assert!(file.fs_metadata().await.unwrap().capacity > 0);

    let stdio = file.to_stdio_recv().await.unwrap();
    drop(stdio);
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    let mut data = Vec::new();
    file.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abcdef");

    let mut file = file.try_into_std().await.unwrap_err();
    assert_eq!(file.metadata().await.unwrap().len, 6);

    file.set_len(3).await.unwrap();
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    data.clear();
    file.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abc");
    file.close().await.unwrap();

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn regular_file_xattrs_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("file")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = OpenOptions::open(&options, path.to_path()).await.unwrap();

    file.set_xattr("remote", Some("user"), b"value")
        .await
        .unwrap();
    assert_eq!(file.xattr("remote", Some("user")).await.unwrap(), b"value");
    assert!(
        file.xattrs(XattrNamespace::Any)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.name == "remote" && entry.namespace.as_deref() == Some("user"))
    );
    file.remove_xattr("remote", Some("user")).await.unwrap();
    assert!(file.xattr("remote", Some("user")).await.is_err());
    file.close().await.unwrap();

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}
