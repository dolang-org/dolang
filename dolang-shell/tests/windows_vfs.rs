#![cfg(windows)]
#![deny(warnings)]

use std::{
    os::windows::io::AsHandle,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use dolang_shell_vfs::{Client, Vfs as _};
use tokio::net::windows::named_pipe::ServerOptions;

static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn embedded_vfs_mode_serves_and_stops() {
    let id = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
    let pipe_name = format!(r"\\.\pipe\dolang-shell-test-{}-{id}", std::process::id());
    let pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&pipe_name)
        .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_dolang"))
        .arg("--vfs")
        .arg(&pipe_name)
        .spawn()
        .unwrap();
    let process = child.as_handle().try_clone_to_owned().unwrap();
    pipe.connect().await.unwrap();

    let client = unsafe { Client::from_named_pipe_server(pipe, process).unwrap() };
    let query = client.query().await.unwrap();
    assert_eq!(query.cwd, std::env::current_dir().unwrap());

    let metadata = client
        .metadata(std::env::current_exe().unwrap())
        .await
        .unwrap();
    assert!(metadata.len > 0);

    client.stop().await.unwrap();
    assert!(child.wait().unwrap().success());
}
