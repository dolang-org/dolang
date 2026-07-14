#![deny(warnings)]

use std::io;

use dolang_shell_vfs::{Client, Command, FileType, OpenOptions, Server, Vfs, typed_path};
use tempfile::tempdir;

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

    let options = client.open_options();
    let error = OpenOptions::open(&options, path.to_path())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    let error = client.read_dir(path.to_path()).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    let error = client.command(path.to_path()).spawn().await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);

    client.stop().await.unwrap();
    server_task.await.unwrap().unwrap();
}
