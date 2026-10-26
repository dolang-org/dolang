#![cfg(unix)]

use std::{
    io,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::task::JoinHandle;

use crate::{
    Vfs,
    process::{ChildIn, ChildOut},
    test_temp::tempdir,
};

/// Fails every read or write.
struct Broken;

impl AsyncRead for Broken {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("broken stream")))
    }
}

impl AsyncWrite for Broken {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("broken stream")))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn start_server(socket_path: &Path) -> JoinHandle<()> {
    let server = crate::server::Server::bind(socket_path).await.unwrap();
    tokio::spawn(async move {
        let _ = server.accept().await;
    })
}

/// Runs `script` under `sh` in `vfs`, with the given streams, and returns
/// whether it succeeded and the relay error.
async fn run(
    vfs: &Vfs,
    script: &str,
    stdin: Option<ChildIn>,
    stdout: Option<ChildOut>,
) -> (bool, Option<io::Error>) {
    let mut command = vfs.command(crate::path::Path::unix("sh"));
    command.arg("-c").arg(script);
    if let Some(stdin) = stdin {
        command.stdin(stdin).unwrap();
    }
    if let Some(stdout) = stdout {
        command.stdout(stdout).unwrap();
    }
    let mut child = command.spawn().await.unwrap();
    let status = child.wait().await.unwrap();
    (status.success(), child.relay_error())
}

async fn check_streams(vfs: &Vfs) {
    // A reader feeds the child and a writer takes its output.
    let (writer, mut output) = super::duplex(1024);
    let (success, error) = run(
        vfs,
        "cat",
        Some(ChildIn::reader(&b"through a relay"[..])),
        Some(ChildOut::writer(writer)),
    )
    .await;
    assert!(success);
    assert!(error.is_none());
    let mut buf = Vec::new();
    output.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"through a relay");

    // A failing reader is reported, though the child saw only an early end.
    let (success, error) = run(vfs, "cat >/dev/null", Some(ChildIn::reader(Broken)), None).await;
    assert!(success);
    assert_eq!(error.unwrap().to_string(), "broken stream");

    // So is a failing writer.
    let (success, error) = run(vfs, "printf hi", None, Some(ChildOut::writer(Broken))).await;
    assert!(success);
    assert_eq!(error.unwrap().to_string(), "broken stream");

    // A child may exit without reading all its input.
    let (success, error) = run(
        vfs,
        "head -c1 >/dev/null",
        Some(ChildIn::reader(futures::io::repeat(b'x'))),
        None,
    )
    .await;
    assert!(success);
    assert!(error.is_none());
}

#[tokio::test]
async fn direct_child_relays_streams() {
    check_streams(&Vfs::direct().unwrap()).await;
}

#[tokio::test]
async fn remote_child_relays_streams() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("test.sock");
    let server = start_server(&socket_path).await;
    let client = Vfs::connect(&socket_path).await.unwrap();
    check_streams(&client).await;
    client.stop().await.unwrap();
    server.await.unwrap();
}
