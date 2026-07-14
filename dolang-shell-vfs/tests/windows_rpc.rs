#![cfg(windows)]
#![deny(warnings)]

use std::{
    os::windows::io::{FromRawHandle, OwnedHandle},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use dolang_shell_vfs::{
    AnyVfs, Child, Client, Command, OpenOptions, Server, Utf8TypedPath, Utf8WindowsPath, Vfs,
};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, ServerOptions},
    task::JoinHandle,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

fn is_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    const NTDLL: &[u8] = b"ntdll.dll\0";
    const WINE_GET_VERSION: &[u8] = b"wine_get_version\0";
    unsafe {
        let module = GetModuleHandleA(NTDLL.as_ptr());
        !module.is_null() && GetProcAddress(module, WINE_GET_VERSION.as_ptr()).is_some()
    }
}

static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

fn typed(path: &Path) -> Utf8TypedPath<'_> {
    Utf8TypedPath::Windows(Utf8WindowsPath::new(path.to_str().unwrap()))
}

fn typed_str(path: &str) -> Utf8TypedPath<'_> {
    Utf8TypedPath::Windows(Utf8WindowsPath::new(path))
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

async fn connected_pair() -> (Client, JoinHandle<std::io::Result<()>>) {
    let id = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
    let name = format!(r"\\.\pipe\dolang-shell-vfs-{}-{id}", std::process::id());
    let client_pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let server_pipe = ClientOptions::new().open(&name).unwrap();
    client_pipe.connect().await.unwrap();

    let server = Server::from_named_pipe_client(server_pipe).unwrap();
    let server_task = tokio::spawn(server.serve());
    let client =
        unsafe { Client::from_named_pipe_server(client_pipe, current_process_handle()).unwrap() };
    (client, server_task)
}

#[tokio::test]
async fn client_or_direct_routes_path_and_open_operations() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("remote.txt");
    let subdir = dir.path().join("entries");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("one.txt"), "one").unwrap();

    let (client, server_task) = connected_pair().await;
    let vfs = AnyVfs::from(client.clone());
    assert!(vfs.as_client().is_some());

    let mut options = vfs.open_options();
    options.write(true).create_new(true);
    let mut file = options.open(typed(&path)).await.unwrap();
    file.write_all(b"transferred handle").await.unwrap();
    file.flush().await.unwrap();
    drop(file);
    assert_eq!(std::fs::read(&path).unwrap(), b"transferred handle");

    let metadata = vfs.metadata(typed(&path)).await.unwrap();
    assert_eq!(metadata.len, 18);

    let mut entries = vfs.read_dir(typed(&subdir)).await.unwrap();
    let entry = entries.next_entry().await.unwrap().unwrap();
    assert_eq!(entry.file_name(), "one.txt");
    assert!(entries.next_entry().await.unwrap().is_none());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn spawn_transfers_standard_stream_handles() {
    let (client, server_task) = connected_pair().await;
    let (mut stdin_send, stdin_recv) = client.pipe().unwrap();
    let (stdout_send, mut stdout_recv) = client.pipe().unwrap();
    let (stderr_send, mut stderr_recv) = client.pipe().unwrap();

    let mut command = client.command(typed_str("cmd.exe"));
    command
        .arg("/d")
        .arg("/v:on")
        .arg("/s")
        .arg("/c")
        .arg("set /p line=& echo out:!line!& echo err:!line! 1>&2");
    command.stdin_pipe(stdin_recv).unwrap();
    command.stdout_pipe(stdout_send).unwrap();
    command.stderr_pipe(stderr_send).unwrap();
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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn spawn_failure_returns_remote_os_error() {
    let (client, server_task) = connected_pair().await;
    let mut child = client
        .command(typed_str("dolang-command-that-does-not-exist.exe"))
        .spawn()
        .await
        .unwrap();
    assert!(child.wait().await.is_err());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn streams_run_in_the_server_namespace() {
    if is_wine() {
        return;
    }
    let dir = tempdir().unwrap();
    let path = dir.path().join("streams.txt");
    let stream_path = dir.path().join("streams.txt:zone");
    std::fs::write(&path, "data").unwrap();
    std::fs::write(&stream_path, "stream").unwrap();

    let (client, server_task) = connected_pair().await;
    let streams = client.streams(typed(&path), true).await.unwrap();
    assert!(streams.iter().any(|entry| entry.name == "zone"));

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn disconnect_ends_the_connected_session_cleanly() {
    let (client, server_task) = connected_pair().await;
    drop(client);
    server_task.await.unwrap().unwrap();
}
