#![deny(warnings)]

use std::process::Stdio;

use dolang_shell_vfs::pipe;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{Duration, Instant, sleep},
};

#[tokio::test]
async fn pipe_round_trip() {
    let (mut send, mut recv) = pipe().unwrap();

    send.write_all(b"hello world").await.unwrap();
    drop(send);

    let mut buf = Vec::new();
    recv.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"hello world");
}

#[tokio::test]
async fn pipe_reports_eof_after_sender_drop() {
    let (send, mut recv) = pipe().unwrap();
    drop(send);

    let mut buf = [0; 16];
    let n = recv.read(&mut buf).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn pipe_reports_write_failure_after_receiver_drop() {
    let (mut send, recv) = pipe().unwrap();
    let mut child = close_stdin_immediately(recv.into_stdio().unwrap());
    let status = child.wait().await.unwrap();
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(1);
    let err = loop {
        match send.write_all(b"hello").await {
            Ok(()) => {
                assert!(
                    Instant::now() < deadline,
                    "write unexpectedly continued succeeding after receiver exit",
                );
                sleep(Duration::from_millis(10)).await;
            }
            Err(err) => break err,
        }
    };
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionAborted
        ),
        "unexpected error kind: {:?}",
        err.kind()
    );
}

#[tokio::test]
async fn pipe_recv_can_be_used_as_child_stdin() {
    let (mut send, recv) = pipe().unwrap();
    let child = cat_stdin_to_stdout(recv.into_stdio().unwrap());

    #[cfg(unix)]
    send.write_all(b"hello from stdin").await.unwrap();
    #[cfg(windows)]
    send.write_all(b"hello from stdin\r\n").await.unwrap();
    drop(send);

    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    assert_eq!(normalize_text_output(&output.stdout), "hello from stdin");
}

#[tokio::test]
async fn pipe_send_can_be_used_as_child_stdout() {
    let (send, mut recv) = pipe().unwrap();
    let mut child = write_hello_to_stdout(send.into_stdio().unwrap());

    let mut buf = Vec::new();
    recv.read_to_end(&mut buf).await.unwrap();

    let status = child.wait().await.unwrap();
    assert!(status.success());
    assert_eq!(normalize_crlf(&buf), b"hello");
}

#[cfg(unix)]
fn cat_stdin_to_stdout(stdin: Stdio) -> tokio::process::Child {
    Command::new("sh")
        .args(["-c", "cat"])
        .stdin(stdin)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn cat_stdin_to_stdout(stdin: Stdio) -> tokio::process::Child {
    Command::new("cmd")
        .args(["/C", "sort"])
        .stdin(stdin)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn write_hello_to_stdout(stdout: Stdio) -> tokio::process::Child {
    Command::new("sh")
        .args(["-c", "printf hello"])
        .stdout(stdout)
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn write_hello_to_stdout(stdout: Stdio) -> tokio::process::Child {
    Command::new("cmd")
        .args(["/C", "echo hello"])
        .stdout(stdout)
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn close_stdin_immediately(stdin: Stdio) -> tokio::process::Child {
    Command::new("sh")
        .args(["-c", "exit 0"])
        .stdin(stdin)
        .spawn()
        .unwrap()
}

#[cfg(windows)]
fn close_stdin_immediately(stdin: Stdio) -> tokio::process::Child {
    Command::new("cmd")
        .args(["/C", "exit", "0"])
        .stdin(stdin)
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn normalize_crlf(buf: &[u8]) -> &[u8] {
    buf
}

#[cfg(windows)]
fn normalize_crlf(buf: &[u8]) -> &[u8] {
    buf.strip_suffix(b"\r\n").unwrap_or(buf)
}

#[cfg(unix)]
fn normalize_text_output(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf).into_owned()
}

#[cfg(windows)]
fn normalize_text_output(buf: &[u8]) -> String {
    String::from_utf8_lossy(normalize_crlf(buf)).into_owned()
}
