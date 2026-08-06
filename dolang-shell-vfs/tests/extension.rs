#![deny(warnings)]
#![cfg(unix)]

use dolang_vfs::{
    AnyVfs, Client, Direct, ExtContext, ExtOpaque, ExtResource, VfsExtension, vfs_extension,
};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;
use tokio::task::JoinHandle;

/// Marker type for a `Counter` retained through [`ExtContext::register`].
struct CounterMarker;

struct Counter(std::sync::atomic::AtomicI64);

impl ExtResource for Counter {
    type Marker = CounterMarker;
}

#[derive(Serialize, Deserialize)]
enum CounterRequest {
    Open(i64),
    Add(ExtOpaque<CounterMarker>, i64),
    Close(ExtOpaque<CounterMarker>),
}

#[derive(Serialize, Deserialize)]
enum CounterResponse {
    Handle(ExtOpaque<CounterMarker>),
    Value(i64),
    Closed,
}

struct CounterExt;

impl VfsExtension for CounterExt {
    type Request = CounterRequest;
    type Response = CounterResponse;

    const NAME: &'static str = "dolang-vfs-test-counter";
    const VERSION: u16 = 1;

    async fn handle(&self, ctx: &mut ExtContext<'_>, request: CounterRequest) -> CounterResponse {
        match request {
            CounterRequest::Open(initial) => {
                let handle = ctx.register(Counter(std::sync::atomic::AtomicI64::new(initial)));
                CounterResponse::Handle(handle)
            }
            CounterRequest::Add(handle, delta) => {
                let guard = ctx
                    .acquire::<Counter>(handle)
                    .expect("valid counter handle");
                let value = guard
                    .0
                    .fetch_add(delta, std::sync::atomic::Ordering::SeqCst)
                    + delta;
                CounterResponse::Value(value)
            }
            CounterRequest::Close(handle) => {
                ctx.unregister::<Counter>(handle)
                    .expect("valid counter handle");
                CounterResponse::Closed
            }
        }
    }
}

vfs_extension!(CounterExt);

async fn open(vfs: &AnyVfs, initial: i64) -> ExtOpaque<CounterMarker> {
    match vfs
        .call_extension::<CounterExt>(CounterRequest::Open(initial))
        .await
        .unwrap()
    {
        CounterResponse::Handle(handle) => handle,
        _ => panic!("expected Handle response"),
    }
}

async fn add(vfs: &AnyVfs, handle: ExtOpaque<CounterMarker>, delta: i64) -> i64 {
    match vfs
        .call_extension::<CounterExt>(CounterRequest::Add(handle, delta))
        .await
        .unwrap()
    {
        CounterResponse::Value(value) => value,
        _ => panic!("expected Value response"),
    }
}

async fn close(vfs: &AnyVfs, handle: ExtOpaque<CounterMarker>) {
    match vfs
        .call_extension::<CounterExt>(CounterRequest::Close(handle))
        .await
        .unwrap()
    {
        CounterResponse::Closed => {}
        _ => panic!("expected Closed response"),
    }
}

async fn exercise_counter(vfs: &AnyVfs) {
    let handle = open(vfs, 10).await;
    assert_eq!(add(vfs, handle.clone(), 5).await, 15);
    assert_eq!(add(vfs, handle.clone(), -3).await, 12);
    close(vfs, handle).await;
}

#[tokio::test]
async fn direct_dispatch_round_trips_through_opaque_handle() {
    exercise_counter(&AnyVfs::Direct(Direct::default())).await;
}

async fn start_server(socket_path: &std::path::Path) -> JoinHandle<()> {
    let path = socket_path.to_path_buf();
    let server = dolang_vfs::Server::bind(&path).await.unwrap();
    tokio::spawn(async move {
        let _ = server.accept().await;
    })
}

#[tokio::test]
async fn remote_dispatch_round_trips_through_opaque_handle() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("vfs.sock");
    let _server = start_server(&socket_path).await;
    let client = Client::connect(&socket_path).await.unwrap();
    exercise_counter(&AnyVfs::Client(client)).await;
}

#[tokio::test]
async fn remote_dispatch_rejects_unknown_extension_version() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("vfs.sock");
    let _server = start_server(&socket_path).await;
    let client = Client::connect(&socket_path).await.unwrap();
    // A version this test never registered should surface as a clean error,
    // not a panic or hang, whether the failure is caught client-side (no
    // registered extension to encode the request against) or server-side
    // (registered on the client but not found by the peer).
    struct UnknownVersionExt;
    impl VfsExtension for UnknownVersionExt {
        type Request = CounterRequest;
        type Response = CounterResponse;
        const NAME: &'static str = "dolang-vfs-test-counter";
        const VERSION: u16 = 2;
        async fn handle(
            &self,
            _ctx: &mut ExtContext<'_>,
            _request: CounterRequest,
        ) -> CounterResponse {
            unreachable!("never registered, so never dispatched")
        }
    }
    let result = client
        .call_extension::<UnknownVersionExt>(CounterRequest::Open(0))
        .await;
    assert!(result.is_err());
}
