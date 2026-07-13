use std::{
    io,
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::{
    io::{PipeReader as StdPipeReader, PipeWriter as StdPipeWriter, Read as _, Write as _},
    os::windows::io::OwnedHandle,
    sync::Arc,
};

use dolang_rpc::DefaultHandle;
#[cfg(windows)]
use tokio::task::JoinHandle;

pub fn pipe() -> io::Result<(PipeSend, PipeRecv)> {
    #[cfg(unix)]
    {
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        Ok((PipeSend(send), PipeRecv(recv)))
    }

    #[cfg(windows)]
    {
        let (recv, send) = std::io::pipe()?;
        Ok((PipeSend::new(send), PipeRecv::new(recv)))
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct PipeSend(tokio::net::unix::pipe::Sender);

#[cfg(unix)]
#[derive(Debug)]
pub struct PipeRecv(tokio::net::unix::pipe::Receiver);

#[cfg(windows)]
#[derive(Debug)]
pub struct PipeSend {
    inner: Arc<StdPipeWriter>,
    pending: Option<JoinHandle<io::Result<usize>>>,
}

#[cfg(windows)]
#[derive(Debug)]
pub struct PipeRecv {
    inner: Arc<StdPipeReader>,
    pending: Option<JoinHandle<(Vec<u8>, io::Result<usize>)>>,
    ready: Option<(Vec<u8>, usize)>,
}

impl PipeSend {
    pub fn try_clone(&self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = self.0.as_fd().try_clone_to_owned()?;
            Ok(Self(
                tokio::net::unix::pipe::Sender::from_owned_fd_unchecked(fd)?,
            ))
        }

        #[cfg(windows)]
        {
            Ok(Self {
                inner: Arc::new(self.inner.try_clone()?),
                pending: None,
            })
        }
    }

    pub fn into_stdio(self) -> io::Result<Stdio> {
        #[cfg(unix)]
        {
            let fd: OwnedFd = self.0.into_blocking_fd()?;
            Ok(Stdio::from(fd))
        }

        #[cfg(windows)]
        {
            if self.pending.is_some() {
                return Err(io::Error::other(
                    "cannot convert PipeSend into Stdio while an async write is in flight",
                ));
            }

            Arc::try_unwrap(self.inner)
                .or_else(|inner| inner.try_clone())
                .map(Stdio::from)
        }
    }

    pub fn into_blocking_handle(self) -> io::Result<DefaultHandle> {
        #[cfg(unix)]
        {
            self.0.into_blocking_fd()
        }

        #[cfg(windows)]
        {
            if self.pending.is_some() {
                return Err(io::Error::other(
                    "cannot convert PipeSend into a handle while an async write is in flight",
                ));
            }
            let pipe = Arc::try_unwrap(self.inner).or_else(|inner| inner.try_clone())?;
            Ok(OwnedHandle::from(pipe))
        }
    }

    #[cfg(unix)]
    pub fn into_blocking_fd(self) -> io::Result<OwnedFd> {
        self.0.into_blocking_fd()
    }
}

impl PipeRecv {
    pub fn try_clone(&self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = self.0.as_fd().try_clone_to_owned()?;
            Ok(Self(
                tokio::net::unix::pipe::Receiver::from_owned_fd_unchecked(fd)?,
            ))
        }

        #[cfg(windows)]
        {
            Ok(Self {
                inner: Arc::new(self.inner.try_clone()?),
                pending: None,
                ready: None,
            })
        }
    }

    pub fn into_stdio(self) -> io::Result<Stdio> {
        #[cfg(unix)]
        {
            let fd: OwnedFd = self.0.into_blocking_fd()?;
            Ok(Stdio::from(fd))
        }

        #[cfg(windows)]
        {
            if self.pending.is_some() {
                return Err(io::Error::other(
                    "cannot convert PipeRecv into Stdio while an async read is in flight",
                ));
            }

            Arc::try_unwrap(self.inner)
                .or_else(|inner| inner.try_clone())
                .map(Stdio::from)
        }
    }

    pub fn into_blocking_handle(self) -> io::Result<DefaultHandle> {
        #[cfg(unix)]
        {
            self.0.into_blocking_fd()
        }

        #[cfg(windows)]
        {
            if self.pending.is_some() {
                return Err(io::Error::other(
                    "cannot convert PipeRecv into a handle while an async read is in flight",
                ));
            }
            let pipe = Arc::try_unwrap(self.inner).or_else(|inner| inner.try_clone())?;
            Ok(OwnedHandle::from(pipe))
        }
    }

    #[cfg(unix)]
    pub fn into_blocking_fd(self) -> io::Result<OwnedFd> {
        self.0.into_blocking_fd()
    }
}

impl TryFrom<PipeSend> for Stdio {
    type Error = io::Error;

    fn try_from(value: PipeSend) -> Result<Self, Self::Error> {
        value.into_stdio()
    }
}

impl TryFrom<PipeRecv> for Stdio {
    type Error = io::Error;

    fn try_from(value: PipeRecv) -> Result<Self, Self::Error> {
        value.into_stdio()
    }
}

#[cfg(unix)]
impl AsyncWrite for PipeSend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(unix)]
impl AsyncRead for PipeRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl PipeSend {
    fn new(inner: StdPipeWriter) -> Self {
        Self {
            inner: Arc::new(inner),
            pending: None,
        }
    }

    fn poll_pending_write(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let Some(pending) = &mut self.pending else {
            return Poll::Ready(Ok(0));
        };

        match Pin::new(pending).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.pending = None;
                Poll::Ready(result)
            }
            Poll::Ready(Err(err)) => {
                self.pending = None;
                Poll::Ready(Err(io::Error::other(err)))
            }
        }
    }

    fn poll_pending_write_completion(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_pending_write(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }
}

#[cfg(windows)]
impl AsyncWrite for PipeSend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.pending.is_some() {
            return self.poll_pending_write(cx);
        }

        let inner = Arc::clone(&self.inner);
        let data = buf.to_vec();
        self.pending = Some(tokio::task::spawn_blocking(move || {
            let mut writer = &*inner;
            writer.write(&data)
        }));
        self.poll_pending_write(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_pending_write_completion(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_pending_write_completion(cx)
    }
}

#[cfg(windows)]
impl PipeRecv {
    fn new(inner: StdPipeReader) -> Self {
        Self {
            inner: Arc::new(inner),
            pending: None,
            ready: None,
        }
    }
}

#[cfg(windows)]
impl AsyncRead for PipeRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some((data, len)) = &mut self.ready {
            let copy_len = (*len).min(buf.remaining());
            buf.put_slice(&data[..copy_len]);
            if copy_len == *len {
                self.ready = None;
            } else {
                data.drain(..copy_len);
                *len -= copy_len;
            }
            return Poll::Ready(Ok(()));
        }

        if self.pending.is_none() {
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }

            let inner = Arc::clone(&self.inner);
            let cap = buf.remaining();
            self.pending = Some(tokio::task::spawn_blocking(move || {
                let mut data = vec![0; cap];
                let mut reader = &*inner;
                let result = reader.read(&mut data);
                (data, result)
            }));
        }

        let Some(pending) = &mut self.pending else {
            return Poll::Ready(Ok(()));
        };

        match Pin::new(pending).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok((data, Ok(n)))) => {
                self.pending = None;
                let copy_len = n.min(buf.remaining());
                buf.put_slice(&data[..copy_len]);
                if copy_len < n {
                    self.ready = Some((data[copy_len..n].to_vec(), n - copy_len));
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok((_data, Err(err)))) => {
                self.pending = None;
                Poll::Ready(Err(err))
            }
            Poll::Ready(Err(err)) => {
                self.pending = None;
                Poll::Ready(Err(io::Error::other(err)))
            }
        }
    }
}
