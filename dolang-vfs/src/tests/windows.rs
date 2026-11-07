#![cfg(windows)]

use std::{
    mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::test_temp::tempdir;
use crate::{
    Vfs,
    error::Result as VfsResult,
    metadata::MetadataPatch,
    security::{OwnershipIdentity, SecurityInfo},
    server::Server,
    target::TargetInfo,
};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{
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

fn typed(path: &Path) -> crate::path::Path<'_> {
    crate::path::Path::windows(path.to_str().unwrap())
}

fn typed_str(path: &str) -> crate::path::Path<'_> {
    crate::path::Path::windows(path)
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
        .stdout(output)
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
        .stdout(output)
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
    let streams = client
        .streams(typed(&path), crate::metadata::Resolve::Target)
        .await
        .unwrap();
    assert!(streams.iter().any(|entry| entry.name() == "zone"));

    stop_pair(client, server_task).await;
}

#[tokio::test]
async fn disconnect_ends_the_connected_session_cleanly() {
    let (client, server_task) = connected_pair().await;
    drop(client);
    server_task.await.unwrap().unwrap();
}

/// Creates a junction at `link` to the directory `target` through
/// `FSCTL_SET_REPARSE_POINT`, which needs no privilege, and which Wine may
/// emulate.
fn junction(link: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
    };

    let target = std::path::absolute(target)?;
    let print: Vec<u16> = target.as_os_str().encode_wide().collect();
    let subst: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(print.iter().copied())
        .collect();
    let len = |units: &[u16]| u16::try_from(units.len() * 2).unwrap();
    let path_len = len(&subst) + 2 + len(&print) + 2;
    // REPARSE_DATA_BUFFER for a mount point: tag, data length, reserved, then
    // the substitute and print name offsets and lengths, then both names
    let mut buf = Vec::new();
    buf.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buf.extend((8 + path_len).to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    for field in [0, len(&subst), len(&subst) + 2, len(&print)] {
        buf.extend(field.to_le_bytes());
    }
    for unit in subst.iter().chain([&0]).chain(&print).chain([&0]) {
        buf.extend(unit.to_le_bytes());
    }

    std::fs::create_dir(link)?;
    let result = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(link)
        .and_then(|dir| {
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    dir.as_raw_handle(),
                    FSCTL_SET_REPARSE_POINT,
                    buf.as_ptr().cast(),
                    u32::try_from(buf.len()).unwrap(),
                    ptr::null_mut(),
                    0,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    if result.is_err() {
        let _ = std::fs::remove_dir(link);
    }
    result
}

/// Creates a junction, or returns `false` to skip the test under Wine, which
/// may not emulate junctions.
fn try_junction(link: &Path, target: &Path) -> bool {
    match junction(link, target) {
        Ok(()) => true,
        Err(err) if dolang_winterop::is_wine() => {
            eprintln!("skipping: Wine can't create a junction: {err}");
            false
        }
        Err(err) => panic!("creating junction {}: {err}", link.display()),
    }
}

const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

#[tokio::test]
async fn junction_resolves_as_a_mount() {
    use crate::metadata::{FileType, Resolve};
    let dir = tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::create_dir(&target).unwrap();
    if !try_junction(&link, &target) {
        return;
    }
    let direct = Vfs::direct().unwrap();

    for (resolve, is_mount) in [
        (Resolve::Target, false),
        (Resolve::Link, false),
        (Resolve::Mount, true),
        (Resolve::Cross, true),
    ] {
        let metadata = direct
            .metadata_resolved(typed(&link), resolve)
            .await
            .unwrap();
        let file_type = if is_mount {
            FileType::Junction
        } else {
            FileType::Dir
        };
        assert_eq!(metadata.file_type(), file_type, "{resolve:?}");
        assert_eq!(metadata.is_mount(), is_mount, "{resolve:?}");
        let tag = metadata.windows().unwrap().reparse_tag();
        let expected = is_mount.then_some(IO_REPARSE_TAG_MOUNT_POINT);
        assert_eq!(tag, expected, "{resolve:?}");
    }

    let mut entries = direct.read_dir(typed(dir.path())).await.unwrap();
    let mut found = false;
    while let Some(entry) = entries.next_entry().await.unwrap() {
        if entry.file_name() == "link" {
            assert_eq!(entry.file_type(), FileType::Dir);
            assert_eq!(entry.reparse_tag(), Some(IO_REPARSE_TAG_MOUNT_POINT));
            found = true;
        }
    }
    assert!(found);
}

#[tokio::test]
async fn recursion_stops_at_junctions_by_operation() {
    use crate::metadata::{FileType, Resolve};
    let dir = tempdir().unwrap();
    let outside = dir.path().join("outside");
    let tree = dir.path().join("tree");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "keep").unwrap();
    std::fs::create_dir(&tree).unwrap();
    if !try_junction(&tree.join("link"), &outside) {
        return;
    }
    let direct = Vfs::direct().unwrap();

    // `copy` descends into the junction and copies what it leads to
    let copy = dir.path().join("copy");
    direct
        .copy(typed(&tree), typed(&copy), true, None)
        .await
        .unwrap();
    let metadata = direct
        .metadata_resolved(typed(&copy.join("link")), Resolve::Cross)
        .await
        .unwrap();
    assert_eq!(metadata.file_type(), FileType::Dir);
    assert!(!metadata.is_mount());
    assert!(copy.join("link/keep.txt").exists());

    // Stopping at mounts copies the junction as a junction
    let copy_mount = dir.path().join("copy_mount");
    direct
        .copy(typed(&tree), typed(&copy_mount), true, Some(Resolve::Mount))
        .await
        .unwrap();
    let metadata = direct
        .metadata_resolved(typed(&copy_mount.join("link")), Resolve::Mount)
        .await
        .unwrap();
    assert!(metadata.is_mount());

    // `remove` removes the junction without touching what it leads to
    direct
        .remove(typed(&tree), true, false, None)
        .await
        .unwrap();
    direct
        .remove(typed(&copy_mount), true, false, None)
        .await
        .unwrap();
    assert!(!tree.exists());
    assert!(outside.join("keep.txt").exists());

    // Without `all`, a junction is removed like a link
    std::fs::create_dir(&tree).unwrap();
    junction(&tree.join("link"), &outside).unwrap();
    direct
        .remove(typed(&tree.join("link")), false, false, None)
        .await
        .unwrap();
    assert!(outside.join("keep.txt").exists());
}

#[tokio::test]
async fn copy_through_junction_cycle_fails() {
    let dir = tempdir().unwrap();
    let tree = dir.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    if !try_junction(&tree.join("loop"), &tree) {
        return;
    }
    let direct = Vfs::direct().unwrap();

    let err = direct
        .copy(typed(&tree), typed(&dir.path().join("copy")), true, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("directory cycle"), "{err}");
    // `remove` stops at the junction, so the cycle is harmless to it
    direct
        .remove(typed(&tree), true, false, None)
        .await
        .unwrap();
    assert!(!tree.exists());
}
