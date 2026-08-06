#![deny(warnings)]

use dolang_vfs::{AnyVfs, Direct, Error, ErrorKind};
use dolang_vfs_winreg::{Access, Key, PredefinedRoot, View};

/// `Result::unwrap_err` requires `T: Debug`, which `Key` intentionally
/// doesn't implement (it holds an `AnyVfs`, which doesn't either); this
/// does the same job without that bound.
fn expect_err<T>(result: Result<T, Error>) -> Error {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    }
}

#[cfg(not(windows))]
mod stub {
    //! On non-Windows platforms the extension is registered but backed by a
    //! stub that always reports `ErrorKind::Unsupported`. This is checked
    //! against both dispatch modes so a caller on a non-Windows peer sees a
    //! real, catchable error rather than a routing failure indistinguishable
    //! from a typo in the extension name/version.

    use dolang_vfs::{Client, Server};
    use tempfile::tempdir;
    use tokio::task::JoinHandle;

    use super::*;

    async fn start_server(socket_path: &std::path::Path) -> JoinHandle<()> {
        let path = socket_path.to_path_buf();
        let server = Server::bind(&path).await.unwrap();
        tokio::spawn(async move {
            let _ = server.accept().await;
        })
    }

    #[tokio::test]
    async fn direct_dispatch_reports_unsupported() {
        let vfs = AnyVfs::Direct(Direct::default());
        let error = expect_err(
            Key::open_root(
                &vfs,
                PredefinedRoot::CurrentUser,
                View::Native,
                Access::READ,
            )
            .await,
        );
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn remote_dispatch_reports_unsupported() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("vfs.sock");
        let _server = start_server(&socket_path).await;
        let client = Client::connect(&socket_path).await.unwrap();
        let vfs = AnyVfs::Client(client);
        let error = expect_err(
            Key::open_root(
                &vfs,
                PredefinedRoot::CurrentUser,
                View::Native,
                Access::READ,
            )
            .await,
        );
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }
}

#[cfg(windows)]
mod live {
    //! Real Windows registry CRUD tests, run under both direct and remote
    //! dispatch (the latter over a real named-pipe RPC session, following
    //! the same transport harness `dolang-vfs`'s own
    //! `tests/windows_rpc.rs` uses). Everything operates under a
    //! per-test-run scratch subkey of `HKEY_CURRENT_USER\Software\...` so
    //! tests never touch real machine state.

    use std::{
        os::windows::io::{FromRawHandle, OwnedHandle},
        sync::atomic::{AtomicU64, Ordering},
    };

    use dolang_vfs::{Client, Server};
    use dolang_vfs_winreg::Value;
    use tokio::{
        net::windows::named_pipe::{ClientOptions, ServerOptions},
        task::JoinHandle,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    use super::*;

    fn is_wine() -> bool {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

        const NTDLL: &[u8] = b"ntdll.dll\0";
        const WINE_GET_VERSION: &[u8] = b"wine_get_version\0";
        unsafe {
            let module = GetModuleHandleA(NTDLL.as_ptr());
            !module.is_null() && GetProcAddress(module, WINE_GET_VERSION.as_ptr()).is_some()
        }
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

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

    async fn connected_client() -> (Client, JoinHandle<std::io::Result<()>>) {
        let id = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\dolang-vfs-winreg-{}-{id}", std::process::id());
        let client_pipe = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let server_pipe = ClientOptions::new().open(&name).unwrap();
        client_pipe.connect().await.unwrap();

        let server_task = tokio::spawn(async move {
            Server::from_named_pipe_client(server_pipe)
                .await
                .unwrap()
                .serve()
                .await
        });
        let client =
            unsafe { Client::from_named_pipe_server(client_pipe, current_process_handle()) }
                .await
                .unwrap();
        (client, server_task)
    }

    /// A client/server pair forced into `SessionMode::Remote` even though
    /// they run in the same process, over an in-memory duplex stream. This
    /// pins down the opaque-handle fallback path independent of transport:
    /// `Client::new`/`Server::new` always report `SessionMode::Remote`
    /// regardless of what stream backs them, unlike the named-pipe
    /// transport used by `connected_client`, which is always `Native`.
    async fn forced_remote_client() -> (Client, JoinHandle<std::io::Result<()>>) {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server_task =
            tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
        let client = Client::new(client_stream).await.unwrap();
        (client, server_task)
    }

    /// Opens (creating if needed) a fresh scratch subkey under
    /// `HKEY_CURRENT_USER\Software\dolang-vfs-winreg-tests\<unique>`.
    /// Returns the `dolang-vfs-winreg-tests` key (so the caller can delete
    /// the scratch key by name when done), the scratch key's own name, and
    /// the scratch key itself.
    async fn scratch_key(vfs: &AnyVfs) -> (Key, String, Key) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);

        let root = Key::open_root(
            vfs,
            PredefinedRoot::CurrentUser,
            View::Native,
            Access::READ_WRITE,
        )
        .await
        .unwrap();
        let parent = root
            .create(
                "Software\\dolang-vfs-winreg-tests",
                View::Native,
                Access::READ_WRITE,
            )
            .await
            .unwrap();
        let name = format!("run-{}-{id}", std::process::id());
        let scratch = parent
            .create(&name, View::Native, Access::READ_WRITE)
            .await
            .unwrap();
        (parent, name, scratch)
    }

    async fn exercise(vfs: &AnyVfs) {
        let (parent, scratch_name, scratch) = scratch_key(vfs).await;

        // Enumerate on an empty key.
        assert_eq!(scratch.enum_subkey(0).await.unwrap(), None);
        assert_eq!(scratch.enum_value(0).await.unwrap(), None);
        assert_eq!(scratch.get_value(Some("missing")).await.unwrap(), None);

        // Subkeys: create, open, enumerate, delete.
        let _child = scratch
            .create("child", View::Native, Access::READ_WRITE)
            .await
            .unwrap();
        assert_eq!(
            scratch.enum_subkey(0).await.unwrap(),
            Some("child".to_string())
        );
        assert_eq!(scratch.enum_subkey(1).await.unwrap(), None);
        scratch
            .open("child", View::Native, Access::READ)
            .await
            .unwrap();
        scratch
            .delete("child", View::Native, false, false)
            .await
            .unwrap();

        // Recursive deletion clears values and arbitrarily nested subkeys.
        let tree = scratch
            .create("tree", View::Native, Access::READ_WRITE)
            .await
            .unwrap();
        tree.set_value(Some("value"), Value::Sz("root".into()))
            .await
            .unwrap();
        let leaf = tree
            .create("branch\\leaf", View::Native, Access::READ_WRITE)
            .await
            .unwrap();
        leaf.set_value(Some("value"), Value::Dword(1))
            .await
            .unwrap();
        leaf.close().await.unwrap();
        tree.close().await.unwrap();
        scratch
            .delete("tree", View::Native, true, false)
            .await
            .unwrap();
        let error = expect_err(scratch.open("tree", View::Native, Access::READ).await);
        assert_eq!(error.kind(), ErrorKind::NotFound);

        scratch
            .delete("missing", View::Native, true, true)
            .await
            .unwrap();

        // Values: every Value variant round-trips through set_value/get_value.
        let values = [
            ("sz", Value::Sz("hello".into())),
            ("expand_sz", Value::ExpandSz("%TEMP%".into())),
            ("multi_sz", Value::MultiSz(vec!["a".into(), "b".into()])),
            ("dword", Value::Dword(42)),
            ("qword", Value::Qword(u64::MAX)),
            ("binary", Value::Binary(vec![1, 2, 3])),
        ];
        for (name, value) in &values {
            scratch.set_value(Some(name), value.clone()).await.unwrap();
            assert_eq!(
                scratch.get_value(Some(name)).await.unwrap(),
                Some(value.clone())
            );
        }

        // Default (unnamed) value.
        scratch
            .set_value(None, Value::Sz("default".into()))
            .await
            .unwrap();
        assert_eq!(
            scratch.get_value(None).await.unwrap(),
            Some(Value::Sz("default".into()))
        );

        // Batched fetch reaches every name/value we set, matching indexed
        // enumeration + get_value.
        let all = scratch.values().await.unwrap();
        for (name, value) in &values {
            assert!(
                all.iter().any(|(n, v)| n == name && v == value),
                "missing {name} in values()"
            );
        }
        assert!(
            all.iter()
                .any(|(n, v)| n.is_empty() && *v == Value::Sz("default".into()))
        );

        // Indexed value enumeration reaches every name we set.
        let mut seen = Vec::new();
        for index in 0.. {
            match scratch.enum_value(index).await.unwrap() {
                Some(name) => seen.push(name),
                None => break,
            }
        }
        for (name, _) in &values {
            assert!(
                seen.contains(&name.to_string()),
                "missing {name} in enumeration"
            );
        }

        // Delete a value.
        scratch.delete_value(Some("binary")).await.unwrap();
        assert_eq!(scratch.get_value(Some("binary")).await.unwrap(), None);

        // Not-found path for a subkey open.
        let error = expect_err(
            scratch
                .open("does-not-exist", View::Native, Access::READ)
                .await,
        );
        assert_eq!(error.kind(), ErrorKind::NotFound);

        // 32/64-bit view flags: only checked on real Windows, since Wine's
        // registry doesn't reliably implement WOW64 handling.
        //
        // This does not (and cannot, without HKEY_LOCAL_MACHINE\Software
        // access, which requires elevation) exercise real WOW64
        // redirection: per Microsoft's "Registry Keys Affected by WOW64"
        // documentation, on Windows 7 and later HKEY_CURRENT_USER\Software
        // (and everything under it, including \Software\Classes) is
        // "Shared" rather than "Redirected" — the 32-bit and 64-bit views
        // both resolve to the same physical key. Only HKEY_LOCAL_MACHINE
        // \Software and HKEY_CLASSES_ROOT are actually redirected. So this
        // just checks that the View::Wow32/View::Wow64 SAM flags are
        // accepted and don't break ordinary key/value operations, not that
        // isolation actually occurs.
        if !is_wine() {
            let wow32 = parent
                .create("view-probe", View::Wow32, Access::READ_WRITE)
                .await
                .unwrap();
            wow32
                .set_value(Some("marker"), Value::Dword(1))
                .await
                .unwrap();
            let wow64 = parent
                .open("view-probe", View::Wow64, Access::READ)
                .await
                .unwrap();
            assert_eq!(
                wow64.get_value(Some("marker")).await.unwrap(),
                Some(Value::Dword(1))
            );
            wow32.close().await.unwrap();
            wow64.close().await.unwrap();
            parent
                .delete("view-probe", View::Wow32, false, false)
                .await
                .unwrap();
        }

        scratch.close().await.unwrap();
        parent
            .delete(&scratch_name, View::Native, false, false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn direct_dispatch_exercises_real_registry() {
        exercise(&AnyVfs::Direct(Direct::default())).await;
    }

    /// Runs [`exercise`] over a remote `client`/`server` pair, always
    /// joining `server`'s `JoinHandle` even if `exercise` panics.
    ///
    /// A bare `exercise(&vfs).await` at the top of a test unwinds straight
    /// out on panic, skipping any cleanup code written after it — so a
    /// server-side error/panic (which just drops the pipe) only ever shows
    /// up client-side as an opaque `ConnectionReset`, with the real cause
    /// discarded along with the unjoined `JoinHandle`. Running `exercise`
    /// itself as a task lets a panic there be caught as a `JoinError`
    /// instead, so the server task can still be joined and its outcome
    /// folded into the failure before resuming the original panic.
    async fn run_remote_exercise(vfs: AnyVfs, server: JoinHandle<std::io::Result<()>>) {
        let exercise_result = tokio::spawn(async move {
            exercise(&vfs).await;
            vfs
        })
        .await;
        match exercise_result {
            Ok(vfs) => {
                let AnyVfs::Client(client) = vfs else {
                    unreachable!()
                };
                client.stop().await.unwrap();
                server.await.unwrap().unwrap();
            }
            Err(exercise_panic) => {
                match server.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        eprintln!("server task also returned an error: {error}");
                    }
                    Err(server_panic) => {
                        eprintln!("server task also panicked: {server_panic}");
                    }
                }
                std::panic::resume_unwind(exercise_panic.into_panic());
            }
        }
    }

    #[tokio::test]
    async fn remote_dispatch_exercises_real_registry() {
        let (client, server) = connected_client().await;
        run_remote_exercise(AnyVfs::Client(client), server).await;
    }

    /// Same exercise, but over a session forced into `SessionMode::Remote`
    /// (see [`forced_remote_client`]) — this locks in the opaque-handle
    /// fallback path (`KeyHandle::Opaque`, no native handle adoption) even
    /// though client and server are same-machine, same-process.
    #[tokio::test]
    async fn forced_remote_dispatch_exercises_real_registry() {
        let (client, server) = forced_remote_client().await;
        run_remote_exercise(AnyVfs::Client(client), server).await;
    }
}
