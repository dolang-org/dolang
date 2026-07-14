#![deny(warnings)]

use std::io::{self, SeekFrom};

#[cfg(target_os = "linux")]
use dolang_shell_vfs::XattrNamespace;
use dolang_shell_vfs::{
    Client, Command, FileHandle, FileType, OpenOptions, Server, Vfs, typed_path,
};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

async fn connected_pair() -> (Client, tokio::task::JoinHandle<io::Result<()>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    let server = Server::new(server_stream);
    let task = tokio::spawn(server.serve());
    (Client::new(client_stream), task)
}

#[tokio::test]
async fn path_operations_work_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let query = client.query().await.unwrap();
    assert_eq!(query.target, dolang_shell_vfs::TargetInfo::current());

    let temp = tempdir().unwrap();
    let first = typed_path(temp.path().join("first")).unwrap();
    let second = typed_path(temp.path().join("second")).unwrap();

    client.create_dir(first.to_path(), false).await.unwrap();
    assert_eq!(
        client.metadata(first.to_path()).await.unwrap().file_type,
        FileType::Dir
    );
    client
        .rename(first.to_path(), second.to_path())
        .await
        .unwrap();
    assert!(
        client
            .canonicalize(second.to_path())
            .await
            .unwrap()
            .is_absolute()
    );
    client
        .remove_dir(second.to_path(), false, false)
        .await
        .unwrap();

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn handle_operations_are_unsupported_remotely() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("file")).unwrap();

    let error = client.read_dir(path.to_path()).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    let error = client.command(path.to_path()).spawn().await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn regular_file_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("file")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = OpenOptions::open(&options, path.to_path()).await.unwrap();

    file.write_all(b"abcdef").await.unwrap();
    file.flush().await.unwrap();
    assert_eq!(file.metadata().await.unwrap().len, 6);
    assert!(file.fs_metadata().await.unwrap().capacity > 0);

    let mut clone = file.try_clone().await.unwrap();
    assert_eq!(clone.seek(SeekFrom::Current(0)).await.unwrap(), 6);
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    assert_eq!(clone.seek(SeekFrom::Current(0)).await.unwrap(), 0);
    let mut data = Vec::new();
    clone.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abcdef");

    let mut file = file.try_into_std().await.unwrap_err();
    assert_eq!(file.metadata().await.unwrap().len, 6);

    file.set_len(3).await.unwrap();
    assert_eq!(file.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    data.clear();
    file.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abc");
    file.close().await.unwrap();

    assert_eq!(clone.seek(SeekFrom::Start(0)).await.unwrap(), 0);
    data.clear();
    clone.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"abc");
    clone.close().await.unwrap();

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn regular_file_xattrs_round_trip_over_generic_stream() {
    let (client, server_task) = connected_pair().await;
    let temp = tempdir().unwrap();
    let path = typed_path(temp.path().join("file")).unwrap();

    let mut options = client.open_options();
    options.read(true).write(true).create(true).truncate(true);
    let mut file = OpenOptions::open(&options, path.to_path()).await.unwrap();

    file.set_xattr("remote", Some("user"), b"value")
        .await
        .unwrap();
    assert_eq!(file.xattr("remote", Some("user")).await.unwrap(), b"value");
    assert!(
        file.xattrs(XattrNamespace::Any)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.name == "remote" && entry.namespace.as_deref() == Some("user"))
    );
    file.remove_xattr("remote", Some("user")).await.unwrap();
    assert!(file.xattr("remote", Some("user")).await.is_err());
    file.close().await.unwrap();

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}
