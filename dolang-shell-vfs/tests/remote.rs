#![deny(warnings)]

use std::io::{self, SeekFrom};

#[cfg(target_os = "linux")]
use dolang_vfs::XattrNamespace;
use dolang_vfs::{
    AnyCommand, AnyVfs, Child, Client, Command, DirEntry, Direct, FileHandle, FileLockBehavior,
    FileLockMode, FileLockRange, FileLockRequest, FileType, OpenOptions, ReadDir, Server,
    Utf8TypedPath, Utf8UnixPath, Utf8WindowsPath, Vfs, typed_path,
};
#[cfg(windows)]
use dolang_winterop::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

async fn connected_pair() -> (Client, tokio::task::JoinHandle<io::Result<()>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    // `Server::new` itself now runs the RPC handshake, so it must be driven
    // concurrently with the client's own construction below rather than
    // completed first — otherwise each side blocks waiting for the other.
    let task = tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
    (Client::new(client_stream).await.unwrap(), task)
}

async fn connected_split_pair() -> (Client, tokio::task::JoinHandle<io::Result<()>>) {
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
        Client::new_split(client_reader, client_writer)
            .await
            .unwrap(),
        task,
    )
}

#[cfg(not(windows))]
#[tokio::test]
async fn windows_admin_reports_unsupported_from_non_windows_backend() {
    let (client, server_task) = connected_pair().await;
    let cwd = Utf8TypedPath::Windows(Utf8WindowsPath::new(r"C:\"));
    let error = client
        .windows_admin(cwd, std::collections::HashMap::new(), true)
        .await
        .err()
        .expect("non-Windows backend unexpectedly opened an administrator VFS");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[cfg(unix)]
async fn socket_server(path: &std::path::Path) -> tokio::task::JoinHandle<io::Result<()>> {
    let server = Server::bind(path).await.unwrap();
    tokio::spawn(server.accept())
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

fn command_with_args<'a>(
    client: &'a Client,
    command: (&str, [&str; 2]),
) -> dolang_vfs::CommandBuilder<'a> {
    let (program, args) = command;
    let mut command = client.command(typed_str(program));
    command.arg(args[0]).arg(args[1]);
    command
}

fn command_with_args_any<'a>(vfs: &'a AnyVfs, command: (&str, [&str; 2])) -> AnyCommand<'a> {
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

    let socket = typed_path(socket).unwrap();
    let inner = outer.unix_socket(socket.to_path()).await.unwrap();
    assert_eq!(
        inner.query().await.unwrap().target,
        dolang_vfs::TargetInfo::current()
    );

    let dir = typed_path(temp.path().join("through-chain")).unwrap();
    inner.create_dir(dir.to_path(), false).await.unwrap();
    let file_path = dir.join("file");
    let mut options = inner.open_options();
    options.write(true).create_new(true);
    let mut file = OpenOptions::open(&options, file_path.to_path())
        .await
        .unwrap();
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

    inner.as_client().unwrap().stop().await.unwrap();
    inner_task.await.unwrap().unwrap();
    assert_eq!(
        outer.query().await.unwrap().target,
        dolang_vfs::TargetInfo::current()
    );
    outer.stop().await.unwrap();
    outer_task.await.unwrap().unwrap();
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

    let middle_path = typed_path(middle_socket).unwrap();
    let inner_path = typed_path(inner_socket).unwrap();
    let middle = outer.unix_socket(middle_path.to_path()).await.unwrap();
    let inner = middle.unix_socket(inner_path.to_path()).await.unwrap();
    assert_eq!(
        inner.query().await.unwrap().target,
        dolang_vfs::TargetInfo::current()
    );

    inner.as_client().unwrap().stop().await.unwrap();
    inner_task.await.unwrap().unwrap();
    middle.as_client().unwrap().stop().await.unwrap();
    middle_task.await.unwrap().unwrap();
    outer.stop().await.unwrap();
    outer_task.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn outer_teardown_does_not_stop_retained_vfs_daemon() {
    let temp = tempdir().unwrap();
    let socket = temp.path().join("inner.sock");
    let inner_task = socket_server(&socket).await;
    let (outer, outer_task) = connected_pair().await;

    let socket_path = typed_path(socket.clone()).unwrap();
    let inner = outer.unix_socket(socket_path.to_path()).await.unwrap();
    drop(inner);
    outer.stop().await.unwrap();
    outer_task.await.unwrap().unwrap();

    let direct = Client::connect(&socket).await.unwrap();
    direct.query().await.unwrap();
    direct.stop().await.unwrap();
    inner_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn path_operations_work_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let query = client.query().await.unwrap();
    assert_eq!(query.target, dolang_vfs::TargetInfo::current());

    let temp = tempdir().unwrap();
    let first = typed_path(temp.path().join("first")).unwrap();
    let second = typed_path(temp.path().join("second")).unwrap();

    client.create_dir(first.to_path(), false).await.unwrap();
    assert_eq!(
        client.metadata(first.to_path()).await.unwrap().file_type,
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn rename_replace_flag_works_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let first_native = temp.path().join("first");
    let second_native = temp.path().join("second");
    let first = typed_path(first_native.clone()).unwrap();
    let second = typed_path(second_native.clone()).unwrap();
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn query_and_stop_work_over_split_streams() {
    let (client, server_task) = connected_split_pair().await;
    let query = client.query().await.unwrap();
    assert_eq!(query.target, dolang_vfs::TargetInfo::current());
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

    // Stopping drains outstanding endpoints, so release them first.
    drop(recv);
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

    // Stopping drains outstanding endpoints, so release them first.
    drop(recv);
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

    // Stopping drains outstanding endpoints, so release them first.
    drop(send);
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
async fn direct_file_relays_as_remote_process_stdin() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = AnyVfs::from(client.clone());
    let direct = Direct::default();
    let temp = tempdir().unwrap();
    let stdin_path = typed_path(temp.path().join("stdin")).unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = OpenOptions::open(&options, stdin_path.to_path())
        .await
        .unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();

    let mut command = command_with_args_any(&remote_vfs, stdin_command());
    command.stdin(stdin.to_stdio_recv().await.unwrap()).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn remote_file_relays_as_direct_process_stdin() {
    let (client, server_task) = connected_pair().await;
    let direct_vfs = AnyVfs::from(Direct::default());
    let temp = tempdir().unwrap();
    let stdin_path = typed_path(temp.path().join("stdin")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = OpenOptions::open(&options, stdin_path.to_path())
        .await
        .unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();

    let mut command = command_with_args_any(&direct_vfs, stdin_command());
    command.stdin(stdin.to_stdio_recv().await.unwrap()).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn remote_process_stdout_relays_into_direct_file() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = AnyVfs::from(client.clone());
    let direct = Direct::default();
    let temp = tempdir().unwrap();
    let stdout_path = typed_path(temp.path().join("stdout")).unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();

    let mut command = command_with_args_any(&remote_vfs, stdout_command());
    command
        .stdout(stdout.to_stdio_send().await.unwrap())
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = direct.open_options();
    options.read(true);
    let mut stdout = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();
    let mut stdout_data = String::new();
    stdout.read_to_string(&mut stdout_data).await.unwrap();
    assert_eq!(stdout_data.trim_end(), "remote-stdout");

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn pipe_relays_between_two_remote_sessions() {
    let (first, first_server) = connected_pair().await;
    let (second, second_server) = connected_pair().await;
    let first_vfs = AnyVfs::from(first.clone());
    let second_vfs = AnyVfs::from(second.clone());
    let (send, recv) = first.pipe().await.unwrap();

    let mut producer = command_with_args_any(&first_vfs, stdout_command());
    producer.stdout(send).unwrap();
    let mut consumer = command_with_args_any(&second_vfs, stdout_reader_command());
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
    let second_vfs = AnyVfs::from(second.clone());
    let temp = tempdir().unwrap();
    let stdin_path = typed_path(temp.path().join("stdin")).unwrap();
    let stdout_path = typed_path(temp.path().join("stdout")).unwrap();

    let mut options = first.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin = OpenOptions::open(&options, stdin_path.to_path())
        .await
        .unwrap();
    stdin.write_all(b"remote-input\n").await.unwrap();
    stdin.seek(SeekFrom::Start(0)).await.unwrap();
    let mut command = command_with_args_any(&second_vfs, stdin_command());
    command.stdin(stdin.to_stdio_recv().await.unwrap()).unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = first.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();
    let mut command = command_with_args_any(&second_vfs, stdout_command());
    command
        .stdout(stdout.to_stdio_send().await.unwrap())
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = first.open_options();
    options.read(true);
    let mut stdout = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();
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
    let a_vfs = AnyVfs::from(a.clone());
    let b_vfs = AnyVfs::from(b.clone());
    let direct = Direct::default();
    let temp = tempdir().unwrap();
    let stdin_path = typed_path(temp.path().join("stdin")).unwrap();
    let stdout_path = typed_path(temp.path().join("stdout")).unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut stdin_file = OpenOptions::open(&options, stdin_path.to_path())
        .await
        .unwrap();
    stdin_file.write_all(b"remote-stdout\n").await.unwrap();
    stdin_file.seek(SeekFrom::Start(0)).await.unwrap();

    let mut options = direct.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let stdout_file = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();

    let (mid_send, mid_recv) = a.pipe().await.unwrap();

    let mut stage_a = command_with_args_any(&a_vfs, cat_command());
    stage_a
        .stdin(stdin_file.to_stdio_recv().await.unwrap())
        .unwrap();
    stage_a.stdout(mid_send).unwrap();

    let mut stage_b = command_with_args_any(&b_vfs, cat_command());
    stage_b.stdin(mid_recv).unwrap();
    stage_b
        .stdout(stdout_file.to_stdio_send().await.unwrap())
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
    let mut stdout_read = OpenOptions::open(&options, stdout_path.to_path())
        .await
        .unwrap();
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
    let vfs = AnyVfs::from(client.clone());
    let (send, recv) = vfs.pipe().await.unwrap();

    let mut producer = command_with_args_any(&vfs, stdout_command());
    producer.stdout(send).unwrap();
    let mut consumer = command_with_args_any(&vfs, stdout_reader_command());
    consumer.stdin(recv).unwrap();

    let mut consumer = consumer.spawn().await.unwrap();
    let mut producer = producer.spawn().await.unwrap();
    assert!(producer.wait().await.unwrap().success());
    assert!(consumer.wait().await.unwrap().success());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cross_domain_stdin_relay_is_aborted_on_terminate() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = AnyVfs::from(client.clone());
    let direct = Direct::default();
    let (mut send, recv) = direct.pipe().await.unwrap();

    let mut command = command_with_args_any(&remote_vfs, long_running_command());
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cross_domain_stdio_cleans_up_after_launch_failure() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = AnyVfs::from(client.clone());
    let direct = Direct::default();
    let (_send, recv) = direct.pipe().await.unwrap();

    let mut command = remote_vfs.command(typed_str("nonexistent_command_12345"));
    command.stdin(recv).unwrap();
    assert!(command.spawn().await.is_err());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dropping_any_child_aborts_cross_domain_relay() {
    let (client, server_task) = connected_pair().await;
    let remote_vfs = AnyVfs::from(client.clone());
    let direct = Direct::default();
    let (mut send, recv) = direct.pipe().await.unwrap();

    let mut command = command_with_args_any(&remote_vfs, long_running_command());
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
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
    assert_eq!(file.metadata().await.unwrap().len, 6);

    file.set_size(3).await.unwrap();
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    data.clear();
    file.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abc");
    file.close().await.unwrap();

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn remote_file_locks_round_trip() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("locks")).unwrap();
    let mut options = client.open_options();
    options.read(true).write(true).create(true);
    let first = OpenOptions::open(&options, path.to_path()).await.unwrap();
    let second = OpenOptions::open(&options, path.to_path()).await.unwrap();
    let request = FileLockRequest {
        range: FileLockRange {
            start: 0,
            end: None,
        },
        mode: FileLockMode::Exclusive,
        behavior: FileLockBehavior::Try,
    };

    let mut lock = first.lock(request).await.unwrap().unwrap();
    assert!(second.lock(request).await.unwrap().is_none());
    lock.release().await.unwrap();
    let mut lock = second.lock(request).await.unwrap().expect("lock acquired");
    lock.release().await.unwrap();

    first.close().await.unwrap();
    second.close().await.unwrap();
    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn security_descriptor_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("security")).unwrap();
    std::fs::write(path.to_path().as_str(), "hello").unwrap();

    let descriptor = client
        .sec_desc(
            path.to_path(),
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            true,
        )
        .await
        .unwrap();
    assert!(descriptor.owner().is_some());
    assert!(descriptor.dacl_loaded());
    let dacl = client
        .sec_desc(path.to_path(), DACL_SECURITY_INFORMATION, true)
        .await
        .unwrap();
    if let Err(error) = client.set_sec_desc(path.to_path(), &dacl, true).await {
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    let mut options = client.open_options();
    options.read(true);
    let mut file = OpenOptions::open(&options, path.to_path()).await.unwrap();
    assert!(
        file.sec_desc(OWNER_SECURITY_INFORMATION)
            .await
            .unwrap()
            .owner()
            .is_some()
    );
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

#[tokio::test]
async fn stop_drains_outstanding_pipe_endpoints() {
    use tokio::time::{Duration, sleep, timeout};

    let (client, server_task) = connected_pair().await;

    let (mut send, mut recv) = client.pipe().await.unwrap();

    let stopping = client.clone();
    let mut stop = tokio::spawn(async move { stopping.stop().await });

    // The stop must not complete while endpoints are still outstanding.
    sleep(Duration::from_millis(200)).await;
    assert!(
        !stop.is_finished(),
        "stop completed while pipe endpoints were still open"
    );

    // Traffic through those endpoints keeps working during the drain.
    send.write_all(b"drained").await.unwrap();
    let mut buf = [0; 7];
    recv.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"drained");

    // New endpoints are refused, though.
    let error = client.pipe().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotConnected);

    drop(send);
    drop(recv);

    timeout(Duration::from_secs(5), &mut stop)
        .await
        .expect("stop did not complete after endpoints were closed")
        .unwrap()
        .expect("stop should succeed");
    server_task.await.unwrap().unwrap();
}
