use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dolang_rpc::{Client, Error, Protocol, Server};
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Response(u32);

#[tokio::test]
async fn multiplexes_out_of_order_calls() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let server = Server::<Test>::new(server_io);
    tokio::spawn(server.serve(async |_, request| match request {
        Request::Echo(value) => Response(value),
        Request::Delay(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Response(ms as u32)
        }
        Request::Shutdown => unreachable!(),
    }));
    let client = Client::<Test>::new(client_io);
    let slow = client.call(Request::Delay(30));
    let fast = client.call(Request::Echo(7));
    assert_eq!(fast.await.unwrap(), Response(7));
    assert_eq!(slow.await.unwrap(), Response(30));
}

#[tokio::test]
async fn unguarded_cancellation_aborts_handler() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let dropped = Arc::new(AtomicBool::new(false));
    let server_dropped = dropped.clone();
    let server = Server::<Test>::new(server_io);
    tokio::spawn(server.serve(async move |_, _| {
        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let guard = SetOnDrop(server_dropped.clone());
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(guard);
        Response(0)
    }));
    let client = Client::<Test>::new(client_io);
    let mut call = client.call(Request::Delay(10_000));
    tokio::time::sleep(Duration::from_millis(10)).await;
    call.cancel();
    assert!(matches!(call.await, Err(Error::Cancelled)));
    assert!(dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn guarded_cancellation_returns_normal_response() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let server = Server::<Test>::new(server_io);
    tokio::spawn(server.serve(async |handle, _| {
        let cancelled = handle
            .cancel_guard(async |_| tokio::time::sleep(Duration::from_secs(10)).await)
            .await
            .is_err();
        Response(u32::from(cancelled))
    }));
    let client = Client::<Test>::new(client_io);
    let mut call = client.call(Request::Delay(10_000));
    tokio::time::sleep(Duration::from_millis(10)).await;
    call.cancel();
    assert_eq!(call.await.unwrap(), Response(1));
}

#[tokio::test]
async fn disconnect_fails_pending_calls() {
    let (client_io, server_io) = tokio::io::duplex(64);
    let client = Client::<Test>::new(client_io);
    let call = client.call(Request::Echo(1));
    drop(server_io);
    assert!(matches!(
        call.await,
        Err(Error::Io(_)) | Err(Error::ConnectionClosed)
    ));
}

#[tokio::test]
async fn close_stops_tasks_and_fails_pending_calls() {
    let (client_io, _peer_io) = tokio::io::duplex(64);
    let client = Client::<Test>::new(client_io);
    let call = client.call(Request::Echo(1));
    client.close().await;
    assert!(matches!(call.await, Err(Error::ConnectionClosed)));
}

#[tokio::test]
async fn server_shutdown_drains_outstanding_requests() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let server = Server::<Test>::new(server_io);
    let server = tokio::spawn(server.serve(async |context, request| match request {
        Request::Echo(value) => Response(value),
        Request::Delay(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Response(ms as u32)
        }
        Request::Shutdown => {
            context.shutdown();
            Response(99)
        }
    }));
    let client = Client::<Test>::new(client_io);
    let slow = client.call(Request::Delay(20));
    let shutdown = client.call(Request::Shutdown);
    assert_eq!(shutdown.await.unwrap(), Response(99));
    assert_eq!(slow.await.unwrap(), Response(20));
    assert!(server.await.unwrap().is_ok());
    client.close().await;
}

#[cfg(unix)]
mod unix_handles {
    use std::{io::Read, os::fd::AsFd};

    use dolang_rpc::OsHandle;
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
        handle: Option<OsHandle>,
    }

    #[tokio::test]
    async fn transfers_handles_in_requests_and_responses() {
        let (client_stream, server_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = Server::<HandlesProtocol>::from_unix_stream(server_stream).unwrap();
        tokio::spawn(server.serve(async |_, mut request| HandleResponse {
            handle: request.handles.pop(),
        }));
        let client = Client::<HandlesProtocol>::from_unix_stream(client_stream).unwrap();
        let (read_fd, write_fd) = pipe().unwrap();
        let call = client.call(HandleRequest {
            handles: vec![OsHandle::new(read_fd)],
        });
        let response = call.await.unwrap();
        let received = response.handle.unwrap().into_inner();
        write(&write_fd, b"ok").unwrap();
        let mut file = std::fs::File::from(received);
        let mut bytes = [0; 2];
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
    }

    #[test]
    fn os_handle_keeps_its_descriptor_borrowable() {
        let (fd, _) = pipe().unwrap();
        let handle = OsHandle::new(fd);
        let _ = handle.as_inner().as_fd();
    }
}

#[cfg(windows)]
mod windows_handles {
    use std::{
        io::Read,
        os::windows::io::{AsHandle, FromRawHandle, OwnedHandle},
        sync::atomic::{AtomicU64, Ordering},
    };

    use dolang_rpc::OsHandle;
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
        let server = Server::<HandlesProtocol>::from_named_pipe_client(server_pipe).unwrap();
        let server = tokio::spawn(server.serve(async |_, request| HandleResponse {
            handle: request.handle,
        }));
        // SAFETY: this test owns and controls the connected server endpoint.
        let client = unsafe {
            Client::<HandlesProtocol>::from_named_pipe_server(client_pipe, current_process_handle())
                .unwrap()
        };

        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let _ = file.as_handle();
        let response = client
            .call(HandleRequest {
                handle: OsHandle::new(OwnedHandle::from(file)),
            })
            .await;
        let response = match response {
            Ok(response) => response,
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
