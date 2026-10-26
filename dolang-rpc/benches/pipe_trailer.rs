//! Benchmarks an RPC round trip over native OS pipes — the same transport
//! shape as a subprocess talking to its parent over stdin/stdout — with a
//! small request, a small response, and a large response trailer. Exercises
//! the default 512KiB fragment size against the 1MB pipe buffer size
//! `dolang-ext-shell` configures for its remote-VFS subprocess transport
//! (`REMOTE_VFS_PIPE_BUFFER_SIZE` in `dolang-ext-shell/src/shell.rs`).
//!
//! See `pipe_raw` for a comparison benchmark that does the same pipe I/O
//! without any RPC framing, to isolate the framing overhead.
//!
//! Unix and Windows only (native anonymous pipes / named-pipe-backed
//! anonymous pipes respectively); a no-op stub keeps the bench target
//! building elsewhere. Run with `cargo bench -p dolang-rpc`.

mod support;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dolang_rpc::{Builder, Protocol, client::Client, server::CallContext};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use serde::{Deserialize, Serialize};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use support::native_pipe;

const APP_PROTOCOL: (&str, &[u16]) = ("bench", &[1]);
const FRAGMENT_SIZE: usize = 512 * 1024;

struct BenchProtocol;

impl Protocol for BenchProtocol {
    type Request = Request;
    type Response = Response;
}

#[derive(Serialize, Deserialize)]
struct Request {
    id: u64,
}

#[derive(Serialize, Deserialize)]
struct Response {
    id: u64,
}

/// Wires up a client and server talking over two native pipes (one per
/// direction, like stdin/stdout), and starts the server loop in the
/// background. The returned client is used for repeated, back-to-back
/// request/response round trips.
async fn build_client(
    trailer: Arc<Vec<u8>>,
) -> (Client<BenchProtocol>, tokio::task::JoinHandle<()>) {
    let (client_recv, server_send) = native_pipe();
    let (server_recv, client_send) = native_pipe();
    let trailer_len = trailer.len();

    let server_task = tokio::spawn(async move {
        Builder::new(APP_PROTOCOL.0, APP_PROTOCOL.1)
            .trailer_credit_interval(trailer_len.max(1))
            .max_fragment_size(FRAGMENT_SIZE)
            .server_split(server_recv.compat(), server_send.compat_write())
            .await
            .unwrap()
            .bind::<BenchProtocol>()
            .serve(
                move |context: CallContext<BenchProtocol>, request: Request| {
                    let trailer = Arc::clone(&trailer);
                    async move {
                        let mut send = context.respond_with_trailer(Response { id: request.id });
                        send.write_all(&trailer).await.unwrap();
                        send.finish();
                    }
                },
            )
            // Ends with `ConnectionClosed` once the benchmark drops the
            // client and its end of the pipes closes; not a real failure.
            .await
            .ok();
    });

    let client = Builder::new(APP_PROTOCOL.0, APP_PROTOCOL.1)
        .trailer_credit_interval(trailer_len.max(1))
        .max_fragment_size(FRAGMENT_SIZE)
        .client_split(client_recv.compat(), client_send.compat_write())
        .await
        .unwrap()
        .bind::<BenchProtocol>();

    (client, server_task)
}

async fn round_trip(client: &Client<BenchProtocol>, trailer_len: usize) {
    let (response, trailer) = client
        .call(Request { id: 0 })
        .await
        .unwrap()
        .into_response_trailer();
    debug_assert_eq!(response.id, 0);
    let mut buf = Vec::with_capacity(trailer_len);
    trailer.unwrap().read_to_end(&mut buf).await.unwrap();
    debug_assert_eq!(buf.len(), trailer_len);
}

fn bench_trailer_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("pipe_trailer_roundtrip");
    group.sample_size(10);
    for trailer_len in [2 * 1024 * 1024, 4 * 1024 * 1024] {
        group.throughput(Throughput::Bytes(trailer_len as u64));
        let (client, _server) = rt.block_on(build_client(Arc::new(vec![0u8; trailer_len])));
        group.bench_with_input(
            BenchmarkId::from_parameter(trailer_len),
            &trailer_len,
            |b, &trailer_len| {
                b.to_async(&rt).iter(|| round_trip(&client, trailer_len));
            },
        );
    }
    group.finish();
}

#[cfg(any(unix, windows))]
criterion_group!(benches, bench_trailer_roundtrip);
#[cfg(any(unix, windows))]
criterion_main!(benches);
