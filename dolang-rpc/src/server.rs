use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use bytes::BytesMut;
use futures::{
    StreamExt,
    future::{AbortHandle, Abortable},
};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};

use crate::{
    DEFAULT_MAX_FRAME_SIZE, Error, InvalidOpaque, Kind, Opaque, OpaqueGuard, OpaqueResource,
    Protocol, decode, encode, encode_empty, opaque, read_message,
    transport::{self, Receiver, SendFrame, Sender},
};

/// A server endpoint for one connection.
pub struct Server<P: Protocol> {
    sender: transport::AnySender,
    receiver: transport::AnyReceiver,
    outgoing: mpsc::UnboundedSender<Message<P::Response>>,
    outgoing_rx: mpsc::UnboundedReceiver<Message<P::Response>>,
    inner: Arc<Mutex<Inner>>,
    max: usize,
    marker: PhantomData<fn() -> P>,
}

enum Message<R> {
    Response { id: u64, value: R },
    Error { id: u64 },
}

struct Inner {
    outstanding: HashMap<u64, Cancellation>,
    objects: opaque::ObjectTable,
    shutdown: Option<oneshot::Sender<()>>,
}

struct Cancellation {
    signal: Option<oneshot::Sender<()>>,
    abort: AbortHandle,
}

impl<P: Protocol> Server<P> {
    /// Creates a server over a bidirectional byte stream.
    pub fn new<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(stream: T) -> Self {
        let (sender, receiver) = transport::generic_duplex(stream);
        Self::from_transport(
            transport::AnySender::Generic(sender),
            transport::AnyReceiver::Generic(receiver),
        )
    }

    #[cfg(unix)]
    pub fn from_unix_stream(stream: std::os::unix::net::UnixStream) -> std::io::Result<Self> {
        let (sender, receiver) = transport::unix::unix(stream)?;
        Ok(Self::from_transport(
            transport::AnySender::Unix(sender),
            transport::AnyReceiver::Unix(receiver),
        ))
    }

    #[cfg(windows)]
    pub fn from_named_pipe_server(pipe: NamedPipeServer) -> std::io::Result<Self> {
        let (sender, receiver) = transport::windows::server_pipe(pipe, true)?;
        Ok(Self::from_transport(
            transport::AnySender::Windows(sender),
            transport::AnyReceiver::Windows(receiver),
        ))
    }

    #[cfg(windows)]
    pub fn from_named_pipe_client(pipe: NamedPipeClient) -> std::io::Result<Self> {
        let (sender, receiver) = transport::windows::client_pipe(pipe, true)?;
        Ok(Self::from_transport(
            transport::AnySender::Windows(sender),
            transport::AnyReceiver::Windows(receiver),
        ))
    }

    fn from_transport(sender: transport::AnySender, receiver: transport::AnyReceiver) -> Self {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            outgoing,
            outgoing_rx,
            inner: Arc::new(Mutex::new(Inner {
                outstanding: HashMap::new(),
                objects: opaque::ObjectTable::default(),
                shutdown: None,
            })),
            max: DEFAULT_MAX_FRAME_SIZE,
            marker: PhantomData,
        }
    }

    /// Serves requests until the peer disconnects or the session fails.
    pub async fn serve<H>(self, handler: H) -> Result<(), Error>
    where
        H: AsyncFn(&mut CallContext<P>, P::Request) -> P::Response + Send + Sync + 'static,
    {
        let Server {
            sender,
            mut receiver,
            outgoing,
            outgoing_rx,
            inner,
            max,
            marker: _,
        } = self;
        let (writer_shutdown, writer_stop) = oneshot::channel();
        let mut writer = tokio::spawn(writer::<P>(sender, outgoing_rx, writer_stop));
        let (shutdown, mut shutdown_requested) = oneshot::channel();
        inner.lock().unwrap().shutdown = Some(shutdown);
        let handler = Arc::new(handler);
        let mut buffered = BytesMut::with_capacity(8192);
        let mut tasks = futures::stream::FuturesUnordered::new();
        let (mut result, mut writer_finished, mut graceful) = loop {
            let mut frame = receiver.recv();
            let message = tokio::select! {
                message = read_message(&mut frame, &mut buffered, max) => match message {
                    Ok(message) => message,
                    Err(error) => break (Err(error), false, false),
                },
                Some(_) = tasks.next(), if !tasks.is_empty() => continue,
                _ = &mut shutdown_requested => break (Ok(()), false, true),
                result = &mut writer => {
                    let result = match result {
                        Ok(result) => result,
                        Err(error) => Err(Error::Protocol(format!("server writer task failed: {error}"))),
                    };
                    break (result, true, false);
                }
            };
            let (kind, id, payload) = message;
            match kind {
                Kind::Request => {
                    let request = match decode(&payload, &mut frame) {
                        Ok(request) => request,
                        Err(error) => break (Err(error), false, false),
                    };
                    let handler = handler.clone();
                    let task_inner = inner.clone();
                    let task_outgoing = outgoing.clone();
                    let (abort, registration) = AbortHandle::new_pair();
                    tasks.push(Abortable::new(
                        async move {
                            let mut context = CallContext {
                                id,
                                inner: task_inner.clone(),
                                marker: PhantomData,
                            };
                            let response = handler(&mut context, request).await;
                            task_inner.lock().unwrap().outstanding.remove(&id);
                            let _ = task_outgoing.send(Message::Response {
                                id,
                                value: response,
                            });
                        },
                        registration,
                    ));
                    inner.lock().unwrap().outstanding.insert(
                        id,
                        Cancellation {
                            signal: None,
                            abort,
                        },
                    );
                }
                Kind::Cancel => {
                    let mut state = inner.lock().unwrap();
                    if let Some(signal) = state
                        .outstanding
                        .get_mut(&id)
                        .and_then(|cancel| cancel.signal.take())
                    {
                        let _ = signal.send(());
                    } else if let Some(cancel) = state.outstanding.remove(&id) {
                        cancel.abort.abort();
                        let _ = outgoing.send(Message::Error { id });
                    }
                }
                _ => {
                    break (
                        Err(Error::Protocol(format!("unexpected {kind:?} frame"))),
                        false,
                        false,
                    );
                }
            }
        };
        drop(receiver);
        if graceful {
            while !tasks.is_empty() {
                tokio::select! {
                    Some(_) = tasks.next() => {}
                    writer_result = &mut writer => {
                        result = match writer_result {
                            Ok(result) => result,
                            Err(error) => Err(Error::Protocol(format!("server writer task failed: {error}"))),
                        };
                        writer_finished = true;
                        graceful = false;
                    }
                }
                if !graceful {
                    break;
                }
            }
        }
        drop(outgoing);
        drop(tasks);
        if !writer_finished {
            if graceful {
                result = match writer.await {
                    Ok(writer_result) => writer_result,
                    Err(error) => Err(Error::Protocol(format!(
                        "server writer task failed: {error}"
                    ))),
                };
            } else {
                let _ = writer_shutdown.send(());
                let _ = writer.await;
            }
        }
        result
    }
}

async fn writer<P: Protocol>(
    mut sender: transport::AnySender,
    mut outgoing: mpsc::UnboundedReceiver<Message<P::Response>>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), Error> {
    loop {
        let outgoing = tokio::select! {
            outgoing = outgoing.recv() => outgoing,
            _ = &mut shutdown => return Ok(()),
        };
        let Some(outgoing) = outgoing else {
            return Ok(());
        };
        let result = async {
            match outgoing {
                Message::Response { id, value } => {
                    let mut frame = sender.send();
                    let mut message = encode(Kind::Response, id, &value, &mut frame)?;
                    frame.finish(&mut message).await?;
                }
                Message::Error { id } => {
                    let mut message = encode_empty(Kind::Error, id);
                    sender.send().finish(&mut message).await?;
                }
            }
            Ok::<(), Error>(())
        };
        tokio::select! {
            result = result => result?,
            _ = &mut shutdown => return Ok(()),
        }
    }
}

/// Services available while processing one request.
pub struct CallContext<P> {
    id: u64,
    inner: Arc<Mutex<Inner>>,
    marker: PhantomData<fn() -> P>,
}

impl<P> CallContext<P> {
    /// Stops accepting requests and gracefully drains the connection.
    pub fn shutdown(&self) {
        if let Some(shutdown) = self.inner.lock().unwrap().shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    /// Runs an operation which can observe request cancellation without dropping the handler.
    pub async fn cancel_guard<T, F>(&mut self, operation: F) -> Result<T, RequestCancelled>
    where
        F: AsyncFnOnce(&mut CallContext<P>) -> T,
    {
        struct Reset {
            id: u64,
            inner: Arc<Mutex<Inner>>,
        }
        impl Drop for Reset {
            fn drop(&mut self) {
                if let Some(cancel) = self.inner.lock().unwrap().outstanding.get_mut(&self.id) {
                    cancel.signal = None;
                }
            }
        }
        let (signal, cancelled) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap();
            let cancel = inner
                .outstanding
                .get_mut(&self.id)
                .expect("call context is not registered");
            assert!(cancel.signal.is_none(), "cancel guard is already active");
            cancel.signal = Some(signal);
        }
        let _reset = Reset {
            id: self.id,
            inner: self.inner.clone(),
        };
        let future = operation(&mut *self);
        tokio::pin!(future);
        tokio::select! {
            value = &mut future => Ok(value),
            result = cancelled => match result { Ok(()) => Err(RequestCancelled), Err(_) => Ok(future.await) },
        }
    }

    pub fn register<T: OpaqueResource>(&self, value: T) -> Opaque<T::Marker> {
        self.inner.lock().unwrap().objects.register(value)
    }

    pub fn acquire<T: OpaqueResource>(
        &self,
        value: Opaque<T::Marker>,
    ) -> Result<OpaqueGuard<T>, InvalidOpaque> {
        self.inner.lock().unwrap().objects.acquire(value)
    }

    pub fn unregister<M: ?Sized + 'static>(&self, value: Opaque<M>) -> Result<(), InvalidOpaque> {
        self.inner.lock().unwrap().objects.unregister(value)
    }
}

/// Indicates that a guarded operation was interrupted by request cancellation.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("request cancelled")]
pub struct RequestCancelled;
