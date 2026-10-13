#![cfg(windows)]

use std::{
    mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::temp::tempdir;
use dolang_vfs::{
    Vfs,
    error::Result as VfsResult,
    metadata::MetadataPatch,
    security::{OwnershipIdentity, SecurityInfo},
    server::Server,
    target::TargetInfo,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, ServerOptions},
    task::JoinHandle,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

fn is_elevated() -> bool {
    let mut token = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation: TOKEN_ELEVATION = unsafe { mem::zeroed() };
    let mut returned = 0;
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&raw mut elevation).cast(),
                u32::try_from(mem::size_of::<TOKEN_ELEVATION>()).unwrap(),
                &mut returned,
            )
        },
        0
    );
    elevation.TokenIsElevated != 0
}

static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

async fn stop_pair(client: Vfs, server: JoinHandle<VfsResult<()>>) {
    client.stop().await.unwrap();
    server.await.unwrap().unwrap();
}

#[test]
fn direct_security_info_reports_token_elevation() {
    let security = SecurityInfo::current().unwrap();
    let Some(info) = security.windows() else {
        panic!("Windows query returned Unix security information");
    };
    assert_eq!(info.is_elevated(), is_elevated());
}

fn typed(path: &Path) -> dolang_vfs::path::Path<'_> {
    dolang_vfs::path::Path::windows(path.to_str().unwrap())
}

fn typed_str(path: &str) -> dolang_vfs::path::Path<'_> {
    dolang_vfs::path::Path::windows(path)
}

fn current_process_handle() -> OwnedHandle {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            GetCurrentProcessId(),
        )
    };
    assert!(!handle.is_null());
    unsafe { OwnedHandle::from_raw_handle(handle as _) }
}

async fn connected_pair() -> (Vfs, JoinHandle<VfsResult<()>>) {
    let id = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
    let name = format!(r"\\.\pipe\dolang-vfs-{}-{id}", std::process::id());
    let client_pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let server_pipe = ClientOptions::new().open(&name).unwrap();
    client_pipe.connect().await.unwrap();

    // `Server::from_named_pipe_client` itself runs the RPC handshake, so it
    // must be driven concurrently with the client's own construction below
    // rather than completed first — otherwise each side blocks waiting for
    // the other.
    let server_task = tokio::spawn(async move {
        Server::from_named_pipe_client(server_pipe)
            .await
            .unwrap()
            .serve()
            .await
    });
    let client = unsafe { Vfs::from_named_pipe_server(client_pipe, current_process_handle()) }
        .await
        .unwrap();
    (client, server_task)
}

#[tokio::test]
async fn query_reports_server_target_including_wine() {
    let (client, server_task) = connected_pair().await;

    assert_eq!(client.target(), &TargetInfo::current());
    assert_eq!(client.target().is_wine(), Some(dolang_winterop::is_wine()));
    assert_eq!(client.security(), &SecurityInfo::current().unwrap());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn windows_account_lookup_round_trips_over_rpc() {
    let security = SecurityInfo::current().unwrap();
    let Some(info) = security.windows() else {
        unreachable!()
    };
    let (client, server_task) = connected_pair().await;
    let name = client.sid_name(info.user_sid()).await.unwrap();
    let qualified = if name.domain().is_empty() {
        name.name().to_owned()
    } else {
        format!("{}\\{}", name.domain(), name.name())
    };
    assert_eq!(
        client.account_name(&qualified).await.unwrap().sid(),
        info.user_sid()
    );
    assert_eq!(
        client
            .account_name("dolang-account-that-does-not-exist")
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn windows_metadata_and_ownership_round_trip_over_rpc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ownership.txt");
    let second_path = dir.path().join("ownership-2.txt");
    std::fs::write(&path, b"metadata").unwrap();
    std::fs::write(&second_path, b"metadata").unwrap();
    let (client, server_task) = connected_pair().await;

    let metadata = client.metadata(typed(&path)).await.unwrap();
    let windows = metadata.windows().unwrap();
    let user = windows.user().cloned().expect("owner SID was not fetched");
    let group = windows.group().cloned().expect("group SID was not fetched");

    let mut options = client.open_options();
    options.read(true);
    let file = options.open(typed(&path)).await.unwrap();
    let file_metadata = file.metadata().await.unwrap();
    let file_windows = file_metadata.windows().unwrap();
    assert_eq!(file_windows.user(), Some(&user));
    assert_eq!(file_windows.group(), Some(&group));

    if is_elevated() {
        client
            .update_metadata(
                &[
                    typed(&path).to_path_buf(),
                    typed(&second_path).to_path_buf(),
                ],
                MetadataPatch::new().with_user(OwnershipIdentity::Sid(user.clone())),
            )
            .await
            .unwrap();

        let name = client.sid_name(&group).await.unwrap();
        let qualified = if name.domain().is_empty() {
            name.name().to_owned()
        } else {
            format!("{}\\{}", name.domain(), name.name())
        };
        client
            .update_metadata(
                &[
                    typed(&path).to_path_buf(),
                    typed(&second_path).to_path_buf(),
                ],
                MetadataPatch::new().with_group(OwnershipIdentity::Name(qualified)),
            )
            .await
            .unwrap();
    }

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn client_or_direct_routes_path_and_open_operations() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("remote.txt");
    let subdir = dir.path().join("entries");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("one.txt"), "one").unwrap();

    let (client, server_task) = connected_pair().await;
    let vfs = client.clone();
    assert!(!vfs.is_direct());

    let mut options = vfs.open_options();
    options.write(true).create_new(true);
    let mut file = options.open(typed(&path)).await.unwrap();
    file.write_all(b"transferred handle").await.unwrap();
    file.flush().await.unwrap();
    drop(file);
    assert_eq!(std::fs::read(&path).unwrap(), b"transferred handle");

    let metadata = vfs.metadata(typed(&path)).await.unwrap();
    assert_eq!(metadata.len(), 18);

    let mut entries = vfs.read_dir(typed(&subdir)).await.unwrap();
    let entry = entries.next_entry().await.unwrap().unwrap();
    assert_eq!(entry.file_name(), "one.txt");
    assert!(entries.next_entry().await.unwrap().is_none());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn spawn_transfers_standard_stream_handles() {
    let (client, server_task) = connected_pair().await;
    let (mut stdin_send, stdin_recv) = client.pipe(None).await.unwrap();
    let (stdout_send, mut stdout_recv) = client.pipe(None).await.unwrap();
    let (stderr_send, mut stderr_recv) = client.pipe(None).await.unwrap();

    let mut command = client.command(typed_str("cmd.exe"));
    command
        .arg("/d")
        .arg("/v:on")
        .arg("/s")
        .arg("/c")
        .arg("set /p line=& echo out:!line!& echo err:!line! 1>&2");
    command.stdin(stdin_recv).unwrap();
    command.stdout(stdout_send).unwrap();
    command.stderr(stderr_send).unwrap();
    let mut child = command.spawn().await.unwrap();

    stdin_send.write_all(b"hello\r\n").await.unwrap();
    drop(stdin_send);
    let status = child.wait().await.unwrap();
    assert!(status.success());

    let mut stdout = String::new();
    let mut stderr = String::new();
    stdout_recv.read_to_string(&mut stdout).await.unwrap();
    stderr_recv.read_to_string(&mut stderr).await.unwrap();
    assert!(stdout.contains("out:hello"), "stdout was {stdout:?}");
    assert!(stderr.contains("err:hello"), "stderr was {stderr:?}");

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn file_stdio_is_reopened_without_overlapped() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stdio.txt");
    let (client, server_task) = connected_pair().await;

    let mut options = client.open_options();
    options.write(true).create(true).truncate(true);
    let output = options.open(typed(&path)).await.unwrap();
    let mut command = client.command(typed_str("cmd.exe"));
    command
        .arg("/d")
        .arg("/s")
        .arg("/c")
        .arg("echo first")
        .stdout(crate::support::stdio_send(output).await)
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let mut options = client.open_options();
    options.append(true);
    let output = options.open(typed(&path)).await.unwrap();
    let mut command = client.command(typed_str("cmd.exe"));
    command
        .arg("/d")
        .arg("/s")
        .arg("/c")
        .arg("echo second")
        .stdout(crate::support::stdio_send(output).await)
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("first"), "contents were {contents:?}");
    assert!(contents.contains("second"), "contents were {contents:?}");

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn spawn_failure_returns_remote_os_error() {
    let (client, server_task) = connected_pair().await;
    let result = client
        .command(typed_str("dolang-command-that-does-not-exist.exe"))
        .spawn()
        .await;
    assert!(result.is_err());

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn streams_run_in_the_server_namespace() {
    if dolang_winterop::is_wine() {
        return;
    }
    let dir = tempdir().unwrap();
    let path = dir.path().join("streams.txt");
    let stream_path = dir.path().join("streams.txt:zone");
    std::fs::write(&path, "data").unwrap();
    std::fs::write(&stream_path, "stream").unwrap();

    let (client, server_task) = connected_pair().await;
    let streams = client.streams(typed(&path), true).await.unwrap();
    assert!(streams.iter().any(|entry| entry.name() == "zone"));

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn disconnect_ends_the_connected_session_cleanly() {
    let (client, server_task) = connected_pair().await;
    drop(client);
    server_task.await.unwrap().unwrap();
}
