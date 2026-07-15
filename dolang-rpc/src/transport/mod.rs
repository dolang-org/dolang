#[cfg(unix)]
use std::os::fd::{BorrowedFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{BorrowedHandle, OwnedHandle};
use std::{io, pin::Pin};

use bytes::{Buf, BufMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) trait Sender: Send + 'static {
    type Send<'a>: SendFrame<'a>
    where
        Self: 'a;

    fn send(&mut self) -> Self::Send<'_>;
}

pub(crate) trait Receiver: Send + 'static {
    type Recv<'a>: RecvFrame
    where
        Self: 'a;

    fn recv(&mut self) -> Self::Recv<'_>;
}

pub(crate) trait RecvFrame {
    #[cfg(unix)]
    fn take_fd(&mut self, index: u32) -> io::Result<OwnedFd>;
    #[cfg(windows)]
    fn take_handle(&mut self, value: usize) -> io::Result<OwnedHandle>;
    async fn recv<B: BufMut>(&mut self, buffer: &mut B) -> io::Result<usize>;
}

pub(crate) trait SendFrame<'frame> {
    #[cfg(unix)]
    fn attach_fd(&mut self, fd: BorrowedFd<'frame>) -> io::Result<u32>;
    #[cfg(windows)]
    fn attach_handle(&mut self, handle: BorrowedHandle<'_>) -> io::Result<usize>;

    async fn finish<B: Buf>(self, buffer: &mut B) -> io::Result<()>;
}

pub(crate) enum AnySender {
    Generic(GenericSender),
    #[cfg(unix)]
    Unix(unix::UnixSender),
    #[cfg(windows)]
    Windows(windows::WindowsSender),
}

pub(crate) enum AnyReceiver {
    Generic(GenericReceiver),
    #[cfg(unix)]
    Unix(unix::UnixReceiver),
    #[cfg(windows)]
    Windows(windows::WindowsReceiver),
}

pub(crate) enum AnySend<'a> {
    Generic(GenericSend<'a>),
    #[cfg(unix)]
    Unix(unix::UnixSend<'a>),
    #[cfg(windows)]
    Windows(windows::WindowsSend<'a>),
}

pub(crate) enum AnyRecv<'a> {
    Generic(GenericRecv<'a>),
    #[cfg(unix)]
    Unix(unix::UnixRecv<'a>),
    #[cfg(windows)]
    Windows(windows::WindowsRecv<'a>),
}

impl Sender for AnySender {
    type Send<'a> = AnySend<'a>;
    fn send(&mut self) -> Self::Send<'_> {
        match self {
            Self::Generic(sender) => AnySend::Generic(sender.send()),
            #[cfg(unix)]
            Self::Unix(sender) => AnySend::Unix(sender.send()),
            #[cfg(windows)]
            Self::Windows(sender) => AnySend::Windows(sender.send()),
        }
    }
}

impl<'frame> SendFrame<'frame> for AnySend<'frame> {
    #[cfg(unix)]
    fn attach_fd(&mut self, fd: BorrowedFd<'frame>) -> io::Result<u32> {
        match self {
            Self::Generic(frame) => frame.attach_fd(fd),
            Self::Unix(frame) => frame.attach_fd(fd),
        }
    }
    #[cfg(windows)]
    fn attach_handle(&mut self, handle: BorrowedHandle<'_>) -> io::Result<usize> {
        match self {
            Self::Generic(frame) => frame.attach_handle(handle),
            Self::Windows(frame) => frame.attach_handle(handle),
        }
    }
    async fn finish<B: Buf>(self, buffer: &mut B) -> io::Result<()> {
        match self {
            Self::Generic(frame) => frame.finish(buffer).await,
            #[cfg(unix)]
            Self::Unix(frame) => frame.finish(buffer).await,
            #[cfg(windows)]
            Self::Windows(frame) => frame.finish(buffer).await,
        }
    }
}

impl Receiver for AnyReceiver {
    type Recv<'a> = AnyRecv<'a>;

    fn recv(&mut self) -> Self::Recv<'_> {
        match self {
            Self::Generic(receiver) => AnyRecv::Generic(receiver.recv()),
            #[cfg(unix)]
            Self::Unix(receiver) => AnyRecv::Unix(receiver.recv()),
            #[cfg(windows)]
            Self::Windows(receiver) => AnyRecv::Windows(receiver.recv()),
        }
    }
}

impl RecvFrame for AnyRecv<'_> {
    #[cfg(unix)]
    fn take_fd(&mut self, index: u32) -> io::Result<OwnedFd> {
        match self {
            Self::Generic(frame) => frame.take_fd(index),
            Self::Unix(frame) => frame.take_fd(index),
        }
    }
    #[cfg(windows)]
    fn take_handle(&mut self, value: usize) -> io::Result<OwnedHandle> {
        match self {
            Self::Generic(frame) => frame.take_handle(value),
            Self::Windows(frame) => frame.take_handle(value),
        }
    }
    async fn recv<B: BufMut>(&mut self, buffer: &mut B) -> io::Result<usize> {
        match self {
            Self::Generic(frame) => frame.recv(buffer).await,
            #[cfg(unix)]
            Self::Unix(frame) => frame.recv(buffer).await,
            #[cfg(windows)]
            Self::Windows(frame) => frame.recv(buffer).await,
        }
    }
}

pub(crate) struct GenericSender(Pin<Box<dyn AsyncWrite + Send>>);
pub(crate) struct GenericReceiver(Pin<Box<dyn AsyncRead + Send>>);

pub(crate) fn generic<R, W>(reader: R, writer: W) -> (GenericSender, GenericReceiver)
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    (
        GenericSender(Box::pin(writer)),
        GenericReceiver(Box::pin(reader)),
    )
}

pub(crate) fn generic_duplex<T>(stream: T) -> (GenericSender, GenericReceiver)
where
    T: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    generic(reader, writer)
}

pub(crate) struct GenericSend<'a>(&'a mut GenericSender);
pub(crate) struct GenericRecv<'a>(&'a mut GenericReceiver);
impl Sender for GenericSender {
    type Send<'a> = GenericSend<'a>;

    fn send(&mut self) -> Self::Send<'_> {
        GenericSend(self)
    }
}

impl Receiver for GenericReceiver {
    type Recv<'a> = GenericRecv<'a>;

    fn recv(&mut self) -> Self::Recv<'_> {
        GenericRecv(self)
    }
}

impl RecvFrame for GenericRecv<'_> {
    #[cfg(unix)]
    fn take_fd(&mut self, _index: u32) -> io::Result<OwnedFd> {
        panic!("generic byte-stream transport does not support file descriptors")
    }
    #[cfg(windows)]
    fn take_handle(&mut self, _value: usize) -> io::Result<OwnedHandle> {
        panic!("generic byte-stream transport does not support handles")
    }

    async fn recv<B: BufMut>(&mut self, buffer: &mut B) -> io::Result<usize> {
        self.0.0.read_buf(buffer).await
    }
}

impl<'frame> SendFrame<'frame> for GenericSend<'frame> {
    #[cfg(unix)]
    fn attach_fd(&mut self, _fd: BorrowedFd<'frame>) -> io::Result<u32> {
        panic!("generic byte-stream transport does not support file descriptors")
    }
    #[cfg(windows)]
    fn attach_handle(&mut self, _handle: BorrowedHandle<'_>) -> io::Result<usize> {
        panic!("generic byte-stream transport does not support handles")
    }

    async fn finish<B: Buf>(self, buffer: &mut B) -> io::Result<()> {
        while buffer.has_remaining() {
            if self.0.0.write_buf(buffer).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame",
                ));
            }
        }
        self.0.0.flush().await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    #[tokio::test]
    #[should_panic(expected = "generic byte-stream transport does not support file descriptors")]
    async fn generic_send_rejects_file_descriptors() {
        let (stream, _) = tokio::io::duplex(64);
        let (mut sender, _) = generic_duplex(stream);
        let (fd, _) = std::os::unix::net::UnixStream::pair().unwrap();
        sender.send().attach_fd(fd.as_fd()).unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "generic byte-stream transport does not support file descriptors")]
    async fn generic_receiver_rejects_file_descriptors() {
        let (stream, _) = tokio::io::duplex(64);
        let (_, mut receiver) = generic_duplex(stream);
        receiver.recv().take_fd(0).unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::io::AsHandle;

    use super::*;

    #[tokio::test]
    #[should_panic(expected = "generic byte-stream transport does not support handles")]
    async fn generic_send_rejects_handles() {
        let (stream, _) = tokio::io::duplex(64);
        let (mut sender, _) = generic_duplex(stream);
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        sender.send().attach_handle(file.as_handle()).unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "generic byte-stream transport does not support handles")]
    async fn generic_receiver_rejects_handles() {
        let (stream, _) = tokio::io::duplex(64);
        let (_, mut receiver) = generic_duplex(stream);
        receiver.recv().take_handle(1).unwrap();
    }
}
