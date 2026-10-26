//! The HTTP side of a mock server: the accept loop, its connections, and the
//! hand-off of each request to the server's dispatcher strand (see
//! ARCHITECTURE.md). Nothing here touches the VM.

use std::future::Future;

use bytes::Bytes;
use futures::{
    StreamExt as _, channel::mpsc as local, future::LocalBoxFuture, stream::FuturesUnordered,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::{body::Incoming, rt::Executor, service::service_fn};
use hyper_util::{rt::TokioIo, server::conn::auto};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};

/// A request the server received, with its body read in full. Doubles as
/// [`RequestObject`](crate::mock::RequestObject)'s annex.
pub(crate) struct RequestSnapshot {
    pub(crate) method: Method,
    pub(crate) url: url::Url,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

#[derive(Clone)]
pub(crate) struct Response {
    pub(crate) status: StatusCode,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) body: Bytes,
}

impl Response {
    pub(crate) fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn into_hyper(self) -> hyper::Response<Full<Bytes>> {
        let mut response = hyper::Response::new(Full::new(self.body));
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        for (name, value) in self.headers {
            headers.insert(name, value);
        }
        response
    }
}

/// One request sent to the dispatcher strand, which replies with the response.
pub(crate) struct Dispatch {
    pub(crate) request: RequestSnapshot,
    pub(crate) reply: oneshot::Sender<Response>,
}

/// Runs HTTP/2 stream tasks inside [`serve`]'s own task set, so no hyper
/// future is ever handed to the VM.
#[derive(Clone)]
struct LocalExec(local::UnboundedSender<LocalBoxFuture<'static, ()>>);

impl<F: Future<Output = ()> + 'static> Executor<F> for LocalExec {
    fn execute(&self, fut: F) {
        let _ = self.0.unbounded_send(Box::pin(fut));
    }
}

/// Accepts and serves connections until `shutdown` resolves (sent or dropped)
/// or the dispatcher strand drops its receiver. Every connection is dropped on
/// exit, and so is `stopped`, which tells `close` the listener is gone.
pub(crate) async fn serve(
    listener: TcpListener,
    dispatch: mpsc::UnboundedSender<Dispatch>,
    mut shutdown: oneshot::Receiver<()>,
    stopped: oneshot::Sender<()>,
) {
    let (exec_tx, mut exec_rx) = local::unbounded();
    let exec = LocalExec(exec_tx);
    let mut tasks = FuturesUnordered::<LocalBoxFuture<'static, ()>>::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = dispatch.closed() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tasks.push(Box::pin(connection(stream, exec.clone(), dispatch.clone())));
                }
                Err(_) => break,
            },
            Some(task) = exec_rx.next() => tasks.push(task),
            Some(()) = tasks.next(), if !tasks.is_empty() => {}
        }
    }
    drop(tasks);
    drop(listener);
    drop(stopped);
}

async fn connection(stream: TcpStream, exec: LocalExec, dispatch: mpsc::UnboundedSender<Dispatch>) {
    let builder = auto::Builder::new(exec);
    let service = service_fn(move |request| handle(request, dispatch.clone()));
    let _ = builder
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

async fn handle(
    request: hyper::Request<Incoming>,
    dispatch: mpsc::UnboundedSender<Dispatch>,
) -> Result<hyper::Response<Full<Bytes>>, hyper::Error> {
    let (parts, body) = request.into_parts();
    // A body that fails to arrive fails the service, and hyper drops the
    // connection.
    let body = body.collect().await?.to_bytes();
    let url = match parts.uri.authority() {
        Some(_) => parts.uri.to_string(),
        None => format!("http://localhost{}", parts.uri),
    };
    let Ok(url) = url::Url::parse(&url) else {
        return Ok(Response::new(StatusCode::BAD_REQUEST, Bytes::new()).into_hyper());
    };
    let (reply, response) = oneshot::channel();
    let request = RequestSnapshot {
        method: parts.method,
        url,
        headers: parts.headers,
        body,
    };
    let closed = || {
        Response::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "http.mock: server closed",
        )
    };
    if dispatch.send(Dispatch { request, reply }).is_err() {
        return Ok(closed().into_hyper());
    }
    Ok(response.await.unwrap_or_else(|_| closed()).into_hyper())
}
