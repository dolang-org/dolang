#![deny(warnings)]
#![cfg(unix)]
use dolang_shell_vfs::{Child, Command, TargetInfo, Utf8TypedPath, Utf8UnixPath, Vfs};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;
use tokio::time::timeout;

const AGENT_BIN: &str = env!("CARGO_BIN_EXE_dolang-shell-vfs");

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
    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(socket_path),
    )
    .await
    .expect("timeout connecting to daemon")
    .expect("failed to connect");
    client.stop().await.expect("stop should succeed");
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
        .arg(&socket_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn agent");

    wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(&socket_path),
    )
    .await
    .expect("timeout connecting to agent")
    .expect("failed to connect");

    let query = client.query().await.expect("query should succeed");
    assert!(!query.env.is_empty(), "should have environment");

    send_signal(child.id(), libc::SIGINT);
    let _ = child.wait().expect("failed to wait on agent");

    assert!(!socket_path.exists(), "socket should be cleaned up");
}

#[tokio::test]
async fn foreground_sigint() {
    let (_dir, socket_path) = find_free_socket_path();

    let mut child = std::process::Command::new(AGENT_BIN)
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
                dolang_shell_vfs::Client::connect(&socket_path),
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
async fn client_query() {
    let (_dir, socket_path) = find_free_socket_path();

    let mut child = std::process::Command::new(AGENT_BIN)
        .arg(&socket_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn agent");

    wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(&socket_path),
    )
    .await
    .expect("timeout connecting")
    .expect("failed to connect");

    let query = client.query().await.expect("query should succeed");

    assert!(!query.env.is_empty(), "env should not be empty");
    assert!(query.cwd.is_absolute(), "cwd should be absolute path");
    assert_eq!(query.target, TargetInfo::current());

    stop_daemon(&socket_path).await;
}

#[tokio::test]
async fn client_which() {
    let (_dir, socket_path) = find_free_socket_path();

    let mut child = std::process::Command::new(AGENT_BIN)
        .arg(&socket_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn agent");

    wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(&socket_path),
    )
    .await
    .expect("timeout connecting")
    .expect("failed to connect");

    let ls_path = client
        .which("ls", None, None)
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
        .arg(&socket_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn agent");

    wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(&socket_path),
    )
    .await
    .expect("timeout connecting")
    .expect("failed to connect");

    let env = HashMap::from([(String::from("HOME"), Some(String::from("/tmp/test-home")))]);
    let path = client
        .well_known_path(dolang_shell_vfs::WellKnownPath::HomeDir, &env)
        .await
        .expect("well-known path should succeed");

    assert_eq!(path, std::path::Path::new("/tmp/test-home"));

    stop_daemon(&socket_path).await;
}

#[tokio::test]
async fn client_stop() {
    let (_dir, socket_path) = find_free_socket_path();

    let mut child = std::process::Command::new(AGENT_BIN)
        .arg(&socket_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn agent");

    wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(&socket_path),
    )
    .await
    .expect("timeout connecting")
    .expect("failed to connect");

    client.stop().await.expect("stop should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let result = tokio::net::UnixStream::connect(&socket_path).await;
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
        .arg(&socket_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn agent");

    wait_for_ready_from_stdout(&mut child).expect("failed to read READY");

    let client = timeout(
        Duration::from_secs(5),
        dolang_shell_vfs::Client::connect(&socket_path),
    )
    .await
    .expect("timeout connecting")
    .expect("failed to connect");

    let query = client.query().await.expect("query should succeed");
    assert!(!query.env.is_empty(), "agent should be responsive");

    send_signal(child.id(), libc::SIGINT);
    let _ = child.wait().expect("failed to wait on agent");

    assert!(!socket_path.exists(), "socket should be cleaned up");
}

#[tokio::test]
async fn sigint_during_spawn() {
    let (_dir, socket_path) = find_free_socket_path();

    let mut child = std::process::Command::new(AGENT_BIN)
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
