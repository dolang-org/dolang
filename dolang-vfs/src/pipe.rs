use std::{
    io,
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
};

use dolang_rpc::DefaultHandle;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
};

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::{
    io::{PipeReader, PipeWriter, Read as _, Write as _},
    os::windows::io::OwnedHandle,
    sync::Arc,
};
#[cfg(windows)]
use tokio::task::JoinHandle;

#[derive(Debug)]
pub enum StdioSend {
    Native(NativeStdioSend),
    Remote(crate::client::RemoteStdioSend),
}

#[derive(Debug)]
pub enum StdioRecv {
    Native(NativeStdioRecv),
    Remote(crate::client::RemoteStdioRecv),
}

#[cfg(unix)]
#[derive(Debug)]
pub enum NativeStdioSend {
    Pipe(tokio::net::unix::pipe::Sender),
    File(File),
}

#[cfg(unix)]
#[derive(Debug)]
pub enum NativeStdioRecv {
    Pipe(tokio::net::unix::pipe::Receiver),
    File(File),
}

#[cfg(windows)]
#[derive(Debug)]
pub enum NativeStdioSend {
    Pipe {
        inner: Arc<PipeWriter>,
        pending: Option<JoinHandle<io::Result<usize>>>,
    },
    File(File),
}

#[cfg(windows)]
#[derive(Debug)]
pub enum NativeStdioRecv {
    Pipe {
        inner: Arc<PipeReader>,
        pending: Option<JoinHandle<(Vec<u8>, io::Result<usize>)>>,
        ready: Option<(Vec<u8>, usize)>,
    },
    File(File),
}

/// Pumps bytes from `src` to `dst` until EOF or error, then shuts `dst` down.
pub(crate) async fn relay<R, W>(mut src: R, mut dst: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let _ = tokio::io::copy(&mut src, &mut dst).await;
    let _ = dst.shutdown().await;
}

/// Creates a native OS pipe, optionally hinting a kernel buffer size.
///
/// The hint is best-effort: on platforms/configurations where it can't be
/// honored, the pipe is created with its default buffer size instead of
/// failing outright.
pub(crate) fn pipe(buf_size: Option<usize>) -> io::Result<(StdioSend, StdioRecv)> {
    #[cfg(unix)]
    {
        let (send, recv) = tokio::net::unix::pipe::pipe()?;
        if let Some(size) = buf_size {
            use std::os::fd::AsRawFd;
            set_pipe_buffer_size(send.as_raw_fd(), size);
        }
        Ok((
            StdioSend::Native(NativeStdioSend::Pipe(send)),
            StdioRecv::Native(NativeStdioRecv::Pipe(recv)),
        ))
    }
    #[cfg(windows)]
    {
        let (recv, send) = match buf_size {
            Some(size) => create_pipe_sized(size)?,
            None => std::io::pipe()?,
        };
        Ok((
            StdioSend::Native(NativeStdioSend::Pipe {
                inner: Arc::new(send),
                pending: None,
            }),
            StdioRecv::Native(NativeStdioRecv::Pipe {
                inner: Arc::new(recv),
                pending: None,
                ready: None,
            }),
        ))
    }
}

/// Resizes a pipe's kernel buffer via `fcntl(F_SETPIPE_SZ)`, applied to
/// either end (the buffer is shared by both). No raw-handle bypass is
/// needed: tokio's pipe types expose the fd directly via `AsRawFd`.
#[cfg(target_os = "linux")]
pub(crate) fn set_pipe_buffer_size(fd: std::os::fd::RawFd, size: usize) {
    let size = i32::try_from(size).unwrap_or(i32::MAX);
    // Best-effort: failure (e.g. exceeding /proc/sys/fs/pipe-max-size
    // without CAP_SYS_RESOURCE) just leaves the default buffer size.
    unsafe {
        libc::fcntl(fd, libc::F_SETPIPE_SZ, size);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn set_pipe_buffer_size(_fd: std::os::fd::RawFd, _size: usize) {}

/// Creates an anonymous pipe with a requested kernel buffer size.
///
/// `std::io::pipe` has no size parameter, so this bypasses it and calls
/// `CreatePipe` directly with `bInheritHandle = FALSE`, matching the
/// non-inheritable handles `std::io::pipe` itself produces.
#[cfg(windows)]
fn create_pipe_sized(size: usize) -> io::Result<(std::io::PipeReader, std::io::PipeWriter)> {
    use std::os::windows::io::FromRawHandle;

    use windows_sys::Win32::{
        Foundation::HANDLE, Security::SECURITY_ATTRIBUTES, System::Pipes::CreatePipe,
    };

    let mut read_handle: HANDLE = std::ptr::null_mut();
    let mut write_handle: HANDLE = std::ptr::null_mut();
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 0,
    };
    let size = u32::try_from(size).unwrap_or(u32::MAX);
    // SAFETY: `read_handle`/`write_handle` are valid out-params for the
    // duration of this call; `attrs` lives on the stack until it returns.
    let ok = unsafe { CreatePipe(&mut read_handle, &mut write_handle, &attrs, size) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `read_handle`/`write_handle` are freshly created, uniquely
    // owned handles from a successful `CreatePipe` call.
    let reader = unsafe { std::io::PipeReader::from_raw_handle(read_handle as _) };
    let writer = unsafe { std::io::PipeWriter::from_raw_handle(write_handle as _) };
    Ok((reader, writer))
}

impl StdioSend {
    pub(crate) fn disarm_remote_cleanup(&mut self) {
        if let Self::Remote(remote) = self {
            remote.disarm_cleanup();
        }
    }
    pub(crate) fn from_file(file: File) -> Self {
        Self::Native(NativeStdioSend::File(file))
    }

    pub async fn try_clone(&self) -> io::Result<Self> {
        match self {
            #[cfg(unix)]
            Self::Native(NativeStdioSend::Pipe(pipe)) => {
                let fd = pipe.as_fd().try_clone_to_owned()?;
                Ok(Self::Native(NativeStdioSend::Pipe(
                    tokio::net::unix::pipe::Sender::from_owned_fd_unchecked(fd)?,
                )))
            }
            #[cfg(windows)]
            Self::Native(NativeStdioSend::Pipe { inner, .. }) => {
                Ok(Self::Native(NativeStdioSend::Pipe {
                    inner: Arc::new(inner.try_clone()?),
                    pending: None,
                }))
            }
            Self::Native(NativeStdioSend::File(file)) => {
                Ok(Self::Native(NativeStdioSend::File(file.try_clone().await?)))
            }
            Self::Remote(remote) => remote.try_clone().await.map(Self::Remote),
        }
    }

    pub async fn into_stdio(self) -> io::Result<Stdio> {
        match self {
            Self::Native(NativeStdioSend::File(file)) => Ok(Stdio::from(file.into_std().await)),
            #[cfg(unix)]
            Self::Native(NativeStdioSend::Pipe(pipe)) => {
                let fd: OwnedFd = pipe.into_blocking_fd()?;
                Ok(Stdio::from(fd))
            }
            #[cfg(windows)]
            Self::Native(NativeStdioSend::Pipe { inner, pending }) => {
                if pending.is_some() {
                    return Err(io::Error::other(
                        "cannot convert StdioSend while an async write is in flight",
                    ));
                }
                Arc::try_unwrap(inner)
                    .or_else(|inner| inner.try_clone())
                    .map(Stdio::from)
            }
            Self::Remote(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote stdio cannot be converted to a native handle",
            )),
        }
    }

    pub(crate) async fn into_blocking_handle(self) -> io::Result<DefaultHandle> {
        match self {
            Self::Native(NativeStdioSend::File(file)) => Ok(file.into_std().await.into()),
            #[cfg(unix)]
            Self::Native(NativeStdioSend::Pipe(pipe)) => pipe.into_blocking_fd(),
            #[cfg(windows)]
            Self::Native(NativeStdioSend::Pipe { inner, pending }) => {
                if pending.is_some() {
                    return Err(io::Error::other(
                        "cannot convert StdioSend while an async write is in flight",
                    ));
                }
                let pipe = Arc::try_unwrap(inner).or_else(|inner| inner.try_clone())?;
                Ok(OwnedHandle::from(pipe))
            }
            Self::Remote(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote stdio has no native handle",
            )),
        }
    }
}

impl StdioRecv {
    pub(crate) fn disarm_remote_cleanup(&mut self) {
        if let Self::Remote(remote) = self {
            remote.disarm_cleanup();
        }
    }
    pub(crate) fn from_file(file: File) -> Self {
        Self::Native(NativeStdioRecv::File(file))
    }

    pub async fn try_clone(&self) -> io::Result<Self> {
        match self {
            #[cfg(unix)]
            Self::Native(NativeStdioRecv::Pipe(pipe)) => {
                let fd = pipe.as_fd().try_clone_to_owned()?;
                Ok(Self::Native(NativeStdioRecv::Pipe(
                    tokio::net::unix::pipe::Receiver::from_owned_fd_unchecked(fd)?,
                )))
            }
            #[cfg(windows)]
            Self::Native(NativeStdioRecv::Pipe { inner, .. }) => {
                Ok(Self::Native(NativeStdioRecv::Pipe {
                    inner: Arc::new(inner.try_clone()?),
                    pending: None,
                    ready: None,
                }))
            }
            Self::Native(NativeStdioRecv::File(file)) => {
                Ok(Self::Native(NativeStdioRecv::File(file.try_clone().await?)))
            }
            Self::Remote(remote) => remote.try_clone().await.map(Self::Remote),
        }
    }

    pub async fn into_stdio(self) -> io::Result<Stdio> {
        match self {
            Self::Native(NativeStdioRecv::File(file)) => Ok(Stdio::from(file.into_std().await)),
            #[cfg(unix)]
            Self::Native(NativeStdioRecv::Pipe(pipe)) => {
                let fd: OwnedFd = pipe.into_blocking_fd()?;
                Ok(Stdio::from(fd))
            }
            #[cfg(windows)]
            Self::Native(NativeStdioRecv::Pipe { inner, pending, .. }) => {
                if pending.is_some() {
                    return Err(io::Error::other(
                        "cannot convert StdioRecv while an async read is in flight",
                    ));
                }
                Arc::try_unwrap(inner)
                    .or_else(|inner| inner.try_clone())
                    .map(Stdio::from)
            }
            Self::Remote(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote stdio cannot be converted to a native handle",
            )),
        }
    }

    pub(crate) async fn into_blocking_handle(self) -> io::Result<DefaultHandle> {
        match self {
            Self::Native(NativeStdioRecv::File(file)) => Ok(file.into_std().await.into()),
            #[cfg(unix)]
            Self::Native(NativeStdioRecv::Pipe(pipe)) => pipe.into_blocking_fd(),
            #[cfg(windows)]
            Self::Native(NativeStdioRecv::Pipe { inner, pending, .. }) => {
                if pending.is_some() {
                    return Err(io::Error::other(
                        "cannot convert StdioRecv while an async read is in flight",
                    ));
                }
                let pipe = Arc::try_unwrap(inner).or_else(|inner| inner.try_clone())?;
                Ok(OwnedHandle::from(pipe))
            }
            Self::Remote(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote stdio has no native handle",
            )),
        }
    }
}

impl AsyncWrite for StdioSend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Native(native) => Pin::new(native).poll_write(cx, buf),
            Self::Remote(remote) => Pin::new(remote).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Native(native) => Pin::new(native).poll_flush(cx),
            Self::Remote(remote) => Pin::new(remote).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Native(native) => Pin::new(native).poll_shutdown(cx),
            Self::Remote(remote) => Pin::new(remote).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for StdioRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Native(native) => Pin::new(native).poll_read(cx, buf),
            Self::Remote(remote) => Pin::new(remote).poll_read(cx, buf),
        }
    }
}

#[cfg(unix)]
impl AsyncWrite for NativeStdioSend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Pipe(pipe) => Pin::new(pipe).poll_write(cx, buf),
            Self::File(file) => Pin::new(file).poll_write(cx, buf),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Pipe(pipe) => Pin::new(pipe).poll_flush(cx),
            Self::File(file) => Pin::new(file).poll_flush(cx),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Pipe(pipe) => Pin::new(pipe).poll_shutdown(cx),
            Self::File(file) => Pin::new(file).poll_shutdown(cx),
        }
    }
}

#[cfg(unix)]
impl AsyncRead for NativeStdioRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Pipe(pipe) => Pin::new(pipe).poll_read(cx, buf),
            Self::File(file) => Pin::new(file).poll_read(cx, buf),
        }
    }
}

#[cfg(windows)]
impl AsyncWrite for NativeStdioSend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::File(file) => Pin::new(file).poll_write(cx, buf),
            Self::Pipe { inner, pending } => {
                if let Some(task) = pending {
                    return match Pin::new(task).poll(cx) {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(Ok(result)) => {
                            *pending = None;
                            Poll::Ready(result)
                        }
                        Poll::Ready(Err(error)) => {
                            *pending = None;
                            Poll::Ready(Err(io::Error::other(error)))
                        }
                    };
                }
                let inner = Arc::clone(inner);
                let data = buf.to_vec();
                *pending = Some(tokio::task::spawn_blocking(move || (&*inner).write(&data)));
                self.poll_write(cx, &[])
            }
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.as_mut().poll_write(cx, &[]) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[cfg(windows)]
impl AsyncRead for NativeStdioRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::File(file) => Pin::new(file).poll_read(cx, buf),
            Self::Pipe {
                inner,
                pending,
                ready,
            } => {
                if let Some((data, len)) = ready {
                    let n = (*len).min(buf.remaining());
                    buf.put_slice(&data[..n]);
                    if n == *len {
                        *ready = None;
                    } else {
                        data.drain(..n);
                        *len -= n;
                    }
                    return Poll::Ready(Ok(()));
                }
                if pending.is_none() {
                    if buf.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    let inner = Arc::clone(inner);
                    let cap = buf.remaining();
                    *pending = Some(tokio::task::spawn_blocking(move || {
                        let mut data = vec![0; cap];
                        let result = (&*inner).read(&mut data);
                        (data, result)
                    }));
                }
                match Pin::new(pending.as_mut().unwrap()).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok((data, Ok(len)))) => {
                        *pending = None;
                        let n = len.min(buf.remaining());
                        buf.put_slice(&data[..n]);
                        if n < len {
                            *ready = Some((data[n..len].to_vec(), len - n));
                        }
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Ok((_, Err(error)))) => {
                        *pending = None;
                        Poll::Ready(Err(error))
                    }
                    Poll::Ready(Err(error)) => {
                        *pending = None;
                        Poll::Ready(Err(io::Error::other(error)))
                    }
                }
            }
        }
    }
}
