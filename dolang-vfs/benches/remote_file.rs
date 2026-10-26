//! Benchmarks bulk file I/O against a remote VFS session over native OS
//! pipes — the same transport shape as the `dolang-ext-shell` remote VFS
//! subprocess, and the same shape as an agent reached through `ssh`.
//!
//! A split (reader/writer) transport puts the session in remote mode, so
//! `open` yields an opaque handle and every read and write becomes an RPC
//! round trip carrying a trailer. That makes this benchmark a measure of the
//! per-round-trip cost of streaming, which is what bulk transfer throughput
//! is made of. `/dev/null` and `/dev/zero` stand in for the endpoints so the
//! disk stays out of the measurement.
//!
//! The topology deliberately mirrors a real deployment: client and server
//! each get their own **current-thread** runtime on their own thread, as
//! `dolang-shell` and the `dolang-vfs` agent both do
//! (`dolang-shell-vfs/src/lib.rs`, `dolang-shell-main/src/lib.rs`). This
//! matters for what the numbers mean — the trailer send path hands a
//! fragment straight to the transport when the driver can be reached
//! cooperatively, and that handoff behaves differently on a multi-threaded
//! runtime, where each wake can cross threads.
//!
//! The chunk size parameter is the caller's buffer size, which is what sets
//! the amount of data one round trip carries; see
//! [`dolang_vfs::STREAM_CHUNK_SIZE`].
//!
//! Unix only (the endpoints are Unix device files); a no-op stub keeps the
//! target building elsewhere. Run with `cargo bench -p dolang-vfs`.

#![cfg_attr(not(unix), allow(unused))]

#[cfg(unix)]
mod bench {
    use std::os::fd::{AsRawFd, OwnedFd};

    use criterion::{BenchmarkId, Criterion, Throughput};
    use dolang_vfs::{Vfs, server::Server};
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::{
        net::unix::pipe,
        runtime::{Builder, Runtime},
    };
    use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

    /// Matches the buffer `dolang-ext-shell` requests for its remote VFS
    /// subprocess transport (`REMOTE_VFS_PIPE_BUFFER_SIZE`).
    #[cfg(target_os = "linux")]
    const PIPE_BUFFER_SIZE: usize = 1024 * 1024;

    /// Creates a pipe as raw descriptors, so each end can be registered with
    /// whichever runtime ends up owning it. Returns `(read, write)`.
    fn raw_pipe() -> (OwnedFd, OwnedFd) {
        let (read, write) = nix::unistd::pipe().unwrap();
        for fd in [&read, &write] {
            // `pipe::Sender`/`pipe::Receiver` require non-blocking mode.
            unsafe {
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        #[cfg(target_os = "linux")]
        // Best-effort, as in dolang-vfs's own pipe setup: failure just
        // leaves the default buffer size.
        unsafe {
            libc::fcntl(
                write.as_raw_fd(),
                libc::F_SETPIPE_SZ,
                PIPE_BUFFER_SIZE as i32,
            );
        }
        (read, write)
    }

    fn current_thread() -> Runtime {
        Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn typed(path: &str) -> dolang_vfs::path::Path<'_> {
        dolang_vfs::path::Path::unix(path)
    }

    /// Connects a client to a server over two pipes (one per direction, like
    /// stdin/stdout), with the server running its own current-thread runtime
    /// on its own thread — one runtime per endpoint, as in a real session.
    fn build_client(rt: &Runtime) -> Vfs {
        let (client_recv, server_send) = raw_pipe();
        let (server_recv, client_send) = raw_pipe();

        std::thread::spawn(move || {
            current_thread().block_on(async move {
                let recv = pipe::Receiver::from_owned_fd(server_recv).unwrap();
                let send = pipe::Sender::from_owned_fd(server_send).unwrap();
                // Ends once the benchmark drops the client and its end of
                // the pipes closes; not a real failure.
                if let Ok(server) = Server::new_split(recv.compat(), send.compat_write()).await {
                    let _ = server.serve().await;
                }
            });
        });

        rt.block_on(async move {
            let recv = pipe::Receiver::from_owned_fd(client_recv).unwrap();
            let send = pipe::Sender::from_owned_fd(client_send).unwrap();
            Vfs::new_split(recv.compat(), send.compat_write())
                .await
                .unwrap()
        })
    }

    async fn write_chunks(client: &Vfs, chunk: &[u8], total: usize) {
        let mut options = client.open_options();
        options.write(true);
        let mut file = options.open(typed("/dev/null")).await.unwrap();
        let mut written = 0;
        while written < total {
            file.write_all(chunk).await.unwrap();
            written += chunk.len();
        }
        file.flush().await.unwrap();
    }

    async fn read_chunks(client: &Vfs, chunk_len: usize, total: usize) {
        let mut buf = vec![0u8; chunk_len];
        let mut options = client.open_options();
        options.read(true);
        let mut file = options.open(typed("/dev/zero")).await.unwrap();
        let mut read = 0;
        while read < total {
            let n = file.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "/dev/zero returned end of file");
            read += n;
        }
    }

    pub fn bench_remote_file(c: &mut Criterion) {
        const TOTAL: usize = 64 * 1024 * 1024;

        let rt = current_thread();
        let client = build_client(&rt);

        let mut group = c.benchmark_group("remote_file");
        group.sample_size(10);
        group.throughput(Throughput::Bytes(TOTAL as u64));
        for chunk_len in [64 * 1024, 512 * 1024, 1024 * 1024, 2048 * 1024] {
            let chunk = vec![0xABu8; chunk_len];
            group.bench_with_input(BenchmarkId::new("write", chunk_len), &chunk_len, |b, _| {
                b.to_async(&rt)
                    .iter(|| write_chunks(&client, &chunk, TOTAL));
            });
            group.bench_with_input(
                BenchmarkId::new("read", chunk_len),
                &chunk_len,
                |b, &chunk_len| {
                    b.to_async(&rt)
                        .iter(|| read_chunks(&client, chunk_len, TOTAL));
                },
            );
        }
        group.finish();
    }
}

#[cfg(unix)]
criterion::criterion_group!(benches, bench::bench_remote_file);
#[cfg(unix)]
criterion::criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
