#![cfg(windows)]
#![deny(warnings)]

use std::{
    mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use dolang_vfs::{
    AnyVfs, Child, Client, Command, FileHandle, MetadataPatch, OpenOptions, OwnershipIdentity,
    SecurityInfo, Server, TargetInfo, Utf8TypedPath, Utf8WindowsPath, Vfs,
};
use tempfile::tempdir;
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

fn is_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    const NTDLL: &[u8] = b"ntdll.dll\0";
    const WINE_GET_VERSION: &[u8] = b"wine_get_version\0";
    unsafe {
        let module = GetModuleHandleA(NTDLL.as_ptr());
        !module.is_null() && GetProcAddress(module, WINE_GET_VERSION.as_ptr()).is_some()
    }
}

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

#[test]
fn direct_security_info_reports_token_elevation() {
    let SecurityInfo::Windows(info) = SecurityInfo::current().unwrap() else {
        panic!("Windows query returned Unix security information");
    };
    assert_eq!(info.is_elevated, is_elevated());
}

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
    let client = unsafe { Client::from_named_pipe_server(client_pipe, current_process_handle()) }
        .await
        .unwrap();
    (client, server_task)
}

#[tokio::test]
async fn query_reports_server_target_including_wine() {
    let (client, server_task) = connected_pair().await;

    let query = client.query().await.unwrap();
    assert_eq!(query.target, TargetInfo::current());
    assert_eq!(query.target.is_wine, Some(is_wine()));
    assert_eq!(query.security, SecurityInfo::current().unwrap());

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn windows_account_lookup_round_trips_over_rpc() {
    let SecurityInfo::Windows(info) = SecurityInfo::current().unwrap() else {
        unreachable!()
    };
    let (client, server_task) = connected_pair().await;
    let name = client.sid_name(&info.user_sid).await.unwrap();
    let qualified = if name.domain.is_empty() {
        name.name.clone()
    } else {
        format!("{}\\{}", name.domain, name.name)
    };
    assert_eq!(
        client.account_name(&qualified).await.unwrap().sid,
        info.user_sid
    );
    assert_eq!(
        client
            .account_name("dolang-account-that-does-not-exist")
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
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
    let user = windows.user.clone().expect("owner SID was not fetched");
    let group = windows.group.clone().expect("group SID was not fetched");

    let mut options = client.open_options();
    options.read(true);
    let mut file = OpenOptions::open(&options, typed(&path)).await.unwrap();
    let file_metadata = file.metadata().await.unwrap();
    let file_windows = file_metadata.windows().unwrap();
    assert_eq!(file_windows.user.as_ref(), Some(&user));
    assert_eq!(file_windows.group.as_ref(), Some(&group));

    if is_elevated() {
        client
            .set_metadata(
                &[
                    typed(&path).to_path_buf(),
                    typed(&second_path).to_path_buf(),
                ],
                MetadataPatch {
                    user: Some(OwnershipIdentity::Sid(user.clone())),
                    ..MetadataPatch::default()
                },
            )
            .await
            .unwrap();

        let name = client.sid_name(&group).await.unwrap();
        let qualified = if name.domain.is_empty() {
            name.name
        } else {
            format!("{}\\{}", name.domain, name.name)
        };
        client
            .set_metadata(
                &[
                    typed(&path).to_path_buf(),
                    typed(&second_path).to_path_buf(),
                ],
                MetadataPatch {
                    group: Some(OwnershipIdentity::Name(qualified)),
                    ..MetadataPatch::default()
                },
            )
            .await
            .unwrap();
    }

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
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
    let (mut stdin_send, stdin_recv) = client.pipe().await.unwrap();
    let (stdout_send, mut stdout_recv) = client.pipe().await.unwrap();
    let (stderr_send, mut stderr_recv) = client.pipe().await.unwrap();

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

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn file_stdio_is_reopened_without_overlapped() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stdio.txt");
    let (client, server_task) = connected_pair().await;

    let mut options = client.open_options();
    options.write(true).create(true).truncate(true);
    let output = OpenOptions::open(&options, typed(&path)).await.unwrap();
    let mut command = client.command(typed_str("cmd.exe"));
    command
        .arg("/d")
        .arg("/s")
        .arg("/c")
        .arg("echo first")
        .stdout(output.to_stdio_send().await.unwrap())
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());
    drop(output);

    let mut options = client.open_options();
    options.append(true);
    let output = OpenOptions::open(&options, typed(&path)).await.unwrap();
    let mut command = client.command(typed_str("cmd.exe"));
    command
        .arg("/d")
        .arg("/s")
        .arg("/c")
        .arg("echo second")
        .stdout(output.to_stdio_send().await.unwrap())
        .unwrap();
    let mut child = command.spawn().await.unwrap();
    assert!(child.wait().await.unwrap().success());
    drop(output);

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("first"), "contents were {contents:?}");
    assert!(contents.contains("second"), "contents were {contents:?}");

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn spawn_failure_returns_remote_os_error() {
    let (client, server_task) = connected_pair().await;
    let result = client
        .command(typed_str("dolang-command-that-does-not-exist.exe"))
        .spawn()
        .await;
    assert!(result.is_err());

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

#[cfg(all(feature = "winreg", feature = "winscm"))]
#[tokio::test]
async fn stock_binary_registers_vfs_extensions_over_stdio() {
    use dolang_vfs::AnyVfs;
    use dolang_vfs_winreg::{Access, Key, PredefinedRoot, View};
    use dolang_vfs_winscm::{ScManager, ServiceAccess};

    const AGENT_BIN: &str = env!("CARGO_BIN_EXE_dolang-vfs");
    let mut child = tokio::process::Command::new(AGENT_BIN)
        .arg("--stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn agent");
    let stdout = child.stdout.take().expect("stdout not captured");
    let stdin = child.stdin.take().expect("stdin not captured");
    let client = Client::new_split(stdout, stdin).await.unwrap();
    let vfs = AnyVfs::Client(client.clone());

    Key::open_root(
        &vfs,
        PredefinedRoot::CurrentUser,
        View::Native,
        Access::READ,
    )
    .await
    .expect("stock binary did not register the WinReg VFS extension")
    .close()
    .await
    .unwrap();
    ScManager::open(&vfs, ServiceAccess::SC_MANAGER_CONNECT)
        .await
        .expect("stock binary did not register the WinSCM VFS extension")
        .close()
        .await
        .unwrap();

    client.stop().await.unwrap();
    // `AnyVfs::Client` owns another client clone, including the child's
    // piped stdin/stdout handles. Drop it before waiting so Windows can
    // observe every parent-side pipe handle closing when the server exits.
    drop(vfs);
    drop(client);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("VFS helper did not exit after stop")
        .unwrap();
    assert!(status.success());
}

/// Runs the VFS helper in stdio mode with the given extra arguments and
/// returns its [`dolang_vfs::Query`]. Variables in `without` are removed
/// from the child's environment, so anything that comes back must have been
/// recovered by the helper itself.
async fn query_stdio_helper(args: &[&str], without: &[&str]) -> dolang_vfs::Query {
    const AGENT_BIN: &str = env!("CARGO_BIN_EXE_dolang-vfs");

    let mut command = tokio::process::Command::new(AGENT_BIN);
    command
        .args(args)
        .arg("--stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    for name in without {
        command.env_remove(name);
    }

    let mut child = command.spawn().expect("failed to spawn agent");

    let stdout = child.stdout.take().expect("stdout not captured");
    let stdin = child.stdin.take().expect("stdin not captured");
    let client = Client::new_split(stdout, stdin)
        .await
        .expect("negotiation should succeed");

    let query = client.query().await.expect("query should succeed");

    client.stop().await.expect("stop should succeed");
    drop(client);
    let _ = child.wait().await;

    query
}

/// The Windows OpenSSH server populates only `PATH` from the registry and
/// leaves the rest of the environment pointing at the service account, so
/// `--login-env` reads the user environment out of the registry itself.
///
/// The child is started without `TEMP` and `USERPROFILE`, which a logon always
/// defines: `TEMP` lives in `HKCU\Environment`, and `USERPROFILE` is resolved
/// from the process token's profile folder. Recovering them proves the import
/// ran rather than the values simply being inherited.
#[tokio::test]
async fn login_env_imports_user_environment() {
    const REMOVED: &[&str] = &["TEMP", "USERPROFILE"];

    let imported = query_stdio_helper(&["--login-env"], REMOVED).await;
    let inherited = query_stdio_helper(&[], REMOVED).await;

    assert!(
        !inherited.env.contains_key("TEMP") && !inherited.env.contains_key("USERPROFILE"),
        "control run should not have the removed variables back"
    );

    let temp = imported
        .env
        .get("TEMP")
        .expect("TEMP should be recovered from HKCU\\Environment");
    assert!(!temp.is_empty(), "TEMP should name a directory");

    let profile = imported
        .env
        .get("USERPROFILE")
        .expect("USERPROFILE should be resolved from the profile known folder");
    assert!(!profile.is_empty(), "USERPROFILE should name a directory");

    // PATH is left exactly as it was: sshd already composed it.
    assert_eq!(
        imported.env.get("PATH"),
        inherited.env.get("PATH"),
        "PATH should not be recomposed by the import"
    );

    // The import merges; it never clears what the parent supplied.
    for name in ["SystemRoot", "COMPUTERNAME"] {
        if inherited.env.contains_key(name) {
            assert!(
                imported.env.contains_key(name),
                "{name} should survive the import"
            );
        }
    }

    // The protocol stream and working directory are undisturbed.
    assert_eq!(imported.cwd, inherited.cwd);
}
