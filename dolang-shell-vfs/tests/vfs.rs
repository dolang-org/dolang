#![deny(warnings)]

use std::time::Duration;

use dolang_vfs::{Vfs, security::SecurityInfo, target::TargetInfo};
use std::collections::HashMap;
use tempfile::tempdir;
use tokio::time::timeout;
use typed_path::Utf8TypedPathBuf;

const AGENT_BIN: &str = env!("CARGO_BIN_EXE_dolang-vfs");

/// Spawns the VFS helper in `--stdio` mode with the given extra arguments and
/// connects a client over its stdio pipes. `without_env` names variables to
/// strip from the child's environment before it starts, so a value the query
/// later reports must have come from the helper itself rather than simple
/// inheritance.
///
/// `--stdio` is available on every platform the helper supports (unlike
/// `--listen`/`--connect`, which are platform-specific), so it is the
/// transport shared by every test below that does not specifically exercise
/// one of those other modes.
async fn spawn_stdio(
    args: &[std::ffi::OsString],
    without_env: &[&str],
) -> (Vfs, tokio::process::Child) {
    let mut command = tokio::process::Command::new(AGENT_BIN);
    command
        .args(args)
        .arg("--stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    for name in without_env {
        command.env_remove(name);
    }
    let mut child = command.spawn().expect("failed to spawn agent");
    let stdout = child.stdout.take().expect("stdout not captured");
    let stdin = child.stdin.take().expect("stdin not captured");
    let client = Vfs::new_split(stdout, stdin)
        .await
        .expect("negotiation should succeed");
    (client, child)
}

/// Spawns the helper, reads its initial snapshot, then stops it and waits for
/// exit. Used by tests that only care about the resulting initial snapshot. A
/// single stray byte of profile or startup chatter on the helper's stdout
/// would desynchronize the frame stream and fail the query, so this also
/// covers stdio-stream cleanliness for every caller.
struct Snapshot {
    env: HashMap<String, String>,
    cwd: Utf8TypedPathBuf,
    current_exe: Utf8TypedPathBuf,
    target: TargetInfo,
    security: SecurityInfo,
}

async fn spawn_and_query(args: &[std::ffi::OsString]) -> Snapshot {
    spawn_and_query_without(args, &[]).await
}

async fn spawn_and_query_without(args: &[std::ffi::OsString], without_env: &[&str]) -> Snapshot {
    let (client, mut child) = spawn_stdio(args, without_env).await;
    let query = Snapshot {
        env: client.env().collect(),
        cwd: client.cwd().to_path_buf(),
        current_exe: client.current_exe().to_path_buf(),
        target: client.target().clone(),
        security: client.security().clone(),
    };
    client.stop().await.expect("stop should succeed");
    client.close().await;
    let _ = child.wait().await;
    query
}

#[tokio::test]
async fn client_query() {
    let query = spawn_and_query(&[]).await;

    assert!(!query.env.is_empty(), "env should not be empty");
    assert!(query.cwd.is_absolute(), "cwd should be absolute path");
    assert!(
        query.current_exe.is_absolute(),
        "current executable should be absolute"
    );
    assert_eq!(query.target, TargetInfo::current());
    assert_eq!(query.security, SecurityInfo::current().unwrap());
}

#[tokio::test]
async fn cd_flag_changes_query_cwd() {
    let target_dir = tempdir().unwrap();
    let target_path = std::fs::canonicalize(target_dir.path()).unwrap();

    let query = spawn_and_query(&[
        std::ffi::OsString::from("--cd"),
        target_path.clone().into_os_string(),
    ])
    .await;

    // Compare canonicalized forms on both sides: Windows reports the current
    // directory without the `\\?\` extended-length prefix that
    // `canonicalize` adds, so `query.cwd` and `target_path` would otherwise
    // disagree only in that prefix.
    assert_eq!(
        std::fs::canonicalize(query.cwd.as_str()).unwrap(),
        target_path,
        "cwd should match --cd argument"
    );
}

#[tokio::test]
async fn set_flag_adds_env_var() {
    let query = spawn_and_query(&[
        std::ffi::OsString::from("--set"),
        std::ffi::OsString::from("VFS_TEST_SECRET=hello123"),
    ])
    .await;

    assert_eq!(
        query.env.get("VFS_TEST_SECRET").map(String::as_str),
        Some("hello123"),
        "env should contain the value set via --set"
    );
}

#[tokio::test]
async fn set_flag_overwrites_existing_env() {
    let query = spawn_and_query(&[
        std::ffi::OsString::from("--set"),
        std::ffi::OsString::from("PATH=/custom/bin:/custom/sbin"),
    ])
    .await;

    assert_eq!(
        query.env.get("PATH").map(String::as_str),
        Some("/custom/bin:/custom/sbin"),
        "PATH should be overwritten by --set"
    );
}

#[tokio::test]
async fn unset_flag_removes_env_var() {
    let query = spawn_and_query(&[
        std::ffi::OsString::from("--set"),
        std::ffi::OsString::from("VFS_UNSET_TARGET=should_vanish"),
        std::ffi::OsString::from("--unset"),
        std::ffi::OsString::from("VFS_UNSET_TARGET"),
    ])
    .await;

    assert!(
        !query.env.contains_key("VFS_UNSET_TARGET"),
        "variable set then unset should not appear in env"
    );
}

#[tokio::test]
async fn combined_set_unset_cwd() {
    let target_dir = tempdir().unwrap();
    let target_path = std::fs::canonicalize(target_dir.path()).unwrap();

    let query = spawn_and_query(&[
        std::ffi::OsString::from("--set"),
        std::ffi::OsString::from("VFS_COMBO_A=alpha"),
        std::ffi::OsString::from("--set"),
        std::ffi::OsString::from("VFS_COMBO_B=beta"),
        std::ffi::OsString::from("--unset"),
        std::ffi::OsString::from("VFS_COMBO_B"),
        std::ffi::OsString::from("--cd"),
        target_path.clone().into_os_string(),
    ])
    .await;

    assert_eq!(
        query.env.get("VFS_COMBO_A").map(String::as_str),
        Some("alpha"),
        "VFS_COMBO_A should be set"
    );
    assert!(
        !query.env.contains_key("VFS_COMBO_B"),
        "VFS_COMBO_B should be unset"
    );
    // See `cd_flag_changes_query_cwd` for why both sides are canonicalized.
    assert_eq!(
        std::fs::canonicalize(query.cwd.as_str()).unwrap(),
        target_path,
        "cwd should match --cd"
    );
}

#[tokio::test]
async fn base64_option_forms_carry_awkward_values() {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let value = r#"a "quoted" $dollar %percent\ back\slash"#;
    let dir = tempdir().unwrap();
    let cwd = dir.path().join("a dir with spaces");
    std::fs::create_dir(&cwd).unwrap();

    let query = spawn_and_query(&[
        std::ffi::OsString::from("--set-base64"),
        STANDARD.encode(format!("AWKWARD={value}")).into(),
        std::ffi::OsString::from("--cd-base64"),
        STANDARD.encode(cwd.to_str().unwrap()).into(),
    ])
    .await;

    assert_eq!(query.env.get("AWKWARD").map(String::as_str), Some(value));
    assert_eq!(
        std::fs::canonicalize(query.cwd.as_str()).unwrap(),
        std::fs::canonicalize(&cwd).unwrap()
    );
}

#[tokio::test]
async fn base64_option_forms_reject_undecodable_values() {
    let output = std::process::Command::new(AGENT_BIN)
        .arg("--set-base64")
        .arg("not base64!")
        .arg("--stdio")
        .output()
        .expect("failed to run agent");

    assert!(!output.status.success(), "agent should refuse to start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--set-base64") && stderr.contains("base64"),
        "error should name the option and the problem: {stderr}"
    );
}

#[cfg(all(feature = "winreg", feature = "winscm"))]
#[tokio::test]
async fn stock_binary_registers_vfs_extensions() {
    use dolang_vfs_winreg::{Access, Key, PredefinedRoot, View};
    use dolang_vfs_winscm::{ScManager, ServiceAccess};

    let (client, mut child) = spawn_stdio(&[], &[]).await;
    let vfs = client.clone();

    #[cfg(unix)]
    {
        use dolang_vfs::error::ErrorKind;

        let winreg_error = Key::open_root(
            &vfs,
            PredefinedRoot::CurrentUser,
            View::Native,
            Access::READ,
        )
        .await
        .err()
        .expect("registry extension unexpectedly succeeded on Unix");
        assert_eq!(winreg_error.kind(), ErrorKind::Unsupported);

        let winscm_error = ScManager::open(&vfs, ServiceAccess::SC_MANAGER_CONNECT)
            .await
            .err()
            .expect("SCM extension unexpectedly succeeded on Unix");
        assert_eq!(winscm_error.kind(), ErrorKind::Unsupported);
    }
    #[cfg(windows)]
    {
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
    }

    client.stop().await.unwrap();
    // `vfs` owns another client clone, including the child's piped
    // stdin/stdout handles. Drop it, then explicitly close the shared client
    // before waiting so the server observes transport EOF.
    drop(vfs);
    client.close().await;
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("VFS helper did not exit after stop")
        .unwrap();
    assert!(status.success());
}

/// Tests below this point are platform-specific: they exercise `--listen`
/// (Unix domain sockets, signal handling) or Windows login-environment
/// import, neither of which has a cross-platform equivalent to share.
#[cfg(unix)]
mod listen_mode {
    use std::{
        io::{BufRead, BufReader},
        os::unix::fs::PermissionsExt,
        path::Path,
        time::Duration,
    };

    use dolang_vfs::Vfs;
    use tempfile::tempdir;
    use tokio::time::timeout;
    use typed_path::{Utf8TypedPath, Utf8UnixPath};

    use super::AGENT_BIN;

    fn typed_str(path: &str) -> Utf8TypedPath<'_> {
        Utf8TypedPath::Unix(Utf8UnixPath::new(path))
    }

    fn find_free_socket_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&dir, perms).unwrap();
        let socket_path = dir.path().join("test.sock");
        (dir, socket_path)
    }

    fn send_signal(pid: u32, signal: libc::c_int) {
        let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
        if result != 0 {
            panic!("failed to send signal: {}", std::io::Error::last_os_error());
        }
    }

    async fn stop_daemon(socket_path: &Path) {
        let client = timeout(Duration::from_secs(5), Vfs::connect(socket_path))
            .await
            .expect("timeout connecting to daemon")
            .expect("failed to connect");
        client.stop().await.expect("stop should succeed");
        client.close().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fn wait_for_ready_from_stdout(child: &mut std::process::Child) -> std::io::Result<()> {
        let stdout = child.stdout.take().expect("stdout not captured");
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            let line = line.map_err(std::io::Error::other)?;
            if line == "READY" {
                return Ok(());
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "process exited before READY",
        ))
    }

    #[tokio::test]
    async fn foreground_spawn_echo() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        let client = timeout(Duration::from_secs(5), Vfs::connect(&socket_path))
            .await
            .expect("timeout connecting to agent")
            .expect("failed to connect");

        assert!(client.env().next().is_some(), "should have environment");

        send_signal(child.id(), libc::SIGINT);
        let _ = child.wait().expect("failed to wait on agent");

        assert!(!socket_path.exists(), "socket should be cleaned up");
    }

    #[tokio::test]
    async fn foreground_sigint() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        tokio::time::sleep(Duration::from_millis(100)).await;

        send_signal(child.id(), libc::SIGINT);

        let status = child.wait().expect("failed to wait on agent");
        assert_eq!(
            status.code(),
            Some(0),
            "agent should exit 0 on handled SIGINT"
        );
        assert!(!socket_path.exists(), "socket should be cleaned up");
    }

    #[tokio::test]
    async fn foreground_sigterm() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        tokio::time::sleep(Duration::from_millis(100)).await;

        send_signal(child.id(), libc::SIGTERM);

        let status = child.wait().expect("failed to wait on agent");
        assert_eq!(
            status.code(),
            Some(0),
            "agent should exit 0 on handled SIGTERM"
        );
        assert!(!socket_path.exists(), "socket should be cleaned up");
    }

    #[tokio::test]
    async fn foreground_socket_cleanup() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        send_signal(child.id(), libc::SIGINT);
        let _ = child.wait().expect("failed to wait on agent");

        assert!(
            !socket_path.exists(),
            "socket file should be removed after exit"
        );
    }

    #[tokio::test]
    async fn multiple_clients() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        let num_clients = 5;
        let mut futures = Vec::new();

        for _ in 0..num_clients {
            let socket_path = socket_path.clone();
            futures.push(async move {
                let client = timeout(
                    Duration::from_secs(5),
                    dolang_vfs::Vfs::connect(&socket_path),
                )
                .await
                .expect("timeout connecting")
                .expect("failed to connect");

                let cmd = client.command(typed_str("true"));
                let mut child = cmd.spawn().await.expect("failed to spawn");
                let status = child.wait().await.expect("failed to get status");

                assert!(status.success());
                status
            });
        }

        let results = futures::future::join_all(futures).await;

        assert_eq!(results.len(), num_clients);
        for (i, status) in results.into_iter().enumerate() {
            assert!(status.success(), "client {} should succeed", i);
        }

        stop_daemon(&socket_path).await;
    }

    #[tokio::test]
    async fn client_which() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        let client = timeout(
            Duration::from_secs(5),
            dolang_vfs::Vfs::connect(&socket_path),
        )
        .await
        .expect("timeout connecting")
        .expect("failed to connect");

        let ls_path = client
            .which(typed_str("ls"), None, None)
            .await
            .expect("which should succeed");

        assert!(ls_path.is_some(), "ls should be found");
        let path = ls_path.unwrap();
        assert!(path.ends_with("ls"), "path should end with ls");
        stop_daemon(&socket_path).await;
    }

    #[tokio::test]
    async fn client_well_known_path() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        let client = timeout(
            Duration::from_secs(5),
            dolang_vfs::Vfs::connect(&socket_path),
        )
        .await
        .expect("timeout connecting")
        .expect("failed to connect");

        let env = std::collections::HashMap::from([(
            String::from("HOME"),
            Some(String::from("/tmp/test-home")),
        )]);
        let path = client
            .well_known_path(dolang_vfs::path::WellKnownPath::HomeDir, None, &env)
            .await
            .expect("well-known path should succeed");

        assert_eq!(path, "/tmp/test-home");

        stop_daemon(&socket_path).await;
    }

    #[tokio::test]
    async fn client_stop() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        let client = timeout(
            Duration::from_secs(5),
            dolang_vfs::Vfs::connect(&socket_path),
        )
        .await
        .expect("timeout connecting")
        .expect("failed to connect");

        client.stop().await.expect("stop should succeed");

        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = tokio::net::UnixStream::connect(&socket_path).await;
        client.close().await;
        assert!(
            result.is_err(),
            "socket should no longer accept connections"
        );
    }

    #[tokio::test]
    async fn stale_socket_removed() {
        let (_dir, socket_path) = find_free_socket_path();

        std::fs::write(&socket_path, "stale socket").expect("failed to create stale socket");
        assert!(socket_path.exists(), "stale socket should exist");

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        let client = timeout(
            Duration::from_secs(5),
            dolang_vfs::Vfs::connect(&socket_path),
        )
        .await
        .expect("timeout connecting")
        .expect("failed to connect");

        assert!(client.env().next().is_some(), "agent should be responsive");

        send_signal(child.id(), libc::SIGINT);
        let _ = child.wait().expect("failed to wait on agent");

        assert!(!socket_path.exists(), "socket should be cleaned up");
    }

    #[tokio::test]
    async fn sigint_during_spawn() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        tokio::time::sleep(Duration::from_millis(100)).await;

        send_signal(child.id(), libc::SIGINT);
        let _ = child.wait().expect("failed to wait on agent");

        assert!(!socket_path.exists(), "socket should be cleaned up");
    }

    #[tokio::test]
    async fn sigterm_during_spawn() {
        let (_dir, socket_path) = find_free_socket_path();

        let mut child = std::process::Command::new(AGENT_BIN)
            .arg("--listen")
            .arg(&socket_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

        tokio::time::sleep(Duration::from_millis(100)).await;

        send_signal(child.id(), libc::SIGTERM);
        let _ = child.wait().expect("failed to wait on agent");

        assert!(!socket_path.exists(), "socket should be cleaned up");
    }
}

#[cfg(unix)]
mod login_env {
    use std::path::Path;

    use tempfile::tempdir;

    use super::spawn_and_query;

    /// Write a stand-in login shell that emits profile chatter on both
    /// streams, exports a few variables, leaks the snapshot descriptor into a
    /// background daemon, and then runs its `-c` command.
    ///
    /// The daemon models the profile that starts an agent without closing
    /// descriptors it does not own: it holds the write end of the snapshot
    /// pipe open long after the probe exits, so an import that treats
    /// end-of-file as the completion signal hangs here rather than
    /// returning. Its pid lands in [`daemon_pid_path`] so the cleanup can be
    /// checked too.
    fn fake_login_shell(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("login-shell");
        std::fs::write(
            &path,
            concat!(
                "#!/bin/sh\n",
                "echo 'profile noise on stdout'\n",
                "echo 'profile noise on stderr' >&2\n",
                "FROM_PROFILE=hello; export FROM_PROFILE\n",
                "WEIRD=$(printf 'a\\377b'); export WEIRD\n",
                "sleep 30 &\n",
                "echo $! > \"$0.daemon\"\n",
                "exec /bin/sh \"$@\"\n",
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Path [`fake_login_shell`] records its background daemon's pid in.
    fn daemon_pid_path(shell: &Path) -> std::path::PathBuf {
        let mut path = shell.as_os_str().to_owned();
        path.push(".daemon");
        path.into()
    }

    /// Write a stand-in login shell that fails before running its command.
    fn failing_login_shell(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("broken-shell");
        std::fs::write(&path, "#!/bin/sh\necho 'profile is broken' >&2\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn login_env_imports_profile_vars() {
        let dir = tempdir().unwrap();
        let shell = fake_login_shell(dir.path());

        let mut flag = std::ffi::OsString::from("--login-env=");
        flag.push(&shell);
        let query = spawn_and_query(&[flag]).await;

        assert_eq!(
            query.env.get("FROM_PROFILE").map(String::as_str),
            Some("hello"),
            "profile-exported variable should be imported"
        );
        // The probe imports the raw bytes, but a snapshot cannot represent them.
        assert!(
            !query.env.contains_key("WEIRD"),
            "non-UTF-8 value should be omitted from the query rather than panicking"
        );
    }

    #[tokio::test]
    async fn login_env_kills_processes_started_by_the_profile() {
        let dir = tempdir().unwrap();
        let shell = fake_login_shell(dir.path());

        let mut flag = std::ffi::OsString::from("--login-env=");
        flag.push(&shell);
        // Completing at all proves the leaked descriptor did not stall the import.
        let query = spawn_and_query(&[flag]).await;
        assert!(query.env.contains_key("FROM_PROFILE"));

        let pid: i32 = std::fs::read_to_string(daemon_pid_path(&shell))
            .expect("profile daemon should have recorded its pid")
            .trim()
            .parse()
            .expect("pid should be numeric");

        // The daemon is not our child, so it cannot be reaped here; signal 0 just
        // reports whether the pid is still live.
        for attempt in 0.. {
            // SAFETY: signal 0 performs permission checks only.
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            assert!(attempt < 50, "profile daemon {pid} outlived the probe");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn login_env_is_off_by_default() {
        let dir = tempdir().unwrap();
        let _shell = fake_login_shell(dir.path());

        let query = spawn_and_query(&[]).await;

        assert!(
            !query.env.contains_key("FROM_PROFILE"),
            "login environment should not be probed without --login-env"
        );
    }

    #[tokio::test]
    async fn set_flag_overrides_login_env() {
        let dir = tempdir().unwrap();
        let shell = fake_login_shell(dir.path());

        let mut flag = std::ffi::OsString::from("--login-env=");
        flag.push(&shell);
        let query = spawn_and_query(&[
            flag,
            std::ffi::OsString::from("--set"),
            std::ffi::OsString::from("FROM_PROFILE=override"),
        ])
        .await;

        assert_eq!(
            query.env.get("FROM_PROFILE").map(String::as_str),
            Some("override"),
            "explicit --set should win over the imported login environment"
        );
    }

    #[tokio::test]
    async fn login_env_probe_failure_reports_flag() {
        let dir = tempdir().unwrap();
        let shell = failing_login_shell(dir.path());

        let mut flag = std::ffi::OsString::from("--login-env=");
        flag.push(&shell);

        let output = std::process::Command::new(super::AGENT_BIN)
            .arg(&flag)
            .arg("--stdio")
            .output()
            .expect("failed to run agent");

        assert!(!output.status.success(), "agent should fail to start");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--login-env"),
            "error should name the flag to drop: {stderr}"
        );
        assert!(
            stderr.contains("profile is broken"),
            "shell diagnostics should reach stderr: {stderr}"
        );
    }
}

#[cfg(windows)]
mod login_env {
    use super::spawn_and_query_without;

    /// The Windows OpenSSH server populates only `PATH` from the registry and
    /// leaves the rest of the environment pointing at the service account, so
    /// `--login-env` reads the user environment out of the registry itself.
    ///
    /// The child is started without `TEMP` and `USERPROFILE`, which a logon
    /// always defines: `TEMP` lives in `HKCU\Environment`, and `USERPROFILE`
    /// is resolved from the process token's profile folder. Recovering them
    /// proves the import ran rather than the values simply being inherited.
    #[tokio::test]
    async fn login_env_imports_user_environment() {
        const REMOVED: &[&str] = &["TEMP", "USERPROFILE"];

        let imported =
            spawn_and_query_without(&[std::ffi::OsString::from("--login-env")], REMOVED).await;
        let inherited = spawn_and_query_without(&[], REMOVED).await;

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
}

/// `--accept` serves exactly one authenticated client and then unlinks its
/// socket. These tests drive the real binary because the interesting behavior
/// -- the key arriving on stdin before anything binds, and the socket
/// disappearing at the right moment -- lives in the CLI, not the library.
#[cfg(unix)]
mod accept_mode {
    use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};

    use dolang_rpc::auth::AuthKey;
    use dolang_vfs::Vfs;
    use tempfile::tempdir;
    use tokio::time::timeout;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::{Child, Command},
    };

    use super::AGENT_BIN;

    const TEST_KEY: &[u8] = b"a-sufficiently-long-test-key";

    fn find_free_socket_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&dir, perms).unwrap();
        let socket_path = dir.path().join("test.sock");
        (dir, socket_path)
    }

    /// Spawns the agent in `--accept --key-stdin` mode and returns once it has
    /// bound its socket and printed `READY`.
    async fn spawn_accept(socket_path: &Path, key: &[u8]) -> Child {
        let mut child = Command::new(AGENT_BIN)
            .arg("--key-stdin")
            .arg("--accept")
            .arg(socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to spawn agent");

        // Length-prefixed, and written before READY: the agent reads the key
        // before it binds anything.
        let mut stdin = child.stdin.take().expect("stdin not captured");
        stdin
            .write_all(&[u8::try_from(key.len()).unwrap()])
            .await
            .unwrap();
        stdin.write_all(key).await.unwrap();
        stdin.flush().await.unwrap();
        drop(stdin);

        wait_for_ready(&mut child)
            .await
            .expect("agent did not become ready");
        child
    }

    async fn wait_for_ready(child: &mut Child) -> std::io::Result<()> {
        let stdout = child.stdout.take().expect("stdout not captured");
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if line == "READY" {
                return Ok(());
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "process exited before READY",
        ))
    }

    async fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
        timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("timeout waiting for agent to exit")
            .expect("wait")
    }

    async fn connect(socket_path: &Path, key: &[u8]) -> Result<Vfs, dolang_vfs::error::Error> {
        timeout(
            Duration::from_secs(5),
            Vfs::connect_with_key(socket_path, Some(AuthKey::new(key).unwrap())),
        )
        .await
        .expect("timeout connecting to agent")
    }

    #[tokio::test]
    async fn accepts_one_authenticated_client_and_unlinks_the_socket() {
        let (_dir, socket_path) = find_free_socket_path();
        let mut child = spawn_accept(&socket_path, TEST_KEY).await;

        let client = connect(&socket_path, TEST_KEY).await.expect("connect");

        // The socket is unlinked as soon as the session is established, so
        // nothing else can reach the agent for the rest of its life.
        for _ in 0..50 {
            if !socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !socket_path.exists(),
            "socket should be unlinked once a client is accepted"
        );

        client.stop().await.expect("stop");
        client.close().await;
        let status = wait_for_exit(&mut child).await;
        assert!(status.success(), "agent exited with {status}");
    }

    #[tokio::test]
    async fn a_wrong_key_neither_consumes_the_slot_nor_unlinks_the_socket() {
        let (_dir, socket_path) = find_free_socket_path();
        let mut child = spawn_accept(&socket_path, TEST_KEY).await;

        let result = connect(&socket_path, b"an-entirely-different-key").await;
        assert!(result.is_err(), "a client with the wrong key was accepted");
        assert!(
            socket_path.exists(),
            "a failed attempt must leave the socket in place"
        );

        // Losing the race to an impostor costs the real client an attempt, not
        // its session.
        let client = connect(&socket_path, TEST_KEY)
            .await
            .expect("the intended client should still be served");
        client.stop().await.expect("stop");
        client.close().await;
        let status = wait_for_exit(&mut child).await;
        assert!(status.success(), "agent exited with {status}");
    }

    #[tokio::test]
    async fn a_silent_connection_does_not_block_the_intended_client() {
        let (_dir, socket_path) = find_free_socket_path();
        let mut child = spawn_accept(&socket_path, TEST_KEY).await;

        // Connect and then say nothing at all, which is what a peer trying to
        // wedge the accept loop would do.
        let _silent = std::os::unix::net::UnixStream::connect(&socket_path).unwrap();

        let client = connect(&socket_path, TEST_KEY)
            .await
            .expect("a stalled peer must not block the accept loop");
        client.stop().await.expect("stop");
        client.close().await;
        let status = wait_for_exit(&mut child).await;
        assert!(status.success(), "agent exited with {status}");
    }

    #[tokio::test]
    async fn key_stdin_is_refused_with_stdio() {
        let output = Command::new(AGENT_BIN)
            .arg("--key-stdin")
            .arg("--stdio")
            .output()
            .await
            .expect("failed to spawn agent");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--key-stdin cannot be combined with --stdio"),
            "unexpected stderr: {stderr}"
        );
    }
}
