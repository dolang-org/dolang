use std::{
    collections::VecDeque,
    io::{self, IoSlice, IoSliceMut},
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixStream,
    sync::Arc,
};

use bytes::{Buf, BufMut};
use nix::{
    errno::Errno,
    sys::socket::{
        ControlMessage, ControlMessageOwned, MsgFlags, Shutdown, recvmsg, sendmsg, shutdown,
    },
};
use tokio::io::unix::AsyncFd;

use super::{Receiver, RecvFrame, SendFrame, Sender};

const MAX_FDS_PER_RECV: usize = 256;

#[cfg(any(target_os = "android", target_os = "linux"))]
const SEND_FLAGS: MsgFlags = MsgFlags::MSG_NOSIGNAL;

#[cfg(not(any(target_os = "android", target_os = "linux")))]
const SEND_FLAGS: MsgFlags = MsgFlags::empty();

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
))]
const RECV_FLAGS: MsgFlags = MsgFlags::MSG_CMSG_CLOEXEC;

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
const RECV_FLAGS: MsgFlags = MsgFlags::empty();

struct Common {
    socket: AsyncFd<OwnedFd>,
}

pub(crate) struct UnixSender {
    common: Arc<Common>,
}

pub(crate) struct UnixReceiver {
    common: Arc<Common>,
    incoming: VecDeque<Option<OwnedFd>>,
}

impl Drop for UnixSender {
    fn drop(&mut self) {
        let _ = shutdown(self.common.socket.as_raw_fd(), Shutdown::Write);
    }
}

impl Drop for UnixReceiver {
    fn drop(&mut self) {
        let _ = shutdown(self.common.socket.as_raw_fd(), Shutdown::Read);
    }
}

pub(crate) fn unix(stream: UnixStream) -> io::Result<(UnixSender, UnixReceiver)> {
    stream.set_nonblocking(true)?;
    let common = Arc::new(Common {
        socket: AsyncFd::new(OwnedFd::from(stream))?,
    });
    Ok((
        UnixSender {
            common: common.clone(),
        },
        UnixReceiver {
            common,
            incoming: VecDeque::new(),
        },
    ))
}

pub(crate) struct UnixSend<'a> {
    sender: &'a mut UnixSender,
    fds: Vec<BorrowedFd<'a>>,
}

pub(crate) struct UnixRecv<'a> {
    receiver: &'a mut UnixReceiver,
    max_index: Option<usize>,
}

impl Sender for UnixSender {
    type Send<'a> = UnixSend<'a>;

    fn send(&mut self) -> Self::Send<'_> {
        UnixSend {
            sender: self,
            fds: Vec::new(),
        }
    }
}

impl Receiver for UnixReceiver {
    type Recv<'a> = UnixRecv<'a>;

    fn recv(&mut self) -> Self::Recv<'_> {
        UnixRecv {
            receiver: self,
            max_index: None,
        }
    }
}

impl RecvFrame for UnixRecv<'_> {
    async fn recv<B: BufMut>(&mut self, buffer: &mut B) -> io::Result<usize> {
        loop {
            let mut ready = self.receiver.common.socket.readable().await?;
            let result = ready.try_io(|socket| recv_once(socket.as_raw_fd(), buffer));
            let (bytes, fds) = match result {
                Ok(result) => result?,
                Err(_) => continue,
            };
            let received_fds = !fds.is_empty();
            if received_fds {
                self.receiver.incoming.extend(fds.into_iter().map(Some));
            }
            if bytes == 0 && received_fds {
                continue;
            }
            return Ok(bytes);
        }
    }

    fn take_fd(&mut self, index: u32) -> io::Result<OwnedFd> {
        let index = usize::try_from(index).unwrap();
        let fd = self
            .receiver
            .incoming
            .get_mut(index)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file descriptor index is unavailable",
                )
            })?
            .take()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file descriptor index was already consumed",
                )
            })?;
        self.max_index = Some(self.max_index.map_or(index, |max| max.max(index)));
        Ok(fd)
    }
}

impl Drop for UnixRecv<'_> {
    fn drop(&mut self) {
        let Some(max_index) = self.max_index else {
            return;
        };
        self.receiver.incoming.drain(..=max_index);
    }
}

impl<'frame> SendFrame<'frame> for UnixSend<'frame> {
    fn attach_fd(&mut self, fd: BorrowedFd<'frame>) -> io::Result<u32> {
        let index = u32::try_from(self.fds.len())
            .map_err(|_| io::Error::other("too many file descriptors in frame"))?;
        self.fds.push(fd);
        Ok(index)
    }

    async fn finish<B: Buf>(self, buffer: &mut B) -> io::Result<()> {
        let raw_fds: Vec<_> = self.fds.iter().map(AsRawFd::as_raw_fd).collect();
        let mut attachments = raw_fds.as_slice();
        while buffer.has_remaining() {
            let mut ready = self.sender.common.socket.writable().await?;
            let result =
                ready.try_io(|socket| send_once(socket.as_raw_fd(), buffer.chunk(), attachments));
            let sent = match result {
                Ok(result) => result?,
                Err(_) => continue,
            };
            if sent == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame",
                ));
            }
            buffer.advance(sent);
            attachments = &[];
        }
        Ok(())
    }
}

fn send_once(fd: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let iov = [IoSlice::new(bytes)];
    loop {
        let result = if fds.is_empty() {
            sendmsg::<()>(fd, &iov, &[], SEND_FLAGS, None)
        } else {
            sendmsg::<()>(
                fd,
                &iov,
                &[ControlMessage::ScmRights(fds)],
                SEND_FLAGS,
                None,
            )
        };
        match result {
            Err(Errno::EINTR) => {}
            Ok(bytes) => return Ok(bytes),
            Err(error) => return Err(error.into()),
        }
    }
}

fn recv_once<B: BufMut>(fd: RawFd, buffer: &mut B) -> io::Result<(usize, Vec<OwnedFd>)> {
    let chunk = buffer.chunk_mut();
    if chunk.len() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receive buffer has no spare capacity",
        ));
    }
    let len = chunk.len().min(64 * 1024);
    // IoSliceMut requires initialized bytes even though recvmsg only writes them.
    unsafe { std::ptr::write_bytes(chunk.as_mut_ptr(), 0, len) };
    let bytes = unsafe { std::slice::from_raw_parts_mut(chunk.as_mut_ptr(), len) };
    let mut iov = [IoSliceMut::new(bytes)];
    let mut cmsg = vec![0; nix::sys::socket::cmsg_space::<[RawFd; MAX_FDS_PER_RECV]>()];
    let message = loop {
        match recvmsg::<()>(fd, &mut iov, Some(&mut cmsg), RECV_FLAGS) {
            Err(Errno::EINTR) => {}
            Ok(message) => break message,
            Err(error) => return Err(error.into()),
        }
    };
    if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ancillary data was truncated",
        ));
    }
    let received = message.bytes;
    let mut fds = Vec::new();
    for control in message.cmsgs()? {
        if let ControlMessageOwned::ScmRights(new_fds) = control {
            let new_fds: Vec<_> = new_fds
                .into_iter()
                .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                .collect();
            #[cfg(target_os = "macos")]
            for fd in &new_fds {
                if unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIOCLEX) } == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            fds.extend(new_fds);
        }
    }
    unsafe { buffer.advance_mut(received) };
    Ok((received, fds))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::AsFd;

    use bytes::{Bytes, BytesMut};
    use nix::{
        fcntl::{FcntlArg, FdFlag, fcntl},
        unistd::{pipe, write},
    };

    use super::*;

    fn pair() -> (UnixSender, UnixReceiver) {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _) = unix(left).unwrap();
        let (_, receiver) = unix(right).unwrap();
        (sender, receiver)
    }

    async fn receive(receiver: &mut impl RecvFrame, expected: usize) -> BytesMut {
        let mut bytes = BytesMut::with_capacity(expected.max(64));
        while bytes.len() < expected {
            assert_ne!(receiver.recv(&mut bytes).await.unwrap(), 0);
        }
        bytes
    }

    #[tokio::test]
    async fn transfers_bytes_without_file_descriptors() {
        let (mut sender, mut receiver) = pair();
        let mut sent = Bytes::from_static(b"hello");
        sender.send().finish(&mut sent).await.unwrap();
        assert_eq!(&receive(&mut receiver.recv(), 5).await[..], b"hello");
    }

    #[tokio::test]
    #[cfg(any(target_os = "android", target_os = "linux"))]
    async fn dropping_receiver_rejects_peer_sends() {
        let (left, right) = UnixStream::pair().unwrap();
        let (_left_sender, left_receiver) = unix(left).unwrap();
        let (mut right_sender, _right_receiver) = unix(right).unwrap();
        drop(left_receiver);
        let mut sent = Bytes::from_static(b"hello");
        assert!(right_sender.send().finish(&mut sent).await.is_err());
    }

    #[tokio::test]
    async fn dropping_connection_rejects_peer_sends() {
        let (left, right) = UnixStream::pair().unwrap();
        let (left_sender, left_receiver) = unix(left).unwrap();
        let (mut right_sender, _right_receiver) = unix(right).unwrap();
        drop(left_receiver);
        drop(left_sender);
        let mut sent = Bytes::from_static(b"hello");
        assert!(right_sender.send().finish(&mut sent).await.is_err());
    }

    #[tokio::test]
    async fn dropping_sender_reports_end_of_stream() {
        let (left, right) = UnixStream::pair().unwrap();
        let (left_sender, _left_receiver) = unix(left).unwrap();
        let (_right_sender, mut right_receiver) = unix(right).unwrap();
        drop(left_sender);
        let mut frame = right_receiver.recv();
        let mut received = BytesMut::with_capacity(64);
        assert_eq!(frame.recv(&mut received).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn transfers_a_usable_file_descriptor() {
        let (mut sender, mut receiver) = pair();
        let (read_fd, write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        assert_eq!(frame.attach_fd(read_fd.as_fd()).unwrap(), 0);
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        drop(read_fd);
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        let received = frame.take_fd(0).unwrap();
        drop(frame);
        write(&write_fd, b"ok").unwrap();
        let mut file = std::fs::File::from(received);
        let mut value = [0; 2];
        file.read_exact(&mut value).unwrap();
        assert_eq!(&value, b"ok");
    }

    #[tokio::test]
    async fn honors_file_descriptor_indexes() {
        let (mut sender, mut receiver) = pair();
        let (read_a, write_a) = pipe().unwrap();
        let (read_b, write_b) = pipe().unwrap();
        let mut frame = sender.send();
        assert_eq!(frame.attach_fd(read_a.as_fd()).unwrap(), 0);
        assert_eq!(frame.attach_fd(read_b.as_fd()).unwrap(), 1);
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        let received_b = frame.take_fd(1).unwrap();
        let received_a = frame.take_fd(0).unwrap();
        drop(frame);
        write(&write_a, b"a").unwrap();
        write(&write_b, b"b").unwrap();
        let mut value = [0];
        std::fs::File::from(received_a)
            .read_exact(&mut value)
            .unwrap();
        assert_eq!(&value, b"a");
        std::fs::File::from(received_b)
            .read_exact(&mut value)
            .unwrap();
        assert_eq!(&value, b"b");
    }

    #[tokio::test]
    async fn preserves_descriptors_for_the_next_frame() {
        let (mut sender, mut receiver) = pair();
        let (read_a, _write_a) = pipe().unwrap();
        let (read_b, _write_b) = pipe().unwrap();
        for fd in [&read_a, &read_b] {
            let mut frame = sender.send();
            frame.attach_fd(fd.as_fd()).unwrap();
            let mut sent = Bytes::from_static(b"x");
            frame.finish(&mut sent).await.unwrap();
        }
        while receiver.incoming.len() < 2 {
            receive(&mut receiver.recv(), 1).await;
        }
        let mut first = receiver.recv();
        first.take_fd(0).unwrap();
        drop(first);
        let mut second = receiver.recv();
        second.take_fd(0).unwrap();
    }

    #[tokio::test]
    async fn rejects_duplicate_and_unavailable_indexes() {
        let (mut sender, mut receiver) = pair();
        let (read_fd, _write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        frame.attach_fd(read_fd.as_fd()).unwrap();
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        frame.take_fd(0).unwrap();
        assert_eq!(
            frame.take_fd(0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            frame.take_fd(1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn received_descriptors_are_close_on_exec() {
        let (mut sender, mut receiver) = pair();
        let (read_fd, _write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        frame.attach_fd(read_fd.as_fd()).unwrap();
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        let received = frame.take_fd(0).unwrap();
        let flags = fcntl(received.as_fd(), FcntlArg::F_GETFD).unwrap();
        assert!(FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC));
    }

    #[tokio::test]
    async fn dropping_an_unfinished_send_closes_staged_descriptors() {
        let (mut sender, _receiver) = pair();
        let (read_fd, write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        frame.attach_fd(write_fd.as_fd()).unwrap();
        drop(frame);
        drop(write_fd);
        let mut file = std::fs::File::from(read_fd);
        let mut byte = [0];
        assert_eq!(file.read(&mut byte).unwrap(), 0);
    }

    #[tokio::test]
    async fn sends_and_receives_concurrently_on_the_shared_socket() {
        let (left, right) = UnixStream::pair().unwrap();
        let (mut left_sender, mut left_receiver) = unix(left).unwrap();
        let (mut right_sender, mut right_receiver) = unix(right).unwrap();
        let left_bytes = vec![b'l'; 512 * 1024];
        let right_bytes = vec![b'r'; 512 * 1024];
        let left_len = left_bytes.len();
        let right_len = right_bytes.len();
        let left = async move {
            let mut sent = Bytes::from(left_bytes);
            let send = left_sender.send().finish(&mut sent);
            let mut frame = left_receiver.recv();
            let receive = receive(&mut frame, right_len);
            let (_, received) = tokio::join!(send, receive);
            received
        };
        let right = async move {
            let mut sent = Bytes::from(right_bytes);
            let send = right_sender.send().finish(&mut sent);
            let mut frame = right_receiver.recv();
            let receive = receive(&mut frame, left_len);
            let (_, received) = tokio::join!(send, receive);
            received
        };
        let (received_right, received_left) = tokio::join!(left, right);
        assert!(received_right.iter().all(|byte| *byte == b'r'));
        assert!(received_left.iter().all(|byte| *byte == b'l'));
    }
}
