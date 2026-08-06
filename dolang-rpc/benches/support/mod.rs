//! Native OS pipe plumbing shared by the `pipe_trailer` and `pipe_raw`
//! benchmarks — the same transport shape as a subprocess talking to its
//! parent over stdin/stdout. Included via `#[path = "support.rs"] mod
//! support;` rather than as a library, since bench targets can't otherwise
//! share code with each other.
//!
//! `native_pipe` returns a `(Recv, Send)` pair, both implementing
//! `AsyncRead + Send + 'static` / `AsyncWrite + Send + 'static`
//! respectively, so callers don't need to know which platform they're on.

#![allow(dead_code)]

pub(crate) const PIPE_BUFFER_SIZE: usize = 1024 * 1024;

#[cfg(unix)]
pub(crate) type Recv = tokio::net::unix::pipe::Receiver;
#[cfg(unix)]
pub(crate) type Send = tokio::net::unix::pipe::Sender;

#[cfg(unix)]
pub(crate) fn native_pipe() -> (Recv, Send) {
    let (send, recv) = tokio::net::unix::pipe::pipe().unwrap();
    set_pipe_buffer_size(&send);
    (recv, send)
}

#[cfg(target_os = "linux")]
fn set_pipe_buffer_size(send: &Send) {
    use std::os::fd::AsRawFd;
    // Best-effort, as in dolang-vfs's src/pipe.rs: failure just leaves
    // the default buffer size.
    unsafe {
        libc::fcntl(
            send.as_raw_fd(),
            libc::F_SETPIPE_SZ,
            PIPE_BUFFER_SIZE as i32,
        );
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn set_pipe_buffer_size(_send: &Send) {}

// Windows has no anonymous pipe API that takes a buffer-size hint, so this
// mirrors dolang-vfs's src/pipe.rs: call `CreatePipe` directly with an
// explicit size, then wrap the resulting blocking handles in
// `spawn_blocking`-backed `AsyncRead`/`AsyncWrite` adapters (std's
// `PipeReader`/`PipeWriter` have no async story of their own).
#[cfg(windows)]
mod windows_pipe {
    use std::{
        io::{self, PipeReader, PipeWriter, Read as _, Write as _},
        os::windows::io::FromRawHandle,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
    };

    use tokio::{
        io::{AsyncRead, AsyncWrite, ReadBuf},
        task::JoinHandle,
    };
    use windows_sys::Win32::{
        Foundation::HANDLE, Security::SECURITY_ATTRIBUTES, System::Pipes::CreatePipe,
    };

    pub(crate) struct Send {
        inner: Arc<PipeWriter>,
        pending: Option<JoinHandle<io::Result<usize>>>,
    }

    pub(crate) struct Recv {
        inner: Arc<PipeReader>,
        pending: Option<JoinHandle<(Vec<u8>, io::Result<usize>)>>,
        ready: Option<(Vec<u8>, usize)>,
    }

    pub(crate) fn native_pipe(buf_size: usize) -> (Recv, Send) {
        let (reader, writer) = create_pipe_sized(buf_size).unwrap();
        (
            Recv {
                inner: Arc::new(reader),
                pending: None,
                ready: None,
            },
            Send {
                inner: Arc::new(writer),
                pending: None,
            },
        )
    }

    /// Creates an anonymous pipe with a requested kernel buffer size.
    /// `std::io::pipe` has no size parameter, so this calls `CreatePipe`
    /// directly with `bInheritHandle = FALSE`, matching the non-inheritable
    /// handles `std::io::pipe` itself produces.
    fn create_pipe_sized(size: usize) -> io::Result<(PipeReader, PipeWriter)> {
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
        let reader = unsafe { PipeReader::from_raw_handle(read_handle as _) };
        let writer = unsafe { PipeWriter::from_raw_handle(write_handle as _) };
        Ok((reader, writer))
    }

    impl AsyncWrite for Send {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if let Some(task) = &mut self.pending {
                return match Pin::new(task).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(result)) => {
                        self.pending = None;
                        Poll::Ready(result)
                    }
                    Poll::Ready(Err(error)) => {
                        self.pending = None;
                        Poll::Ready(Err(io::Error::other(error)))
                    }
                };
            }
            let inner = Arc::clone(&self.inner);
            let data = buf.to_vec();
            self.pending = Some(tokio::task::spawn_blocking(move || (&*inner).write(&data)));
            self.poll_write(cx, &[])
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.as_mut().poll_write(cx, &[]) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    impl AsyncRead for Recv {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if let Some((data, len)) = &mut self.ready {
                let n = (*len).min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n == *len {
                    self.ready = None;
                } else {
                    data.drain(..n);
                    *len -= n;
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
                    let result = (&*inner).read(&mut data);
                    (data, result)
                }));
            }
            match Pin::new(self.pending.as_mut().unwrap()).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok((data, Ok(len)))) => {
                    self.pending = None;
                    let n = len.min(buf.remaining());
                    buf.put_slice(&data[..n]);
                    if n < len {
                        self.ready = Some((data[n..len].to_vec(), len - n));
                    }
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Ok((_, Err(error)))) => {
                    self.pending = None;
                    Poll::Ready(Err(error))
                }
                Poll::Ready(Err(error)) => {
                    self.pending = None;
                    Poll::Ready(Err(io::Error::other(error)))
                }
            }
        }
    }
}

#[cfg(windows)]
pub(crate) type Recv = windows_pipe::Recv;
#[cfg(windows)]
pub(crate) type Send = windows_pipe::Send;

#[cfg(windows)]
pub(crate) fn native_pipe() -> (Recv, Send) {
    windows_pipe::native_pipe(PIPE_BUFFER_SIZE)
}
