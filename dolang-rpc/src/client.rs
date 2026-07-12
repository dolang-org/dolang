use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, OwnedHandle};

#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};

#[cfg(windows)]
use windows_sys::Win32::System::Threading::GetProcessId;

use crate::{
    DEFAULT_MAX_FRAME_SIZE, Error, Kind, Protocol, decode, encode, encode_empty, read_message,
    transport::{self, Receiver, SendFrame, Sender},
};

type Pending<R> = HashMap<u64, oneshot::Sender<Result<R, Error>>>;

enum Message<Q> {
    Request { id: u64, value: Q },
    Cancel { id: u64 },
}

struct Inner<P: Protocol> {
    outgoing: mpsc::UnboundedSender<Message<P::Request>>,
    core: Arc<Core<P::Response>>,
    next_id: Mutex<u64>,
    tasks: Mutex<Option<Tasks>>,
    request_keepalive: Arc<Mutex<HashMap<u64, P::Request>>>,
    #[cfg(windows)]
    _peer_process: Option<OwnedHandle>,
}

struct Tasks {
    writer_shutdown: Option<oneshot::Sender<()>>,
    reader_shutdown: Option<oneshot::Sender<()>>,
    writer: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<()>,
}

impl Tasks {
    fn shutdown(&mut self) {
        if let Some(shutdown) = self.writer_shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(shutdown) = self.reader_shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    async fn join(mut self) {
        self.shutdown();
        let _ = tokio::join!(self.writer, self.reader);
    }
}

impl<P: Protocol> Drop for Inner<P> {
    fn drop(&mut self) {
        if let Some(tasks) = self.tasks.get_mut().unwrap().as_mut() {
            tasks.shutdown();
        }
        fail(&self.core, Error::ConnectionClosed);
    }
}

struct Core<R> {
    cancel: Arc<dyn Fn(u64) + Send + Sync>,
    pending: Mutex<Pending<R>>,
}

/// A cloneable request endpoint.
pub struct Client<P: Protocol> {
    inner: Arc<Inner<P>>,
}

impl<P: Protocol> Clone for Client<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<P: Protocol> Client<P> {
    /// Starts a client session on a bidirectional byte stream.
    pub fn new<T>(stream: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::with_max_frame_size(stream, DEFAULT_MAX_FRAME_SIZE)
    }

    /// Starts a client session with an explicit maximum inbound payload size.
    pub fn with_max_frame_size<T>(stream: T, max_frame_size: usize) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, receiver) = transport::generic_duplex(stream);
        Self::from_transport(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
            max_frame_size,
            false,
            #[cfg(windows)]
            None,
        )
    }

    #[cfg(unix)]
    pub fn from_unix_stream(stream: UnixStream) -> io::Result<Self> {
        let (sender, receiver) = transport::unix::unix(stream)?;
        Ok(Self::from_transport(
            transport::AnySender::Unix(sender),
            transport::AnyReceiver::Unix(receiver),
            DEFAULT_MAX_FRAME_SIZE,
            false,
            #[cfg(windows)]
            None,
        ))
    }

    #[cfg(windows)]
    /// Starts a client session on the server end of a Windows named pipe.
    ///
    /// `peer_process` is retained for the lifetime of the session and must
    /// grant process-query and synchronization access. Construction fails if
    /// it does not identify the named-pipe peer.
    ///
    /// # Safety
    ///
    /// The identified peer must be trusted to send only handle values that it
    /// created in this process with `DuplicateHandle`. A malicious peer can
    /// otherwise cause this process to close arbitrary handles.
    pub unsafe fn from_named_pipe_server(
        pipe: NamedPipeServer,
        peer_process: OwnedHandle,
    ) -> io::Result<Self> {
        validate_peer_process(
            &peer_process,
            transport::windows::server_pipe_peer_pid(&pipe)?,
        )?;
        let (sender, receiver) = transport::windows::server_pipe(pipe, false)?;
        Ok(Self::from_transport(
            transport::AnySender::Windows(sender),
            transport::AnyReceiver::Windows(receiver),
            DEFAULT_MAX_FRAME_SIZE,
            true,
            Some(peer_process),
        ))
    }

    #[cfg(windows)]
    /// Starts a client session on the client end of a Windows named pipe.
    ///
    /// `peer_process` is retained for the lifetime of the session and must
    /// grant process-query and synchronization access. Construction fails if
    /// it does not identify the named-pipe peer.
    ///
    /// # Safety
    ///
    /// The identified peer must be trusted to send only handle values that it
    /// created in this process with `DuplicateHandle`. A malicious peer can
    /// otherwise cause this process to close arbitrary handles.
    pub unsafe fn from_named_pipe_client(
        pipe: NamedPipeClient,
        peer_process: OwnedHandle,
    ) -> io::Result<Self> {
        validate_peer_process(
            &peer_process,
            transport::windows::client_pipe_peer_pid(&pipe)?,
        )?;
        let (sender, receiver) = transport::windows::client_pipe(pipe, false)?;
        Ok(Self::from_transport(
            transport::AnySender::Windows(sender),
            transport::AnyReceiver::Windows(receiver),
            DEFAULT_MAX_FRAME_SIZE,
            true,
            Some(peer_process),
        ))
    }

    fn from_transport(
        sender: transport::AnySender,
        receiver: transport::AnyReceiver,
        max_frame_size: usize,
        keep_requests_alive: bool,
        #[cfg(windows)] peer_process: Option<OwnedHandle>,
    ) -> Self {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        let cancel_outgoing = outgoing.clone();
        let core = Arc::new(Core {
            cancel: Arc::new(move |id| {
                let _ = cancel_outgoing.send(Message::Cancel { id });
            }),
            pending: Mutex::new(HashMap::new()),
        });
        let inner = Arc::new(Inner {
            outgoing,
            core: core.clone(),
            next_id: Mutex::new(0),
            tasks: Mutex::new(None),
            request_keepalive: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(windows)]
            _peer_process: peer_process,
        });
        let (writer_shutdown, writer_stop) = oneshot::channel();
        let (reader_shutdown, reader_stop) = oneshot::channel();
        let writer = tokio::spawn(writer::<P>(
            sender,
            outgoing_rx,
            core.clone(),
            inner.request_keepalive.clone(),
            keep_requests_alive,
            writer_stop,
        ));
        let reader = tokio::spawn(reader::<P>(
            receiver,
            core,
            inner.request_keepalive.clone(),
            max_frame_size,
            reader_stop,
        ));
        *inner.tasks.lock().unwrap() = Some(Tasks {
            writer_shutdown: Some(writer_shutdown),
            reader_shutdown: Some(reader_shutdown),
            writer,
            reader,
        });
        Self { inner }
    }

    /// Stops the session and waits for its background tasks to exit.
    pub async fn close(self) {
        let tasks = self.inner.tasks.lock().unwrap().take();
        fail(&self.inner.core, Error::ConnectionClosed);
        if let Some(tasks) = tasks {
            tasks.join().await;
        }
    }

    /// Begins one request.
    pub fn call(&self, request: P::Request) -> Call<P::Response> {
        let (tx, rx) = oneshot::channel();
        let tasks = self.inner.tasks.lock().unwrap();
        let id = {
            let mut next = self.inner.next_id.lock().unwrap();
            let id = *next;
            *next = id.checked_add(1).expect("request identifiers exhausted");
            id
        };
        if tasks.is_none() {
            let _ = tx.send(Err(Error::ConnectionClosed));
            return Call {
                id,
                rx,
                inner: self.inner.core.clone(),
                cancel_sent: true,
            };
        }
        self.inner.core.pending.lock().unwrap().insert(id, tx);
        let queued = self
            .inner
            .outgoing
            .send(Message::Request { id, value: request })
            .is_ok();
        drop(tasks);
        if !queued {
            complete(&self.inner.core, id, Err(Error::ConnectionClosed));
        }
        Call {
            id,
            rx,
            inner: self.inner.core.clone(),
            cancel_sent: !queued,
        }
    }
}

#[cfg(windows)]
fn validate_peer_process(peer_process: &OwnedHandle, pipe_peer_pid: u32) -> io::Result<()> {
    let process_pid = unsafe { GetProcessId(peer_process.as_raw_handle() as _) };
    if process_pid == 0 {
        return Err(io::Error::last_os_error());
    }
    if process_pid != pipe_peer_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named-pipe peer does not match the expected process",
        ));
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::io::FromRawHandle;

    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    use super::*;

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

    #[test]
    fn validates_named_pipe_peer_process() {
        let process = current_process_handle();
        let pid = unsafe { GetCurrentProcessId() };
        validate_peer_process(&process, pid).unwrap();
        assert_eq!(
            validate_peer_process(&process, !pid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}

/// An in-progress RPC request.
pub struct Call<R> {
    id: u64,
    rx: oneshot::Receiver<Result<R, Error>>,
    inner: Arc<Core<R>>,
    cancel_sent: bool,
}

impl<R> Call<R> {
    /// Requests cancellation. The call remains awaitable.
    pub fn cancel(&mut self) {
        if !self.cancel_sent {
            self.cancel_sent = true;
            (self.inner.cancel)(self.id);
        }
    }
}

impl<R> Future for Call<R> {
    type Output = Result<R, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(Ok(response))) => Poll::Ready(Ok(response)),
            Poll::Ready(Ok(Err(e))) => Poll::Ready(Err(e)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(Error::ConnectionClosed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<R> Drop for Call<R> {
    fn drop(&mut self) {
        self.cancel();
        self.inner.pending.lock().unwrap().remove(&self.id);
    }
}

fn complete<R>(inner: &Core<R>, id: u64, result: Result<R, Error>) {
    if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
        let _ = tx.send(result);
    }
}

fn fail<R>(inner: &Core<R>, error: Error) {
    for (_, tx) in std::mem::take(&mut *inner.pending.lock().unwrap()) {
        let _ = tx.send(Err(error.copy()));
    }
}

async fn writer<P: Protocol>(
    mut transport: transport::AnySender,
    mut outgoing: mpsc::UnboundedReceiver<Message<P::Request>>,
    inner: Arc<Core<P::Response>>,
    request_keepalive: Arc<Mutex<HashMap<u64, P::Request>>>,
    keep_requests_alive: bool,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        let outgoing = tokio::select! {
            outgoing = outgoing.recv() => outgoing,
            _ = &mut shutdown => return,
        };
        let Some(outgoing) = outgoing else {
            return;
        };
        let result = tokio::select! {
            result = async {
                match outgoing {
                    Message::Request { id, value } => {
                        let mut frame = transport.send();
                        let result = match encode(Kind::Request, id, &value, &mut frame) {
                            Ok(mut message) => frame.finish(&mut message).await.map_err(Error::from),
                            Err(error) => {
                                drop(frame);
                                Err(error)
                            }
                        };
                        if result.is_ok() && keep_requests_alive {
                            request_keepalive.lock().unwrap().insert(id, value);
                        }
                        result
                    }
                    Message::Cancel { id } => {
                        let mut message = encode_empty(Kind::Cancel, id);
                        transport
                            .send()
                            .finish(&mut message)
                            .await
                            .map_err(Error::from)
                    }
                }
            } => result,
            _ = &mut shutdown => return,
        };
        if let Err(error) = result {
            fail(&inner, error);
            return;
        }
    }
}

async fn reader<P: Protocol>(
    mut transport: transport::AnyReceiver,
    inner: Arc<Core<P::Response>>,
    request_keepalive: Arc<Mutex<HashMap<u64, P::Request>>>,
    max: usize,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut buffered = BytesMut::with_capacity(8192);
    loop {
        let mut frame = transport.recv();
        let message = tokio::select! {
            message = read_message(&mut frame, &mut buffered, max) => message,
            _ = &mut shutdown => return,
        };
        match message {
            Ok((Kind::Response, id, payload)) => match decode(&payload, &mut frame) {
                Ok(response) => {
                    request_keepalive.lock().unwrap().remove(&id);
                    complete(&inner, id, Ok(response));
                }
                Err(error) => {
                    fail(&inner, error);
                    return;
                }
            },
            Ok((Kind::Error, id, _)) => {
                request_keepalive.lock().unwrap().remove(&id);
                complete(&inner, id, Err(Error::Cancelled));
            }
            Ok((kind, _, _)) => {
                fail(
                    &inner,
                    Error::Protocol(format!("unexpected {kind:?} frame")),
                );
                return;
            }
            Err(error) => {
                fail(&inner, error);
                return;
            }
        }
    }
}
