use std::{
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use dolang_rpc::{
    Builder, Error, Protocol,
    client::Client,
    server::CallContext,
    session::{Gift, OpaqueResource},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// `Server<P>`/`Client<P>` are only ever reachable via `UnboundServer`/
/// `UnboundClient`, which mandate an application-protocol descriptor. Tests
/// don't care about application-protocol negotiation itself, so they all
/// share this one dummy descriptor.
const APP_PROTOCOL: (&str, &[u16]) = ("test", &[1]);

fn builder() -> Builder {
    Builder::new(APP_PROTOCOL.0, APP_PROTOCOL.1)
}

async fn unbound_client<T, P: Protocol>(stream: T) -> Client<P>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    builder().client(stream).await.unwrap().bind()
}

async fn unbound_client_with_builder<T, P: Protocol>(b: Builder, stream: T) -> Client<P>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    b.client(stream).await.unwrap().bind()
}

async fn unbound_client_split<R, W, P: Protocol>(reader: R, writer: W) -> Client<P>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    builder().client_split(reader, writer).await.unwrap().bind()
}

#[cfg(unix)]
async fn unbound_client_unix<P: Protocol>(stream: std::os::unix::net::UnixStream) -> Client<P> {
    builder().client_unix(stream).await.unwrap().bind()
}

#[cfg(unix)]
async fn unbound_client_unix_with_builder<P: Protocol>(
    b: Builder,
    stream: std::os::unix::net::UnixStream,
) -> Client<P> {
    b.client_unix(stream).await.unwrap().bind()
}

struct ShortWriter<W> {
    inner: W,
    max_write: usize,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ShortWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let len = buf.len().min(self.max_write);
        Pin::new(&mut self.inner).poll_write(cx, &buf[..len])
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Test;
impl Protocol for Test {
    type Request = Request;
    type Response = Response;
}

#[derive(Serialize, Deserialize)]
enum Request {
    Echo(u32),
    Delay(u64),
    Shutdown,
    /// A large payload, used to force multi-fragment messages.
    Bulk(Vec<u8>),
    /// Echoes `u32` back in the response, and — if the request carried a
    /// raw trailer — echoes that trailer back as the response's trailer.
    TrailerRoundTrip(u32),
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Response(u32);

#[tokio::test]
async fn multiplexes_out_of_order_calls() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    // `UnboundServer` construction performs a real handshake, so it must run
    // concurrently with the client's own construction below (one spawned,
    // one awaited directly) rather than sequentially.
    tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |context, request| {
                let response = match request {
                    Request::Echo(value) => Response(value),
                    Request::Delay(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Response(ms as u32)
                    }
                    Request::Shutdown | Request::Bulk(_) | Request::TrailerRoundTrip(_) => {
                        unreachable!()
                    }
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let slow = client.call(Request::Delay(30));
    let fast = client.call(Request::Echo(7));
    assert_eq!(fast.await.unwrap().into_response(), Response(7));
    assert_eq!(slow.await.unwrap().into_response(), Response(30));
}

#[tokio::test]
async fn concurrent_call_limit_withholds_requests_until_a_terminal_response() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let dispatched = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(tokio::sync::Notify::new());
    let server_dispatched = dispatched.clone();
    let server_first_started = first_started.clone();
    tokio::spawn(async move {
        builder()
            .max_concurrent_calls(1)
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |context, request| {
                server_dispatched.fetch_add(1, Ordering::SeqCst);
                let response = match request {
                    Request::Delay(ms) => {
                        server_first_started.notify_one();
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Response(ms as u32)
                    }
                    Request::Echo(value) => Response(value),
                    _ => unreachable!(),
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(
        // The server's lower advertised value must win negotiation.
        builder().max_concurrent_calls(2),
        client_io,
    )
    .await;
    let first = client.call(Request::Delay(30));
    first_started.notified().await;
    let second = client.call(Request::Echo(7));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    assert_eq!(first.await.unwrap().into_response(), Response(30));
    assert_eq!(second.await.unwrap().into_response(), Response(7));
    assert_eq!(dispatched.load(Ordering::SeqCst), 2);
}

/// A request whose payload cannot be encoded is failed locally and never
/// reaches the wire, so no response — and therefore no terminal event — ever
/// comes back for it. Its call slot has to be reclaimed where the failure
/// happens, or a client wedges permanently after `max_concurrent_calls` of
/// them.
#[tokio::test]
async fn a_request_that_fails_to_encode_does_not_hold_its_call_slot() {
    #[derive(Debug, Eq, PartialEq)]
    struct Fallible(bool);

    impl Serialize for Fallible {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if self.0 {
                return Err(serde::ser::Error::custom("this request cannot be encoded"));
            }
            serializer.serialize_bool(self.0)
        }
    }

    impl<'de> Deserialize<'de> for Fallible {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Ok(Self(bool::deserialize(deserializer)?))
        }
    }

    struct Fallible1;
    impl Protocol for Fallible1 {
        type Request = Fallible;
        type Response = Fallible;
    }

    // A leaked slot leaves a later call queued for admission forever, and
    // which call that is depends on the limit, so the bound goes around the
    // whole test rather than around any one call.
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client_io, server_io) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            builder()
                .server(server_io)
                .await
                .unwrap()
                .bind::<Fallible1>()
                .serve(async |context: CallContext<Fallible1>, _request| {
                    context.respond(Fallible(false));
                })
                .await
        });
        let client = unbound_client_with_builder::<_, Fallible1>(
            builder().max_concurrent_calls(1),
            client_io,
        )
        .await;

        for _ in 0..4 {
            assert!(matches!(
                client.call(Fallible(true)).await,
                Err(Error::Serialize(_))
            ));
        }

        let response = client.call(Fallible(false)).await.unwrap().into_response();
        assert_eq!(response, Fallible(false));
    })
    .await
    .expect("the failed requests must not have consumed the call limit");
}

#[tokio::test]
async fn cancelling_a_call_waiting_for_admission_never_dispatches_it() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let dispatched = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(tokio::sync::Notify::new());
    let server_dispatched = dispatched.clone();
    let server_first_started = first_started.clone();
    tokio::spawn(async move {
        builder()
            .max_concurrent_calls(1)
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |context, request| {
                server_dispatched.fetch_add(1, Ordering::SeqCst);
                let Request::Delay(ms) = request else {
                    panic!("queued request was dispatched")
                };
                server_first_started.notify_one();
                tokio::time::sleep(Duration::from_millis(ms)).await;
                context.respond(Response(ms as u32));
            })
            .await
    });
    let client =
        unbound_client_with_builder::<_, Test>(builder().max_concurrent_calls(1), client_io).await;
    let first = client.call(Request::Delay(20));
    first_started.notified().await;
    let mut queued = client.call(Request::Echo(7));
    queued.cancel();
    assert!(matches!(queued.await, Err(Error::Cancelled)));
    assert_eq!(first.await.unwrap().into_response(), Response(20));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn split_transport_round_trip() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let server = tokio::spawn(async move {
        builder()
            .server_split(server_reader, server_writer)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |mut context, request| {
                let response = match request {
                    Request::Echo(value) => Response(value),
                    Request::Shutdown => {
                        context.shutdown();
                        Response(0)
                    }
                    Request::Delay(_) | Request::Bulk(_) | Request::TrailerRoundTrip(_) => {
                        unreachable!()
                    }
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_split::<_, _, Test>(client_reader, client_writer).await;
    assert_eq!(
        client.call(Request::Echo(7)).await.unwrap().into_response(),
        Response(7)
    );
    assert_eq!(
        client
            .call(Request::Shutdown)
            .await
            .unwrap()
            .into_response(),
        Response(0)
    );
    client.close().await;
    assert!(server.await.unwrap().is_ok());
}

#[tokio::test]
async fn unguarded_cancellation_aborts_handler() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let dropped = Arc::new(AtomicBool::new(false));
    let server_dropped = dropped.clone();
    tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |context, _| {
                struct SetOnDrop(Arc<AtomicBool>);
                impl Drop for SetOnDrop {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let guard = SetOnDrop(server_dropped.clone());
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(guard);
                context.respond(Response(0));
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let mut call = client.call(Request::Delay(10_000));
    tokio::time::sleep(Duration::from_millis(10)).await;
    call.cancel();
    assert!(matches!(call.await, Err(Error::Cancelled)));
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn guarded_cancellation_returns_normal_response() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |mut context, _| {
                let cancelled = context
                    .cancel_guard(async |_| tokio::time::sleep(Duration::from_secs(10)).await)
                    .await
                    .is_err();
                context.respond(Response(u32::from(cancelled)));
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let mut call = client.call(Request::Delay(10_000));
    tokio::time::sleep(Duration::from_millis(10)).await;
    call.cancel();
    assert_eq!(call.await.unwrap().into_response(), Response(1));
}

#[tokio::test]
async fn disconnect_fails_pending_calls() {
    let (client_io, server_io) = tokio::io::duplex(64);
    // A real server is needed so the client's construction handshake has a
    // peer to negotiate with; its handler never responds, so the pending
    // call is still outstanding when the connection drops.
    let server = tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |_context, _request| {
                std::future::pending::<()>().await;
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let call = client.call(Request::Echo(1));
    server.abort();
    assert!(matches!(
        call.await,
        Err(Error::Io(_)) | Err(Error::ConnectionClosed)
    ));
}

#[tokio::test]
async fn close_stops_tasks_and_fails_pending_calls() {
    let (client_io, peer_io) = tokio::io::duplex(64);
    // Kept running (not aborted) for the whole test: this test exercises
    // `Client::close`, not peer disconnection.
    let _server = tokio::spawn(async move {
        builder()
            .server(peer_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |_context, _request| {
                std::future::pending::<()>().await;
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let call = client.call(Request::Echo(1));
    client.close().await;
    assert!(matches!(call.await, Err(Error::ConnectionClosed)));
}

#[tokio::test]
async fn server_shutdown_drains_outstanding_requests() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let delay_started = Arc::new(tokio::sync::Notify::new());
    let server_delay_started = delay_started.clone();
    let server = tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |mut context, request| {
                let response = match request {
                    Request::Echo(value) => Response(value),
                    Request::Delay(ms) => {
                        server_delay_started.notify_one();
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Response(ms as u32)
                    }
                    Request::Shutdown => {
                        context.shutdown();
                        Response(99)
                    }
                    Request::Bulk(_) | Request::TrailerRoundTrip(_) => unreachable!(),
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let slow = client.call(Request::Delay(20));
    delay_started.notified().await;
    let shutdown = client.call(Request::Shutdown);
    assert_eq!(shutdown.await.unwrap().into_response(), Response(99));
    assert_eq!(slow.await.unwrap().into_response(), Response(20));
    client.close().await;
    assert!(server.await.unwrap().is_ok());
}

#[tokio::test]
async fn interleaves_large_and_small_messages_round_robin() {
    let make = || builder().max_fragment_size(256);
    let (client_io, server_io) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |context, request| {
                let response = match request {
                    Request::Echo(value) => Response(value),
                    Request::Bulk(data) => Response(data.len() as u32),
                    Request::Delay(_) | Request::Shutdown | Request::TrailerRoundTrip(_) => {
                        unreachable!()
                    }
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let bulk = client.call(Request::Bulk(vec![b'x'; 64 * 1024]));
    let echo = client.call(Request::Echo(7));
    assert_eq!(echo.await.unwrap().into_response(), Response(7));
    assert_eq!(bulk.await.unwrap().into_response(), Response(64 * 1024));
}

#[tokio::test]
async fn bounded_concurrency_limits_simultaneous_large_transfers() {
    let make = || builder().max_fragment_size(256).max_concurrent_calls(2);
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |context, request| {
                let response = match request {
                    Request::Echo(value) => Response(value),
                    Request::Bulk(data) => Response(data.len() as u32),
                    Request::Delay(_) | Request::Shutdown | Request::TrailerRoundTrip(_) => {
                        unreachable!()
                    }
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    // More concurrent large transfers than `max_concurrent_calls`, so at
    // least one must sit in the scheduler's `waiting` queue.
    let bulk_a = client.call(Request::Bulk(vec![b'a'; 16 * 1024]));
    let bulk_b = client.call(Request::Bulk(vec![b'b'; 16 * 1024]));
    let bulk_c = client.call(Request::Bulk(vec![b'c'; 16 * 1024]));
    let echo = client.call(Request::Echo(7));
    assert_eq!(echo.await.unwrap().into_response(), Response(7));
    assert_eq!(bulk_a.await.unwrap().into_response(), Response(16 * 1024));
    assert_eq!(bulk_b.await.unwrap().into_response(), Response(16 * 1024));
    assert_eq!(bulk_c.await.unwrap().into_response(), Response(16 * 1024));
}

#[tokio::test]
async fn cancel_during_fragment_transmission_completes_without_hanging() {
    let dispatched = Arc::new(AtomicBool::new(false));
    let server_dispatched = dispatched.clone();
    let make = || builder().max_fragment_size(32);
    // A tiny duplex buffer forces many small read/write handoffs between
    // the client writer and server reader tasks, spreading a large
    // transfer out over many scheduling points so cancellation reliably
    // lands mid-transmission rather than before or after it entirely.
    let (client_io, server_io) = tokio::io::duplex(64);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |context, request| {
                server_dispatched.store(true, Ordering::Release);
                let response = match request {
                    Request::Bulk(data) => Response(data.len() as u32),
                    _ => unreachable!(),
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let mut call = client.call(Request::Bulk(vec![b'x'; 256 * 1024]));
    tokio::time::sleep(Duration::from_micros(200)).await;
    call.cancel();
    assert!(matches!(call.await, Err(Error::Cancelled)));
}

#[tokio::test]
async fn resource_limits_enforced_end_to_end() {
    let make = || builder().max_payload_size(16);
    let (client_io, server_io) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |context, request| {
                let response = match request {
                    Request::Bulk(data) => Response(data.len() as u32),
                    _ => unreachable!(),
                };
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let call = client.call(Request::Bulk(vec![b'x'; 1024]));
    assert!(matches!(
        call.await,
        Err(Error::Protocol(_)) | Err(Error::ConnectionClosed) | Err(Error::Io(_))
    ));
}

async fn trailer_echo_handler(mut context: CallContext<Test>, request: Request) {
    match request {
        Request::TrailerRoundTrip(value) => {
            let mut data = None;
            if let Some(mut trailer) = context.trailer() {
                let mut bytes = Vec::new();
                trailer.read_to_end(&mut bytes).await.unwrap();
                data = Some(bytes);
            }
            if let Some(data) = data {
                let mut trailer = context.respond_with_trailer(Response(value));
                trailer.write_all(&data).await.unwrap();
                trailer.finish();
            } else {
                context.respond(Response(value));
            }
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn request_and_response_trailers_round_trip_absent_empty_single_and_multi_fragment() {
    let make = || builder().max_fragment_size(8);
    let (client_io, server_io) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(trailer_echo_handler)
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    // Absent: an ordinary call sends and receives no trailer at all.
    let (response, trailer) = client
        .call(Request::TrailerRoundTrip(1))
        .await
        .unwrap()
        .into_response_trailer();
    assert_eq!(response, Response(1));
    assert!(trailer.is_none());

    // Present but empty, distinguishable from absent.
    let send = client.call_with_trailer(Request::TrailerRoundTrip(2));
    let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
    assert_eq!(response, Response(2));
    let mut received = Vec::new();
    trailer
        .as_mut()
        .unwrap()
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert!(received.is_empty());

    // Single-fragment: fits within max_fragment_size.
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(3));
    send.write_all(b"abcd").await.unwrap();
    let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
    assert_eq!(response, Response(3));
    let mut received = Vec::new();
    trailer
        .as_mut()
        .unwrap()
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert_eq!(received, b"abcd");

    // Multi-fragment: exceeds max_fragment_size, both directions.
    let big = vec![b'x'; 100];
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(4));
    send.write_all(&big).await.unwrap();
    let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
    assert_eq!(response, Response(4));
    let mut received = Vec::new();
    trailer
        .as_mut()
        .unwrap()
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert_eq!(received, big);
}

#[tokio::test]
async fn short_transport_write_stages_and_flushes_the_fragment_suffix() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(trailer_echo_handler)
            .await
    });
    let client = unbound_client_split::<_, _, Test>(
        client_reader,
        ShortWriter {
            inner: client_writer,
            // The wire header fits, then only this prefix of the payload
            // fits in the direct transport write.
            max_write: 16,
        },
    )
    .await;

    let data = (0..100).map(|value| value as u8).collect::<Vec<_>>();
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(9));
    send.write_all(&data).await.unwrap();
    let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
    assert_eq!(response, Response(9));
    let mut received = Vec::new();
    trailer
        .as_mut()
        .unwrap()
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert_eq!(received, data);
}

#[cfg(unix)]
#[tokio::test]
async fn request_trailer_round_trips_over_unix_transport() {
    let (client_stream, server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
    tokio::spawn(async move {
        builder()
            .server_unix(server_stream)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(trailer_echo_handler)
            .await
    });
    let client = unbound_client_unix::<Test>(client_stream).await;
    let data = vec![b'x'; 4096];
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(1));
    send.write_all(&data).await.unwrap();
    let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
    assert_eq!(response, Response(1));
    let mut received = Vec::new();
    trailer
        .as_mut()
        .unwrap()
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert_eq!(received, data);
}

#[tokio::test]
async fn trailer_call_cancelled_mid_transmission_completes_without_hanging() {
    let make = || builder().max_fragment_size(32);
    // A tiny duplex buffer forces many small read/write handoffs, spreading
    // the trailer transfer out over many scheduling points so cancellation
    // spreads trailer transmission across scheduling points.
    let (client_io, server_io) = tokio::io::duplex(64);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |mut context: CallContext<Test>, request| {
                let response = match request {
                    Request::TrailerRoundTrip(value) => Response(value),
                    Request::Echo(value) => Response(value),
                    _ => unreachable!(),
                };
                // Read the trailer rather than letting `respond` discard it,
                // so the abort under test is the producer's own and not a
                // race against the peer telling it to stop. The read fails
                // when the producer is dropped, which is the point.
                if let Some(mut trailer) = context.trailer() {
                    let mut received = Vec::new();
                    let _ = trailer.read_to_end(&mut received).await;
                }
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(1));
    send.write_all(&[b'x'; 17]).await.unwrap();
    drop(send);
    // Dropping a producer after committing a fragment finishes its staged
    // bytes and aborts that call without poisoning the following message.
    assert_eq!(
        client.call(Request::Echo(7)).await.unwrap().into_response(),
        Response(7)
    );
}

#[tokio::test]
async fn trailer_call_cancelled_after_full_transmission_falls_back_to_ordinary_cancel() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |mut context, _request| {
                let mut body = Vec::new();
                context
                    .trailer()
                    .unwrap()
                    .read_to_end(&mut body)
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(10)).await;
                context.respond(Response(0));
            })
            .await
    });
    let client = unbound_client::<_, Test>(client_io).await;
    let data = b"a small trailer that finishes sending almost immediately";
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(1));
    send.write_all(data).await.unwrap();
    let mut call = send.finish();
    tokio::time::sleep(Duration::from_millis(10)).await;
    call.cancel();
    assert!(matches!(call.await, Err(Error::Cancelled)));
}

/// A handler that answers without reading the request trailer leaves the
/// client's writer parked on exhausted credit. Only the eager `Discard` sent
/// when the unread `TrailerRecv` is dropped gets it moving again — the old
/// lazy notice could never fire here, because a parked sender emits no
/// further fragments to notice.
#[tokio::test]
async fn unread_request_trailer_stops_a_credit_parked_sender_promptly() {
    let make = || builder().trailer_session_window(16).max_fragment_size(64);
    let (client_io, server_io) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |context, request| {
                let response = match request {
                    Request::TrailerRoundTrip(value) => Response(value),
                    _ => unreachable!(),
                };
                // Never touches `trailer`; responding drops it.
                context.respond(response);
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let data = vec![b'x'; 1024];
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(1));
    let result = tokio::time::timeout(Duration::from_secs(10), send.write_all(&data))
        .await
        .expect("the sender must be released, not left parked forever");
    assert_eq!(
        result.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe,
        "an abandoned trailer aborts its sender"
    );
    // The discard is advisory: the call itself still completes normally.
    assert_eq!(send.finish().await.unwrap().into_response(), Response(1));
}

#[tokio::test]
async fn server_discarding_a_request_trailer_errors_the_writer_but_response_still_completes() {
    let make = || builder().max_fragment_size(8);
    let (client_io, server_io) = tokio::io::duplex(64);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |mut context, request| {
                let value = match request {
                    Request::TrailerRoundTrip(value) => value,
                    _ => unreachable!(),
                };
                let mut trailer = context.trailer().unwrap();
                let mut prefix = [0u8; 4];
                trailer.read_exact(&mut prefix).await.unwrap();
                // Simulate hitting an error partway through consuming the
                // request trailer (e.g. a failed file write): stop wanting
                // more of it, but still answer normally through the ordinary
                // response. Dropping the handle notifies the peer at once.
                drop(trailer);
                context.respond(Response(value));
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(1));
    let big = vec![b'x'; 10_000];
    let error = send.write_all(&big).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    let response = send.finish().await.unwrap().into_response();
    assert_eq!(response, Response(1));
}

#[tokio::test]
async fn client_discarding_a_response_trailer_errors_the_servers_writer() {
    let make = || builder().max_fragment_size(8);
    let (client_io, server_io) = tokio::io::duplex(64);
    let write_error = Arc::new(Mutex::new(None));
    let server_write_error = write_error.clone();
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |context, request| {
                let value = match request {
                    Request::TrailerRoundTrip(value) => value,
                    _ => unreachable!(),
                };
                let mut trailer = context.respond_with_trailer(Response(value));
                let big = vec![b'x'; 10_000];
                let result = trailer.write_all(&big).await;
                *server_write_error.lock().unwrap() = result.err();
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let (response, trailer) = client
        .call(Request::TrailerRoundTrip(1))
        .await
        .unwrap()
        .into_response_trailer();
    assert_eq!(response, Response(1));
    let mut trailer = trailer.unwrap();
    let mut prefix = [0u8; 4];
    trailer.read_exact(&mut prefix).await.unwrap();
    // Stop wanting the rest of a still-streaming response trailer. Dropping
    // the handle notifies the peer at once.
    drop(trailer);
    // Give the server's writer time to observe the discard and fail.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        write_error.lock().unwrap().as_ref().map(|e| e.kind()),
        Some(io::ErrorKind::BrokenPipe)
    );
}

#[cfg(unix)]
mod unix_handles {
    use std::io::Read;

    use dolang_rpc::handle::OsHandle;
    use nix::unistd::{pipe, write};
    use serde::{Deserialize, Serialize};

    use super::*;

    struct HandlesProtocol;
    impl Protocol for HandlesProtocol {
        type Request = HandleRequest;
        type Response = HandleResponse;
    }

    #[derive(Serialize, Deserialize)]
    struct HandleRequest {
        handles: Vec<OsHandle>,
    }

    #[derive(Serialize, Deserialize)]
    struct HandleResponse {
        handles: Vec<OsHandle>,
    }

    #[tokio::test]
    async fn transfers_handles_in_requests_and_responses() {
        let (client_stream, server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        tokio::spawn(async move {
            builder()
                .server_unix(server_stream)
                .await
                .unwrap()
                .bind::<HandlesProtocol>()
                .serve(async |context, request| {
                    context.respond(HandleResponse {
                        handles: request.handles,
                    });
                })
                .await
        });
        let client = unbound_client_unix::<HandlesProtocol>(client_stream).await;
        let (read_fd, write_fd) = pipe().unwrap();
        let call = client.call(HandleRequest {
            handles: vec![OsHandle::new(read_fd)],
        });
        let response = call.await.unwrap().into_response();
        let received = response.handles.into_iter().next().unwrap().into_inner();
        write(&write_fd, b"ok").unwrap();
        let mut file = std::fs::File::from(received);
        let mut bytes = [0; 2];
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
    }

    #[tokio::test]
    async fn attachments_can_be_combined_with_a_trailer() {
        let (client_stream, server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        tokio::spawn(async move {
            builder()
                .server_unix(server_stream)
                .await
                .unwrap()
                .bind::<HandlesProtocol>()
                .serve(async |context, request| {
                    context.respond(HandleResponse {
                        handles: request.handles,
                    });
                })
                .await
        });
        let client = unbound_client_unix::<HandlesProtocol>(client_stream).await;
        let (read_fd, write_fd) = pipe().unwrap();
        let mut send = client.call_with_trailer(HandleRequest {
            handles: vec![OsHandle::new(read_fd)],
        });
        send.write_all(b"trailer").await.unwrap();
        let call = send.finish();
        let response = call.await.unwrap().into_response();
        let received = response.handles.into_iter().next().unwrap().into_inner();
        write(&write_fd, b"ok").unwrap();
        let mut file = std::fs::File::from(received);
        let mut bytes = [0; 2];
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
    }

    #[tokio::test]
    async fn transfers_handles_across_multiple_attachment_fragments() {
        let (client_stream, server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        tokio::spawn(async move {
            builder()
                .max_handles_per_message(65)
                .server_unix(server_stream)
                .await
                .unwrap()
                .bind::<HandlesProtocol>()
                .serve(async |context, request| {
                    context.respond(HandleResponse {
                        handles: request.handles,
                    });
                })
                .await
        });
        let client = unbound_client_unix_with_builder::<HandlesProtocol>(
            builder().max_handles_per_message(65),
            client_stream,
        )
        .await;
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for _ in 0..65 {
            let (read, write) = pipe().unwrap();
            reads.push(OsHandle::new(read));
            writes.push(write);
        }
        let response = client
            .call(HandleRequest { handles: reads })
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.handles.len(), 65);
        for (index, (handle, write_fd)) in response
            .handles
            .into_iter()
            .zip(writes.into_iter())
            .enumerate()
        {
            write(&write_fd, &[index as u8]).unwrap();
            let mut file = std::fs::File::from(handle.into_inner());
            let mut byte = [0];
            file.read_exact(&mut byte).unwrap();
            assert_eq!(byte[0], index as u8);
        }
    }

    #[tokio::test]
    async fn negotiates_handle_fragment_and_message_limits() {
        let (client_stream, server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        tokio::spawn(async move {
            builder()
                .max_handles_per_fragment(1)
                .max_handles_per_message(2)
                .server_unix(server_stream)
                .await
                .unwrap()
                .bind::<HandlesProtocol>()
                .serve(async |context, request| {
                    context.respond(HandleResponse {
                        handles: request.handles,
                    });
                })
                .await
        });
        let client = unbound_client_unix::<HandlesProtocol>(client_stream).await;
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for _ in 0..2 {
            let (read, write) = pipe().unwrap();
            reads.push(OsHandle::new(read));
            writes.push(write);
        }
        let response = client
            .call(HandleRequest { handles: reads })
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.handles.len(), 2);

        let mut excess = Vec::new();
        for _ in 0..3 {
            let (read, _write) = pipe().unwrap();
            excess.push(OsHandle::new(read));
        }
        assert!(matches!(
            client.call(HandleRequest { handles: excess }).await,
            Err(Error::Serialize(_))
        ));
    }
}

#[cfg(windows)]
mod windows_handles {
    use std::{
        io::Read,
        os::windows::io::{AsHandle, FromRawHandle, OwnedHandle},
        sync::atomic::{AtomicU64, Ordering},
    };

    use dolang_rpc::handle::OsHandle;
    use serde::{Deserialize, Serialize};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    use super::*;

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

    struct HandlesProtocol;
    impl Protocol for HandlesProtocol {
        type Request = HandleRequest;
        type Response = HandleResponse;
    }

    #[derive(Serialize, Deserialize)]
    struct HandleRequest {
        handle: OsHandle,
    }

    #[derive(Serialize, Deserialize)]
    struct HandleResponse {
        handle: OsHandle,
    }

    async fn pipe_pair() -> (NamedPipeServer, NamedPipeClient) {
        let id = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\dolang-rpc-{}-{id}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        server.connect().await.unwrap();
        (server, client)
    }

    fn current_process_handle() -> OwnedHandle {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                GetCurrentProcessId(),
            )
        };
        assert!(!handle.is_null());
        unsafe { OwnedHandle::from_raw_handle(handle as _) }
    }

    #[tokio::test]
    async fn transfers_handles_in_requests_and_responses() {
        // Use the pipe-server end for the less-privileged RPC client, matching
        // the parent/helper deployment that motivates this transport.
        let (client_pipe, server_pipe) = pipe_pair().await;
        // `UnboundServer`/`UnboundClient` construction both perform a real
        // handshake, so they must run concurrently (one spawned, one
        // awaited directly) rather than sequentially.
        let server = tokio::spawn(async move {
            builder()
                .server_named_pipe_client(server_pipe)
                .await
                .unwrap()
                .bind::<HandlesProtocol>()
                .serve(async |context, request| {
                    context.respond(HandleResponse {
                        handle: request.handle,
                    });
                })
                .await
        });
        // SAFETY: this test owns and controls the connected server endpoint.
        let client =
            unsafe { builder().client_named_pipe_server(client_pipe, current_process_handle()) }
                .await
                .unwrap()
                .bind::<HandlesProtocol>();

        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let _ = file.as_handle();
        let response = client
            .call(HandleRequest {
                handle: OsHandle::new(OwnedHandle::from(file)),
            })
            .await;
        let response = match response {
            Ok(response) => response.into_response(),
            Err(error) => panic!(
                "client failed with {error}; server returned {:?}",
                server.await
            ),
        };
        let mut received = std::fs::File::from(response.handle.into_inner());
        let mut byte = [0];
        received.read_exact(&mut byte).unwrap();
    }
}

/// A protocol whose response hands the caller an opaque, used to pin down what
/// happens to that opaque when nobody is left to receive it.
struct Gifts;
impl Protocol for Gifts {
    type Request = GiftRequest;
    type Response = GiftResponse;
}

#[derive(Serialize, Deserialize)]
struct GiftRequest;

#[derive(Serialize, Deserialize)]
struct GiftResponse {
    endpoint: Gift<EndpointMarker>,
}

struct EndpointMarker;

/// A registered resource that reports its own death.
struct Endpoint(Arc<AtomicBool>);

impl OpaqueResource for Endpoint {
    type Marker = EndpointMarker;
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn wait_for(flag: &AtomicBool) -> bool {
    for _ in 0..200 {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Serves `Gifts`, registering an [`Endpoint`] per request that flips
/// `dropped` when it dies.
///
/// The handler waits to be released by `respond_now` from inside a cancel
/// guard, so a caller can abandon the call and still be sure the response gets
/// built and sent: an unguarded cancel would simply abort the handler, and
/// there would be no gift to lose track of in the first place.
fn serve_gifts<T>(
    server_io: T,
    dropped: Arc<AtomicBool>,
    guarded: Arc<tokio::sync::Notify>,
    respond_now: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _ = builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Gifts>()
            .serve(async move |mut context, _request: GiftRequest| {
                let _ = context
                    .cancel_guard(async |_| {
                        guarded.notify_one();
                        respond_now.notified().await;
                    })
                    .await;
                let endpoint = context.register(Endpoint(dropped.clone()));
                context.respond(GiftResponse { endpoint });
            })
            .await;
    })
}

/// The bug this whole branch exists for: a caller that walks away from a call
/// whose response carries a gift used to strand the resource for the life of
/// the connection.
///
/// Nothing in the application ever sees the response — the reader task decodes
/// it and drops it — so releasing it is entirely the session's job. Note what
/// this implies about the decode: skipping it for a response nobody is waiting
/// on would look like an optimization and would silently reintroduce the leak.
#[tokio::test]
async fn a_cancelled_call_releases_the_gift_in_its_response() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let dropped = Arc::new(AtomicBool::new(false));
    let guarded = Arc::new(tokio::sync::Notify::new());
    let respond_now = Arc::new(tokio::sync::Notify::new());
    let _server = serve_gifts(
        server_io,
        dropped.clone(),
        guarded.clone(),
        respond_now.clone(),
    );
    // Kept alive for the whole test: the release travels over this connection,
    // so closing it early would prove nothing.
    let client = unbound_client::<_, Gifts>(client_io).await;

    let call = client.call(GiftRequest);
    guarded.notified().await;
    drop(call);
    respond_now.notify_one();

    assert!(
        wait_for(&dropped).await,
        "the endpoint in an abandoned response was never released"
    );
}

/// The same resource, claimed normally: it must survive until the caller is
/// actually done with it, and die once the caller drops it.
#[tokio::test]
async fn a_claimed_gift_lives_exactly_as_long_as_the_caller_holds_it() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let dropped = Arc::new(AtomicBool::new(false));
    let guarded = Arc::new(tokio::sync::Notify::new());
    let respond_now = Arc::new(tokio::sync::Notify::new());
    let _server = serve_gifts(
        server_io,
        dropped.clone(),
        guarded.clone(),
        respond_now.clone(),
    );
    let client = unbound_client::<_, Gifts>(client_io).await;

    let call = client.call(GiftRequest);
    guarded.notified().await;
    respond_now.notify_one();
    let response = call.await.unwrap().into_response();

    // A clone is a local handle, not a second protocol reference; the resource
    // must outlive it and every other copy the caller makes.
    let clone = response.endpoint.clone();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !dropped.load(Ordering::SeqCst),
        "the endpoint died while the caller still held it"
    );

    drop(response);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !dropped.load(Ordering::SeqCst),
        "the endpoint died while a clone of the caller's handle was still live"
    );

    drop(clone);
    assert!(
        wait_for(&dropped).await,
        "the endpoint outlived the caller's last handle on it"
    );
}

/// Releasing an opaque must not need an ambient runtime. The caller's last
/// handle on a gift routinely falls out of scope during teardown, after the
/// runtime that carried the session is already gone, and a release that
/// reached for `Handle::current` there would panic in a destructor.
#[test]
fn dropping_a_claimed_gift_outside_a_runtime_does_not_panic() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (endpoint, client, server) = runtime.block_on(async {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let guarded = Arc::new(tokio::sync::Notify::new());
        let respond_now = Arc::new(tokio::sync::Notify::new());
        let server = serve_gifts(
            server_io,
            Arc::new(AtomicBool::new(false)),
            guarded.clone(),
            respond_now.clone(),
        );
        let client = unbound_client::<_, Gifts>(client_io).await;
        let call = client.call(GiftRequest);
        guarded.notified().await;
        respond_now.notify_one();
        let response = call.await.unwrap().into_response();
        (response.endpoint, client, server)
    });

    drop(runtime);
    drop(endpoint);
    drop(client);
    drop(server);
}

/// The auto-release default must be completely transparent: an ordinary
/// `read_to_end` consumer moves megabytes through a credit pool a fraction of
/// that size without knowing flow control exists.
#[tokio::test]
async fn large_trailer_streams_through_a_small_pool_with_an_ordinary_consumer() {
    let make = || {
        builder()
            .trailer_session_window(4096)
            .trailer_credit_interval(1024)
            .max_fragment_size(1024)
    };
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(trailer_echo_handler)
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    // Two orders of magnitude past the window, in both directions.
    let data = vec![b'z'; 512 * 1024];
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(1));
    send.write_all(&data).await.unwrap();
    let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
    assert_eq!(response, Response(1));
    let mut received = Vec::new();
    trailer
        .as_mut()
        .unwrap()
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert_eq!(received, data);
}

/// Manual release is what the sender's pacing actually follows: with the
/// credit pool exhausted and nothing released, the sender must be stuck, and
/// must resume the moment credit is returned.
#[tokio::test]
async fn manual_release_gates_the_sender() {
    let make = || {
        builder()
            .trailer_session_window(1024)
            .trailer_credit_interval(1024)
            .max_fragment_size(256)
    };
    let (client_io, server_io) = tokio::io::duplex(8192);
    // Set once the handler has read a window's worth without releasing.
    let gate = Arc::new(tokio::sync::Notify::new());
    let server_gate = gate.clone();
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(move |mut context: CallContext<Test>, request| {
                let gate = server_gate.clone();
                async move {
                    let value = match request {
                        Request::TrailerRoundTrip(value) => value,
                        _ => unreachable!(),
                    };
                    let mut trailer = context.trailer_manual_credit().unwrap();
                    let mut buf = vec![0u8; 1024];
                    let mut held = 0;
                    // Consume the whole pool and release nothing, which
                    // must leave the peer with no credit at all.
                    while held < 1024 {
                        let n = trailer.read(&mut buf[..1024 - held]).await.unwrap();
                        held += n;
                    }
                    gate.notified().await;
                    // Now drain normally, releasing as we go.
                    trailer.release(held);
                    loop {
                        let n = trailer.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        trailer.release(n);
                    }
                    context.respond(Response(value));
                }
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;
    let data = vec![b'q'; 64 * 1024];
    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(5));

    // Far more than the window, so this cannot finish until credit flows.
    let mut write = Box::pin(send.write_all(&data));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut write)
            .await
            .is_err(),
        "the sender must park once the pool is spent and nothing is released"
    );

    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(30), write)
        .await
        .expect("releasing credit must unpark the sender")
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(30), send.finish())
        .await
        .expect("the call must complete once the trailer drains")
        .unwrap()
        .into_response();
    assert_eq!(response, Response(5));
}

/// One consumer sitting on its bytes must not stop an unrelated trailer on
/// the same connection, *provided the pool has headroom beyond what the
/// stalled consumer holds*. That proviso is the whole shape of the design:
/// there is no per-trailer window, so isolation comes from the sender not
/// having spent the pool on the stalled trailer rather than from anything the
/// protocol enforces.
#[tokio::test]
async fn a_stalled_trailer_does_not_block_an_unrelated_one_given_pool_headroom() {
    let make = || {
        builder()
            .trailer_credit_interval(1024)
            .trailer_session_window(1024 * 1024)
            .max_fragment_size(256)
    };
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |mut context, request| {
                let value = match request {
                    Request::TrailerRoundTrip(value) => value,
                    _ => unreachable!(),
                };
                if value == 1 {
                    // The stalled consumer: take a chunk and hold it for
                    // the rest of the test.
                    let mut trailer = context.trailer_manual_credit().unwrap();
                    let mut buf = vec![0u8; 1024];
                    let _ = trailer.read(&mut buf).await.unwrap();
                    std::future::pending::<()>().await;
                } else {
                    let mut sink = Vec::new();
                    context
                        .trailer()
                        .unwrap()
                        .read_to_end(&mut sink)
                        .await
                        .unwrap();
                }
                context.respond(Response(value));
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    let mut stalled = client.call_with_trailer(Request::TrailerRoundTrip(1));
    let big = vec![b'a'; 64 * 1024];
    tokio::spawn(async move {
        let _ = stalled.write_all(&big).await;
        std::future::pending::<()>().await;
    });

    let mut healthy = client.call_with_trailer(Request::TrailerRoundTrip(2));
    let payload = vec![b'b'; 32 * 1024];
    tokio::time::timeout(Duration::from_secs(30), async {
        healthy.write_all(&payload).await.unwrap();
        healthy.finish().await
    })
    .await
    .expect("a stalled trailer must not block an unrelated one")
    .unwrap();
}

/// Long-lived trailers must not consume a per-message concurrency slot: more
/// of them can be open at once than the old `max_incomplete_trailers` (16)
/// ever allowed, and an ordinary call still goes through alongside them.
#[tokio::test]
async fn many_concurrent_long_lived_trailers_all_progress() {
    const TRAILERS: u32 = 40;
    let make = || {
        builder()
            .trailer_credit_interval(4096)
            .max_fragment_size(512)
    };
    let (client_io, server_io) = tokio::io::duplex(16384);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async |mut context, request| match request {
                Request::TrailerRoundTrip(value) => {
                    let mut sink = Vec::new();
                    context
                        .trailer()
                        .unwrap()
                        .read_to_end(&mut sink)
                        .await
                        .unwrap();
                    context.respond(Response(value + sink.len() as u32));
                }
                Request::Echo(value) => context.respond(Response(value)),
                _ => unreachable!(),
            })
            .await
    });
    let client = Arc::new(unbound_client_with_builder::<_, Test>(make(), client_io).await);

    let mut calls = Vec::new();
    for id in 0..TRAILERS {
        let client = client.clone();
        calls.push(tokio::spawn(async move {
            let mut send = client.call_with_trailer(Request::TrailerRoundTrip(id));
            send.write_all(&vec![b'x'; 8192]).await.unwrap();
            send.finish().await.unwrap().into_response()
        }));
    }
    // An ordinary call must still get through with all of those open.
    let echo = tokio::time::timeout(Duration::from_secs(30), client.call(Request::Echo(99)))
        .await
        .expect("trailer-phase messages must not consume concurrency slots")
        .unwrap()
        .into_response();
    assert_eq!(echo, Response(99));

    for (id, call) in calls.into_iter().enumerate() {
        let response = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("every trailer must complete")
            .unwrap();
        assert_eq!(response, Response(id as u32 + 8192));
    }
}

/// Drains a request trailer and answers without one, the shape a bulk write
/// takes: data flows one way and the response is a bare acknowledgement.
async fn trailer_sink_handler(mut context: CallContext<Test>, request: Request) {
    match request {
        Request::TrailerRoundTrip(value) => {
            if let Some(mut trailer) = context.trailer() {
                let mut sink = tokio::io::sink();
                tokio::io::copy(&mut trailer, &mut sink).await.unwrap();
            }
            context.respond(Response(value));
        }
        _ => unreachable!(),
    }
}

/// A one-way trailer that ends below the coalescing threshold has never
/// credited a byte, and nothing after it can trigger a flush: no more data
/// arrives, no response trailer travels the other way, and a completed
/// trailer sends no `Discard` to settle against. Its completion is the only
/// chance to return the peer's pool debt, and without it a stream of small
/// writes — which is what remote file and stdio I/O are made of — parks the
/// sender for good once the pool runs dry.
#[tokio::test]
async fn one_way_trailers_below_the_coalescing_threshold_do_not_leak_session_credit() {
    // Default-shaped windows, with a pool a small multiple of the transfer
    // so a per-transfer leak shows up in a few rounds rather than hundreds.
    let make = || {
        builder()
            .trailer_session_window(64 * 1024)
            .trailer_credit_interval(32 * 1024)
    };
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(trailer_sink_handler)
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    // Enough rounds to drain the pool several times over. A leak is not
    // strictly one transfer per round — a trailer the peer happens to drop
    // before observing its end sends a `Discard`, which settles that round's
    // debt — so a handful of rounds proves nothing.
    let data = vec![b'q'; 4096];
    for round in 0..120 {
        let mut send = client.call_with_trailer(Request::TrailerRoundTrip(round));
        let round_trip = async {
            send.write_all(&data).await.unwrap();
            send.finish().await.unwrap().into_response()
        };
        let response = tokio::time::timeout(Duration::from_secs(10), round_trip)
            .await
            .unwrap_or_else(|_| panic!("round {round} stalled; session credit leaked"));
        assert_eq!(response, Response(round));
    }
}

/// Every completed trailer must return its share of the session pool. If any
/// of it leaked — a final coalesced credit never flushed, or a late `Credit`
/// dropped because its send had already left the scheduler — the pool would
/// shrink on each transfer and this loop would stall well before the end.
#[tokio::test]
async fn sequential_trailers_do_not_leak_session_credit() {
    // A pool only twice the size of one transfer, so even a small per-
    // transfer leak exhausts it within a few iterations.
    let make = || {
        builder()
            .trailer_session_window(64 * 1024)
            .trailer_credit_interval(16 * 1024)
            .max_fragment_size(4096)
    };
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(trailer_echo_handler)
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    let data = vec![b'p'; 32 * 1024];
    for round in 0..20 {
        let mut send = client.call_with_trailer(Request::TrailerRoundTrip(round));
        let round_trip = async {
            send.write_all(&data).await.unwrap();
            let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
            let mut received = Vec::new();
            trailer
                .as_mut()
                .unwrap()
                .read_to_end(&mut received)
                .await
                .unwrap();
            (response, received)
        };
        let (response, received) = tokio::time::timeout(Duration::from_secs(30), round_trip)
            .await
            .unwrap_or_else(|_| panic!("round {round} stalled; session credit leaked"));
        assert_eq!(response, Response(round));
        assert_eq!(received.len(), data.len());
    }
}

/// A detached request trailer outlives the response, which is what makes one
/// call a duplex pipe: the server answers immediately, then keeps reading
/// what the client is still writing while streaming back a response trailer
/// of its own. Both directions are open at once and each ends on its own.
#[tokio::test]
async fn a_detached_request_trailer_keeps_streaming_after_the_response() {
    let make = || builder().max_fragment_size(8);
    let (client_io, server_io) = tokio::io::duplex(64);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |mut context, request| {
                let value = match request {
                    Request::TrailerRoundTrip(value) => value,
                    _ => unreachable!(),
                };
                // Detach the request half, then answer. The call is over on
                // the wire from here, but the trailer is not.
                let mut incoming = context.trailer().unwrap();
                let mut outgoing = context.respond_with_trailer(Response(value));
                let mut received = Vec::new();
                incoming.read_to_end(&mut received).await.unwrap();
                // Echoing back what arrived only after responding proves the
                // read really happened past the end of the call.
                outgoing.write_all(&received).await.unwrap();
                outgoing.finish();
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    let mut send = client.call_with_trailer(Request::TrailerRoundTrip(7));
    let data = vec![b'd'; 4096];
    let exchange = async {
        send.write_all(&data).await.unwrap();
        let (response, mut trailer) = send.finish().await.unwrap().into_response_trailer();
        let mut echoed = Vec::new();
        trailer
            .as_mut()
            .unwrap()
            .read_to_end(&mut echoed)
            .await
            .unwrap();
        (response, echoed)
    };
    let (response, echoed) = tokio::time::timeout(Duration::from_secs(30), exchange)
        .await
        .expect("the duplex exchange stalled");
    assert_eq!(response, Response(7));
    assert_eq!(echoed, data);
}

/// Big enough that a leak of one request's payload shows up within a couple of
/// rounds, small enough to keep the tests quick.
const QUOTA: usize = 16 * 1024;

/// Over half the pool, so no two of these can be outstanding at once, and
/// comfortably under the per-message cap once postcard's framing is added.
const BIG: usize = QUOTA / 2 + 1024;

fn quota_builder() -> Builder {
    builder()
        .max_payload_size(QUOTA)
        .max_outstanding_payload(QUOTA)
        .max_fragment_size(4096)
}

/// A pool so small that even the few bytes of a `Response(u32)` accumulate
/// into a stall, which is what makes a *response*-side leak visible: the
/// request direction of these tests costs almost nothing.
fn tiny_quota_builder() -> Builder {
    builder()
        .max_payload_size(TINY_QUOTA)
        .max_outstanding_payload(TINY_QUOTA)
}

const TINY_QUOTA: usize = 512;

/// Echoed rather than a small number so postcard spends its full five varint
/// bytes on it. `Response(1)` would cost one byte, and the round counts below
/// would then not add up to a stall even if every response leaked — the test
/// would pass without testing anything.
const WIDE: u32 = u32::MAX;

/// Enough rounds that a leaked response overruns `TINY_QUOTA` several times
/// over.
const TINY_ROUNDS: usize = 400;

/// Drives `rounds` calls through a session whose payload quota holds only a few
/// of them at once, running each to completion before the next.
///
/// Unreleased quota stays subtracted forever, so any leak — one path that
/// answers a call without dropping its charge — stops the connection dead
/// within a few rounds rather than degrading gracefully. That is what makes
/// this shape the right test for release: there is nothing to assert beyond
/// "it kept going".
async fn quota_rounds<F>(make: fn() -> Builder, rounds: usize, round_trip: F)
where
    F: AsyncFn(&Client<Test>),
{
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(
                async move |context: CallContext<Test>, request| match request {
                    Request::Bulk(_) => context.respond(Response(0)),
                    Request::Echo(value) => context.respond(Response(value)),
                    // Answers nothing and drops the context, which must still
                    // release the request's quota.
                    Request::Delay(_) => drop(context),
                    _ => unreachable!(),
                },
            )
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(make(), client_io).await;

    for round in 0..rounds {
        tokio::time::timeout(Duration::from_secs(10), round_trip(&client))
            .await
            .unwrap_or_else(|_| panic!("round {round} stalled; payload quota leaked"));
    }
}

/// The ordinary path, on the request side: a large call answered and its result
/// consumed. Each request is over half the pool, so a single leaked charge
/// stalls the very next round.
#[tokio::test]
async fn completed_calls_do_not_leak_payload_quota() {
    quota_rounds(quota_builder, 12, async |client: &Client<Test>| {
        let response = client
            .call(Request::Bulk(vec![b'x'; BIG]))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response, Response(0));
    })
    .await;
}

/// A handler that drops its context without responding fails the call, and must
/// release just the same. A charge lost on an error path is the same permanent
/// subtraction as one lost on a success path, and rather more likely to go
/// unnoticed.
#[tokio::test]
async fn calls_the_handler_never_answers_do_not_leak_payload_quota() {
    quota_rounds(quota_builder, 12, async |client: &Client<Test>| {
        assert!(matches!(
            client.call(Request::Delay(0)).await,
            Err(Error::Cancelled)
        ));
    })
    .await;
}

/// The response side. A `CallResult` nobody decomposes still has to release, or
/// a caller who simply ignores a response strangles the connection — the
/// server's send budget is what runs out, so the symptom is a call that never
/// completes rather than an error anywhere.
#[tokio::test]
async fn dropped_call_results_do_not_leak_payload_quota() {
    quota_rounds(
        tiny_quota_builder,
        TINY_ROUNDS,
        async |client: &Client<Test>| {
            let result = client.call(Request::Echo(WIDE)).await.unwrap();
            drop(result);
        },
    )
    .await;
}

/// Also the response side, through the token. Extracting the credit moves the
/// release off the `CallResult` and onto the token's own drop, so the release
/// genuinely outlives the value it came from — the whole reason the API exists
/// and the easiest place for it to go missing.
#[tokio::test]
async fn extracted_payload_credit_releases_when_it_is_dropped() {
    quota_rounds(
        tiny_quota_builder,
        TINY_ROUNDS,
        async |client: &Client<Test>| {
            let mut result = client.call(Request::Echo(WIDE)).await.unwrap();
            let credit = result.take_payload_credit();
            assert_eq!(result.into_response(), Response(WIDE));
            // Deliberately after the `CallResult` is gone: nothing was released
            // until this line.
            drop(credit);
        },
    )
    .await;
}

/// A handler that releases early stops holding the connection hostage while it
/// pends, which is the documented escape hatch for a long-lived call with a
/// large request. Every one of these pends until the test drops the client, so
/// without the early release the second call could never be admitted.
#[tokio::test]
async fn releasing_early_lets_a_pending_call_stop_holding_its_quota() {
    let (client_io, server_io) = tokio::io::duplex(8192);
    let (pending_tx, mut pending) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        quota_builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |mut context: CallContext<Test>, _| {
                context.release_payload();
                let _ = pending_tx.send(());
                std::future::pending::<()>().await
            })
            .await
    });
    let client = unbound_client_with_builder::<_, Test>(quota_builder(), client_io).await;

    // Each request is over half the pool, so two of them cannot be
    // outstanding at once unless the first one's quota came back.
    let big = || Request::Bulk(vec![b'x'; BIG]);
    let _first = client.call(big());
    let _second = client.call(big());
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), pending.recv())
            .await
            .expect("the second call never reached the handler")
            .unwrap();
    }
}

/// The reason payload quota and trailer credit are separate pools rather than
/// one.
///
/// This is the ordinary streaming-upload shape: a descriptor payload, a large
/// trailer, and a handler that reads the trailer to completion before it
/// responds. Each call holds its payload charge until the handler completes,
/// and the handler cannot complete until the trailer has flowed. The payloads
/// here are sized so that a few of them fill the payload pool outright — which
/// is harmless with two pools (the rest simply wait their turn) and fatal with
/// one, since the calls that got in would hold every byte of credit their own
/// trailers need to finish.
#[tokio::test]
async fn concurrent_uploads_do_not_deadlock_payload_quota_against_trailer_credit() {
    const PAYLOAD: usize = 4 * 1024;
    const TRAILER: usize = 32 * 1024;
    const POOL: usize = 16 * 1024;

    let make = || {
        builder()
            .max_payload_size(2 * PAYLOAD)
            .max_outstanding_payload(POOL)
            .trailer_session_window(POOL)
            .max_fragment_size(4096)
    };
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        make()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(async move |mut context: CallContext<Test>, request| {
                let Request::Bulk(payload) = request else {
                    unreachable!()
                };
                // Consume the whole trailer *before* responding, which is what
                // makes the payload charge outlive the trailer's need for
                // credit and closes the cycle a shared pool would have.
                let mut received = Vec::new();
                context
                    .trailer()
                    .unwrap()
                    .read_to_end(&mut received)
                    .await
                    .unwrap();
                assert_eq!(received.len(), TRAILER);
                context.respond(Response(payload.len() as u32));
            })
            .await
    });
    let client = Arc::new(unbound_client_with_builder::<_, Test>(make(), client_io).await);

    // Enough calls that their payloads together are twice the pool.
    let calls: Vec<_> = (0..8u32)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move {
                let mut send = client.call_with_trailer(Request::Bulk(vec![b'd'; PAYLOAD]));
                send.write_all(&vec![b'u'; TRAILER]).await.unwrap();
                let response = send.finish().await.unwrap().into_response();
                assert_eq!(response, Response(PAYLOAD as u32));
            })
        })
        .collect();
    for call in calls {
        tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("an upload wedged; the two credit pools are entangled")
            .unwrap();
    }
}

/// The count is generous precisely because the bytes are bounded separately,
/// so many small calls must actually be admitted concurrently rather than
/// queued a few at a time.
#[tokio::test]
async fn many_small_calls_are_admitted_concurrently() {
    let (client_io, server_io) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        builder()
            .server(server_io)
            .await
            .unwrap()
            .bind::<Test>()
            .serve(
                async move |context: CallContext<Test>, request| match request {
                    Request::Echo(value) => context.respond(Response(value)),
                    _ => unreachable!(),
                },
            )
            .await
    });
    let client = Arc::new(unbound_client::<_, Test>(client_io).await);

    let calls: Vec<_> = (0..512u32)
        .map(|value| {
            let client = client.clone();
            tokio::spawn(async move { client.call(Request::Echo(value)).await })
        })
        .collect();
    for (value, call) in calls.into_iter().enumerate() {
        let response = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("a call stalled")
            .unwrap()
            .unwrap()
            .into_response();
        assert_eq!(response, Response(value as u32));
    }
}

/// A protocol whose *responses* are the large payloads, which is what makes a
/// send blocked on payload quota reachable from the server side. `Test`'s
/// `Response(u32)` costs a handful of bytes and could never fill a pool.
#[derive(Serialize, Deserialize)]
enum DrainRequest {
    Big,
    Shutdown,
    /// Parks the handler until the test releases it, holding the drain open.
    Hold,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DrainResponse(Vec<u8>);

struct DrainProtocol;
impl Protocol for DrainProtocol {
    type Request = DrainRequest;
    type Response = DrainResponse;
}

/// How many oversized responses to have in flight. Any two of them exceed
/// `QUOTA`, so at least two are still parked in the scheduler's waiting queue
/// when the drain begins.
const DRAIN_CALLS: usize = 3;

async fn drain_client(io: tokio::io::DuplexStream) -> Client<DrainProtocol> {
    quota_builder().client(io).await.unwrap().bind()
}

/// Spawns a server whose `Big` handler answers with a payload too large for
/// two to share the session's payload quota, and whose `Shutdown` handler
/// requests a graceful drain.
fn drain_server(io: tokio::io::DuplexStream) -> tokio::task::JoinHandle<Result<(), Error>> {
    drain_server_holding(io, Arc::new(tokio::sync::Notify::new()))
}

/// As [`drain_server`], but `Hold` parks until `hold` is notified — which is
/// how a test keeps a call outstanding, and therefore the drain unsealed, for
/// as long as it needs.
fn drain_server_holding(
    io: tokio::io::DuplexStream,
    hold: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<Result<(), Error>> {
    tokio::spawn(async move {
        quota_builder()
            .server(io)
            .await
            .unwrap()
            .bind::<DrainProtocol>()
            .serve(async move |mut context, request| match request {
                DrainRequest::Big => context.respond(DrainResponse(vec![b'x'; BIG])),
                DrainRequest::Shutdown => {
                    context.shutdown();
                    context.respond(DrainResponse(Vec::new()));
                }
                DrainRequest::Hold => {
                    // Cloned into the handler's own future so the wait does
                    // not hold a borrow of the captured `Arc` across an
                    // await, which the `'static` handler bound rejects.
                    let hold = hold.clone();
                    hold.notified().await;
                    context.respond(DrainResponse(Vec::new()));
                }
            })
            .await
    })
}

#[tokio::test]
async fn graceful_shutdown_delivers_responses_blocked_on_payload_quota() {
    let (client_io, server_io) = tokio::io::duplex(8192);
    let server = drain_server(server_io);
    let client = drain_client(client_io).await;

    // Queued before the shutdown request, so all of them are dispatched — and
    // their responses queued — before the drain starts. Only one fits the
    // quota at a time; the rest can move only as the client reads a response
    // and returns that credit, which requires the server's *receive* half to
    // still be running well after shutdown was asked for.
    let calls: Vec<_> = (0..DRAIN_CALLS)
        .map(|_| client.call(DrainRequest::Big))
        .collect();
    let shutdown = client.call(DrainRequest::Shutdown);

    // Awaited before the shutdown call, and in order, because payload quota
    // is held for the whole call lifecycle: a response's charge is released
    // when its `CallResult` is dropped, so leaving these unread would starve
    // the very credit the server is waiting on. Consuming each one is what
    // lets the next be admitted.
    //
    // Before the drain signal existed, the writer's drain condition ignored
    // quota-blocked sends and the receive half was torn down the moment
    // shutdown was requested, so every response after the first failed here
    // with `ConnectionClosed`.
    for call in calls {
        assert_eq!(call.await.unwrap().into_response().0.len(), BIG);
    }
    assert_eq!(
        shutdown.await.unwrap().into_response(),
        DrainResponse(Vec::new())
    );
    client.close().await;
    assert!(server.await.unwrap().is_ok());
}

#[tokio::test]
async fn graceful_shutdown_waits_for_the_client_transport_to_close() {
    let (client_io, server_io) = tokio::io::duplex(8192);
    let mut server = drain_server(server_io);
    let client = drain_client(client_io).await;

    let response = client.call(DrainRequest::Big);
    let shutdown = client.call(DrainRequest::Shutdown);
    assert_eq!(response.await.unwrap().into_response().0.len(), BIG);
    assert_eq!(
        shutdown.await.unwrap().into_response(),
        DrainResponse(Vec::new())
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut server)
            .await
            .is_err(),
        "server exited before the client closed its transport",
    );

    client.close().await;
    tokio::time::timeout(Duration::from_secs(5), &mut server)
        .await
        .expect("server did not finish after the client transport closed")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_lost_peer_does_not_hang_a_send_blocked_on_payload_quota() {
    // The mirror of the test above: with the receive half gone there is no
    // credit left to wait for, so the writer must abandon what it cannot
    // start rather than draining it. A regression here is a hang, not a
    // wrong answer, so the bound goes around the whole session.
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let server = drain_server(server_io);
        let client = drain_client(client_io).await;
        let calls: Vec<_> = (0..DRAIN_CALLS)
            .map(|_| client.call(DrainRequest::Big))
            .collect();
        // Drops the transport with responses still parked on quota.
        client.close().await;
        for call in calls {
            assert!(call.await.is_err());
        }
        // Returns rather than waiting forever for credit that can no longer
        // arrive.
        let _ = server.await.unwrap();
    })
    .await
    .expect("server hung draining a send that could never be credited");
}

#[tokio::test]
async fn a_drain_refuses_new_calls_but_finishes_the_ones_it_has() {
    let (client_io, server_io) = tokio::io::duplex(8192);
    let hold = Arc::new(tokio::sync::Notify::new());
    let server = drain_server_holding(server_io, hold.clone());
    let client = drain_client(client_io).await;

    let held = client.call(DrainRequest::Hold);
    let shutdown = client.call(DrainRequest::Shutdown);
    // `shutdown` is requested inside `respond`, before the response reaches
    // the wire, so observing the response means the server is already
    // draining — no sleep needed to make the next call land in the window.
    assert_eq!(
        shutdown.await.unwrap().into_response(),
        DrainResponse(Vec::new())
    );

    // Refused rather than dispatched: a drain finishes what it has, it does
    // not take on more. Were it dispatched it would answer with `BIG` bytes.
    assert!(client.call(DrainRequest::Big).await.is_err());

    // The call that was already in flight still answers, and only once it
    // does can the drain seal and the server return.
    hold.notify_one();
    assert_eq!(
        held.await.unwrap().into_response(),
        DrainResponse(Vec::new())
    );
    client.close().await;
    assert!(server.await.unwrap().is_ok());
}
