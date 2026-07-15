use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
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
    pending: Mutex<Pending<P::Response>>,
    next_id: Mutex<u64>,
    tasks: Mutex<Option<Tasks>>,
    request_keepalive: Mutex<HashMap<u64, P::Request>>,
    #[cfg(windows)]
    _peer_process: Option<OwnedHandle>,
}

struct Writer<P: Protocol> {
    transport: transport::AnySender,
    outgoing: mpsc::UnboundedReceiver<Message<P::Request>>,
    inner: Weak<Inner<P>>,
    keep_requests_alive: bool,
}

struct Reader<P: Protocol> {
    transport: transport::AnyReceiver,
    inner: Weak<Inner<P>>,
    max_frame_size: usize,
    buffered: BytesMut,
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
        self.fail(Error::ConnectionClosed);
    }
}

impl<P: Protocol> Inner<P> {
    fn complete(&self, id: u64, result: Result<P::Response, Error>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(result);
        }
    }

    fn fail(&self, error: Error) {
        for (_, tx) in std::mem::take(&mut *self.pending.lock().unwrap()) {
            let _ = tx.send(Err(error.copy()));
        }
    }
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
    /// Returns whether both clients refer to the same RPC session.
    pub fn is_same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Starts a client session on a bidirectional byte stream.
    pub fn new<T>(stream: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::with_max_frame_size(stream, DEFAULT_MAX_FRAME_SIZE)
    }

    /// Starts a client session on separate byte-stream reader and writer halves.
    pub fn new_split<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let (sender, receiver) = transport::generic(reader, writer);
        Self::from_transport(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
            DEFAULT_MAX_FRAME_SIZE,
            false,
            #[cfg(windows)]
            None,
        )
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
        let inner = Arc::new(Inner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
            tasks: Mutex::new(None),
            request_keepalive: Mutex::new(HashMap::new()),
            #[cfg(windows)]
            _peer_process: peer_process,
        });
        let (writer_shutdown, writer_stop) = oneshot::channel();
        let (reader_shutdown, reader_stop) = oneshot::channel();
        let writer = tokio::spawn(
            Writer {
                transport: sender,
                outgoing: outgoing_rx,
                inner: Arc::downgrade(&inner),
                keep_requests_alive,
            }
            .run(writer_stop),
        );
        let reader = tokio::spawn(
            Reader {
                transport: receiver,
                inner: Arc::downgrade(&inner),
                max_frame_size,
                buffered: BytesMut::with_capacity(8192),
            }
            .run(reader_stop),
        );
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
        self.inner.fail(Error::ConnectionClosed);
        if let Some(tasks) = tasks {
            tasks.join().await;
        }
    }

    /// Begins one request.
    pub fn call(&self, request: P::Request) -> Call<P> {
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
                inner: self.inner.clone(),
                cancel_sent: true,
            };
        }
        self.inner.pending.lock().unwrap().insert(id, tx);
        let queued = self
            .inner
            .outgoing
            .send(Message::Request { id, value: request })
            .is_ok();
        drop(tasks);
        if !queued {
            self.inner.complete(id, Err(Error::ConnectionClosed));
        }
        Call {
            id,
            rx,
            inner: self.inner.clone(),
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
pub struct Call<P: Protocol> {
    id: u64,
    rx: oneshot::Receiver<Result<P::Response, Error>>,
    inner: Arc<Inner<P>>,
    cancel_sent: bool,
}

impl<P: Protocol> Call<P> {
    /// Requests cancellation. The call remains awaitable.
    pub fn cancel(&mut self) {
        if !self.cancel_sent {
            self.cancel_sent = true;
            let _ = self.inner.outgoing.send(Message::Cancel { id: self.id });
        }
    }
}

impl<P: Protocol> Future for Call<P> {
    type Output = Result<P::Response, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(Ok(response))) => Poll::Ready(Ok(response)),
            Poll::Ready(Ok(Err(e))) => Poll::Ready(Err(e)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(Error::ConnectionClosed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<P: Protocol> Drop for Call<P> {
    fn drop(&mut self) {
        self.cancel();
        self.inner.pending.lock().unwrap().remove(&self.id);
    }
}

impl<P: Protocol> Writer<P> {
    async fn handle_request(&mut self, id: u64, value: P::Request) -> bool {
        let mut frame = self.transport.send();
        let result = match encode(Kind::Request, id, &value, &mut frame) {
            Ok(mut message) => frame.finish(&mut message).await.map_err(Error::from),
            Err(err) => {
                if let Some(inner) = self.inner.upgrade() {
                    inner.complete(id, Err(err));
                } else {
                    return true;
                }
                return false;
            }
        };
        let Some(inner) = self.inner.upgrade() else {
            return true;
        };
        if result.is_ok() && self.keep_requests_alive {
            inner.request_keepalive.lock().unwrap().insert(id, value);
        }
        if let Err(err) = result {
            inner.complete(id, Err(err));
            return true;
        }
        false
    }

    async fn run(mut self, mut shutdown: oneshot::Receiver<()>) {
        loop {
            let outgoing = tokio::select! {
                outgoing = self.outgoing.recv() => outgoing,
                _ = &mut shutdown => return,
            };
            let Some(outgoing) = outgoing else {
                return;
            };
            let exit = tokio::select! {
                result = async {
                    match outgoing {
                        Message::Request { id, value } => self.handle_request(id, value).await,
                        Message::Cancel { id } => {
                            let mut message = encode_empty(Kind::Cancel, id);
                            self.transport
                                .send()
                                .finish(&mut message).await.is_err()
                        }
                    }
                } => result,
                _ = &mut shutdown => return,
            };
            if exit {
                break;
            }
        }
    }
}

impl<P: Protocol> Reader<P> {
    async fn run(mut self, mut shutdown: oneshot::Receiver<()>) {
        loop {
            let mut frame = self.transport.recv();
            let message = tokio::select! {
                message = read_message(&mut frame, &mut self.buffered, self.max_frame_size) => message,
                _ = &mut shutdown => return,
            };
            let Some(inner) = self.inner.upgrade() else {
                return;
            };
            match message {
                Ok((Kind::Response, id, payload)) => match decode(&payload, &mut frame) {
                    Ok(response) => {
                        inner.request_keepalive.lock().unwrap().remove(&id);
                        inner.complete(id, Ok(response));
                    }
                    Err(error) => {
                        inner.fail(error);
                        return;
                    }
                },
                Ok((Kind::Error, id, _)) => {
                    inner.request_keepalive.lock().unwrap().remove(&id);
                    inner.complete(id, Err(Error::Cancelled));
                }
                Ok((kind, _, _)) => {
                    inner.fail(Error::Protocol(format!("unexpected {kind:?} frame")));
                    return;
                }
                Err(error) => {
                    inner.fail(error);
                    return;
                }
            }
        }
    }
}
