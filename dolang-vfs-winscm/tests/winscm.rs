#![deny(warnings)]

use dolang_vfs::{AnyVfs, Direct, Error, ErrorKind};
use dolang_vfs_winscm::ScManager;

/// `Result::unwrap_err` requires `T: Debug`, which `ScManager`/`Service`
/// intentionally don't implement (they hold an `AnyVfs`, which doesn't
/// either); this does the same job without that bound.
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
    use dolang_vfs_winscm::ServiceAccess;
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
        let error = expect_err(ScManager::open(&vfs, ServiceAccess::SC_MANAGER_CONNECT).await);
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn remote_dispatch_reports_unsupported() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("vfs.sock");
        let _server = start_server(&socket_path).await;
        let client = Client::connect(&socket_path).await.unwrap();
        let vfs = AnyVfs::Client(client);
        let error = expect_err(ScManager::open(&vfs, ServiceAccess::SC_MANAGER_CONNECT).await);
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }
}

#[cfg(windows)]
mod live {
    //! Real Windows SCM tests, run under both direct and remote dispatch
    //! (the latter over a real named-pipe RPC session, following the same
    //! transport harness `dolang-vfs-winreg`'s own tests use).
    //!
    //! `CreateServiceW`/`OpenSCManagerW(SC_MANAGER_CREATE_SERVICE)` require
    //! administrator privileges on real Windows. Rather than failing
    //! outright when not elevated, [`scratch_service`] reports the failure
    //! and every test built on it skips (with a printed message) instead of
    //! panicking — mirroring how `dolang-vfs-winreg`'s tests skip
    //! Wine-incompatible assertions rather than treating them as failures.

    use std::{
        os::windows::io::{FromRawHandle, OwnedHandle},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use dolang_vfs::{Client, Server};
    use dolang_vfs_winscm::{
        CreateServiceOptions, ErrorControl, NotifyMask, Service, ServiceAccess,
        ServiceConfigUpdate, ServiceState, ServiceStateFilter, ServiceType, StartType,
    };
    use dolang_winterop::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
    use tokio::{
        net::windows::named_pipe::{ClientOptions, ServerOptions},
        task::JoinHandle,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    use super::*;

    /// Whether `NotifyServiceStatusChangeW` appears to support genuine
    /// asynchronous delivery for a *future* status transition, as opposed
    /// to only the "callback fires immediately if the service is already
    /// in one of the requested states" case that both Wine and real
    /// Windows document.
    ///
    /// Confirmed empirically: under Wine, registering a wait whose mask
    /// excludes the service's current state reliably times out even after
    /// the service is told to start — Wine's SCM does not appear to
    /// deliver a notification for an actual future transition, only the
    /// immediate-match case. Real Windows is expected to support genuine
    /// delivery (that's the entire reason this crate's async wait exists),
    /// but this suite can't assume that without running there, so it
    /// degrades gracefully instead of hard-failing under Wine.
    fn supports_async_notification() -> bool {
        !is_wine()
    }

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

    const TIMEOUT: Duration = Duration::from_secs(10);

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

    async fn connected_client() -> (Client, JoinHandle<std::io::Result<()>>) {
        let id = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\dolang-vfs-winscm-{}-{id}", std::process::id());
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
    /// they run in the same process, over an in-memory duplex stream —
    /// exercises remote dispatch's wire (de)serialization path even though
    /// this extension never uses native-handle passthrough itself.
    async fn forced_remote_client() -> (Client, JoinHandle<std::io::Result<()>>) {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server_task =
            tokio::spawn(async move { Server::new(server_stream).await.unwrap().serve().await });
        let client = Client::new(client_stream).await.unwrap();
        (client, server_task)
    }

    /// Resolves the path to a real, quick-exiting executable to use as a
    /// service's binary. It doesn't need to implement the service-control
    /// protocol at all — SCM will drive the service from `START_PENDING` to
    /// `STOPPED` once the process exits on its own, which is the observable
    /// transition the notify-wait tests await.
    fn dummy_binary_path() -> String {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        format!(r"{system_root}\System32\cmd.exe /c exit 0")
    }

    /// Opens the SC manager and creates a uniquely named scratch service,
    /// returning `None` (with a printed message) if that requires
    /// privileges this test process doesn't have, rather than failing.
    async fn scratch_service(vfs: &AnyVfs) -> Option<(ScManager, Service, String)> {
        let manager = match ScManager::open(
            vfs,
            ServiceAccess::SC_MANAGER_CREATE_SERVICE
                | ServiceAccess::SC_MANAGER_CONNECT
                | ServiceAccess::SC_MANAGER_ENUMERATE_SERVICE,
        )
        .await
        {
            Ok(manager) => manager,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping: opening the SC manager for creation requires Administrator: {error}"
                );
                return None;
            }
            Err(error) => panic!("failed to open SC manager: {error}"),
        };

        // Services are stored in a persistent, system-wide database, unlike
        // an in-memory test fixture — a name collision with a leftover
        // service from a previous (e.g. panicked, cleanup-skipping) test
        // run is a real possibility, especially since process IDs get
        // reused across separate runs. Including a wall-clock timestamp
        // alongside the pid/counter makes that practically impossible.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("dolang-vfs-winscm-test-{}-{now}-{id}", std::process::id());
        let service = match manager
            .create_service_with_options(
                &name,
                &name,
                ServiceType::WIN32_OWN_PROCESS,
                StartType::DEMAND_START,
                ErrorControl::NORMAL,
                &dummy_binary_path(),
                CreateServiceOptions {
                    service_start_name: Some("LocalSystem".to_string()),
                    ..CreateServiceOptions::default()
                },
                ServiceAccess::SERVICE_ALL_ACCESS,
            )
            .await
        {
            Ok(service) => service,
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                eprintln!("skipping: creating a service requires Administrator: {error}");
                return None;
            }
            Err(error) => panic!("failed to create service: {error}"),
        };

        Some((manager, service, name))
    }

    async fn cleanup(manager: ScManager, service: Service, name: String) {
        // Best-effort: the dummy binary may already have made the service
        // exit and SCM may have torn down transient state around it, so
        // don't fail the test over cleanup races.
        let _ = service.delete().await;
        let _ = service.close().await;
        let _ = manager.close().await;
        let _ = name;
    }

    async fn exercise(vfs: &AnyVfs) {
        let Some((manager, service, name)) = scratch_service(vfs).await else {
            return;
        };

        // A freshly created, never-started service is stopped.
        let status = service.query_status().await.unwrap();
        assert_eq!(status.current_state, ServiceState::STOPPED);

        // Configuration is returned as a point-in-time value, and selective
        // updates leave every omitted field unchanged.
        let config = service.config().await.unwrap();
        assert_eq!(config.service_type, ServiceType::WIN32_OWN_PROCESS);
        assert_eq!(config.start_type, StartType::DEMAND_START);
        assert_eq!(config.error_control, ErrorControl::NORMAL);
        assert_eq!(config.display_name, name);
        assert!(config.dependencies.is_empty());
        let updated_display_name = format!("{name} updated");
        service
            .set_config(ServiceConfigUpdate {
                display_name: Some(updated_display_name.clone()),
                dependencies: Some(Vec::new()),
                ..ServiceConfigUpdate::default()
            })
            .await
            .unwrap();
        let updated = service.config().await.unwrap();
        assert_eq!(updated.display_name, updated_display_name);
        assert_eq!(updated.binary_path, config.binary_path);
        assert_eq!(updated.start_type, config.start_type);
        // The earlier object is an independent snapshot.
        assert_eq!(config.display_name, name);

        // Not-found path.
        let error = expect_err(
            manager
                .open_service(
                    "dolang-vfs-winscm-definitely-not-a-real-service",
                    ServiceAccess::SERVICE_QUERY_STATUS,
                )
                .await,
        );
        assert_eq!(error.kind(), ErrorKind::NotFound);

        // The scratch service shows up in an enumeration filtered to Win32
        // services in any state, with a status matching a direct query.
        let services = manager
            .enumerate_services(ServiceType::WIN32, ServiceStateFilter::ALL)
            .await
            .unwrap();
        let entry = services
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("scratch service {name} missing from enumeration"));
        assert_eq!(entry.status.current_state, ServiceState::STOPPED);

        // Security descriptor round trip: fetch the owner + DACL, then set
        // the exact same descriptor back unmodified. Proves
        // `QueryServiceObjectSecurity`/`SetServiceObjectSecurity` work
        // end-to-end without needing to construct a new ACL from scratch —
        // `scratch_service` already opened the service with
        // `SERVICE_ALL_ACCESS`, which includes the `READ_CONTROL`/
        // `WRITE_DAC` rights both calls need.
        //
        // Under Wine specifically, the call succeeds but the returned
        // descriptor has no owner SID — Wine's SCM security-descriptor
        // support appears to be a stub, same category of gap as
        // `supports_async_notification`'s. The round trip itself (get, then
        // set back without error) still proves the wire plumbing works, so
        // only the owner-presence assertion is skipped there.
        let mask = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let descriptor = service.sec_desc(mask).await.unwrap();
        if !is_wine() {
            assert!(descriptor.owner().is_some(), "expected an owner SID");
        }
        service.set_sec_desc(&descriptor).await.unwrap();

        // Start the service and observe a status-change notification
        // fire — the actual point of this crate: an async wait built on
        // `dolang_winterop::apc`'s reactor for a Win32 API that delivers
        // completion via APC.
        //
        // On real Windows the mask excludes `STOPPED` (the service's
        // current state) so this genuinely waits for a future transition
        // rather than the immediate synchronous callback both Wine and
        // real Windows document for an already-matching current state.
        // Under Wine specifically, that genuine-future-transition case was
        // confirmed empirically to never fire at all for this dummy,
        // fails-fast binary (a real timeout, not a race) — Wine's SCM
        // notification support appears limited to the immediate-match
        // case — so the mask there includes `STOPPED` too, and the
        // stronger "did the state actually change" assertion is skipped.
        // This still proves the full API/reactor round-trip works
        // end-to-end under Wine, just not genuine asynchronous delivery;
        // see `supports_async_notification`'s doc comment.
        //
        // `start()` itself is allowed to fail regardless of platform: the
        // dummy binary doesn't implement the service-control protocol, so
        // an SCM implementation may report a start timeout/failure once it
        // gives up waiting for the process to check in.
        let async_notification = supports_async_notification();
        let mask = if async_notification {
            NotifyMask::START_PENDING | NotifyMask::RUNNING
        } else {
            NotifyMask::START_PENDING | NotifyMask::RUNNING | NotifyMask::STOPPED
        };
        // Scoped so the pinned wait future (and its borrow of `service`) is
        // dropped before `service` moves into `cleanup` below.
        let status = {
            let mut wait = std::pin::pin!(service.wait_for_status_change(mask));
            // `wait` is a lazy future: nothing has been sent to the server
            // yet until it's actually polled. Poll it once now so the
            // `WaitForStatusChange` request (and the real
            // `NotifyServiceStatusChangeW` registration behind it) is
            // genuinely in flight before triggering the transition below —
            // otherwise the sleep just runs with nothing happening,
            // `start()` completes (and the dummy binary, which doesn't
            // check in with SCM, may already have driven the service back
            // to `STOPPED` by the time it does), and only then does the
            // wait actually register — against a service that's already
            // past the transition it's supposed to observe, hanging until
            // this test's own timeout.
            assert!(
                futures::poll!(wait.as_mut()).is_pending(),
                "wait resolved before the service transitioned"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Err(error) = service.start().await {
                eprintln!("service start reported failure (tolerated): {error}");
            }
            tokio::time::timeout(TIMEOUT, wait)
                .await
                .expect("status-change wait timed out")
                .expect("status-change wait failed")
        };
        if async_notification {
            assert_ne!(
                status.current_state,
                ServiceState::STOPPED,
                "expected a transition away from STOPPED"
            );
        }

        cleanup(manager, service, name).await;
    }

    /// Cancelling an in-flight status-change wait must not hang the caller,
    /// nor leave the service's own handle stuck unable to register another
    /// wait — this is what actually exercises the dedicated-notification-
    /// handle design (see `crate::service::Service`'s doc comment: SCM has
    /// no "unregister" API, only "close the handle a request was
    /// registered on," so `wait_for_status_change` always uses a handle
    /// scoped to just that one call) and `ApcContext::cancel_guard`'s
    /// hazard-drain path (see `dolang-winterop::apc`'s module doc) from a
    /// real caller, not just synthetic unit tests.
    async fn exercise_cancellation(vfs: &AnyVfs) {
        let Some((manager, service, name)) = scratch_service(vfs).await else {
            return;
        };

        // Excludes `STOPPED` (the service's current state) for the same
        // reason as `exercise`'s wait — see its comment — so this wait
        // stays genuinely pending (the service never actually starts in
        // this block) rather than resolving immediately.
        let mask = NotifyMask::START_PENDING | NotifyMask::RUNNING;
        {
            let wait = service.wait_for_status_change(mask);
            tokio::pin!(wait);
            // The service never changes state here, so the wait must still
            // be pending — otherwise this isn't testing cancellation of a
            // genuinely in-flight wait.
            tokio::time::timeout(Duration::from_millis(200), &mut wait)
                .await
                .expect_err("wait resolved without any status change");
            // Dropping `wait` here cancels it.
        }

        // A second wait on the very same `Service` handle must still work
        // promptly — proving the cancelled wait's cleanup (closing its own
        // dedicated notification handle, then draining) didn't corrupt the
        // shared reactor or leave anything about `service` itself unusable.
        // See `exercise`'s comment for why the mask includes `STOPPED` when
        // the platform doesn't support genuine async delivery.
        let mask = if supports_async_notification() {
            NotifyMask::START_PENDING | NotifyMask::RUNNING
        } else {
            NotifyMask::START_PENDING | NotifyMask::RUNNING | NotifyMask::STOPPED
        };
        // Scoped so the pinned wait future (and its borrow of `service`) is
        // dropped before `service` moves into `cleanup` below.
        {
            let mut wait = std::pin::pin!(service.wait_for_status_change(mask));
            // See `exercise`'s comment: this must be polled once before the
            // sleep/start below to actually put the request in flight, or
            // the registration races behind the service's own transition
            // back to `STOPPED`.
            assert!(
                futures::poll!(wait.as_mut()).is_pending(),
                "wait resolved before the service transitioned"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Err(error) = service.start().await {
                eprintln!("service start reported failure (tolerated): {error}");
            }
            tokio::time::timeout(TIMEOUT, wait)
                .await
                .expect("status-change wait timed out after a prior cancellation")
                .expect("status-change wait failed after a prior cancellation");
        }

        cleanup(manager, service, name).await;
    }

    /// Runs every dispatch-mode combination sequentially in one test,
    /// rather than as separate `#[tokio::test]` functions (which `cargo
    /// test` runs concurrently within the same process by default).
    ///
    /// This is a real, empirically-discovered constraint, not a stylistic
    /// choice: Wine's SCM implementation is itself RPC-based
    /// (`rpc:RpcServerAssoc_FindContextHandle`/`I_RpcReceive` errors appear
    /// in its own logs), and concurrent SCM traffic from multiple tests in
    /// the same process reliably produced spurious `ERROR_INVALID_HANDLE`
    /// (6) failures against otherwise-valid, freshly created handles —
    /// this went away entirely once every SCM operation in this test
    /// binary was serialized. Real Windows' SCM may not have the same
    /// constraint, but serializing here costs nothing and removes the
    /// flakiness either way.
    #[tokio::test]
    async fn live_exercises_real_scm() {
        exercise(&AnyVfs::Direct(Direct::default())).await;
        exercise_cancellation(&AnyVfs::Direct(Direct::default())).await;

        let (client, _server) = connected_client().await;
        exercise(&AnyVfs::Client(client)).await;

        let (client, _server) = forced_remote_client().await;
        exercise(&AnyVfs::Client(client)).await;
    }
}
