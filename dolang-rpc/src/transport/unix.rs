use std::{
    io::{self, IoSlice, IoSliceMut},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixStream,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::BufMut;
use nix::{
    errno::Errno,
    sys::socket::{
        ControlMessage, ControlMessageOwned, MsgFlags, Shutdown, recvmsg, sendmsg, shutdown,
    },
};
use tokio::io::unix::AsyncFd;

use super::{AnySender, Receiver, RecvFrame, SendFrame, Sender};
use crate::handle::ErasedHandle;

pub(crate) struct EncodeHandles {
    handles: Vec<OwnedFd>,
    max_handles: usize,
    supported: bool,
}

impl EncodeHandles {
    pub(crate) fn new(sender: &AnySender, max_handles: usize) -> Self {
        Self {
            handles: Vec::new(),
            max_handles,
            supported: matches!(sender, AnySender::Unix(_)),
        }
    }

    pub(crate) fn finish(self) -> OutgoingHandles {
        let fds = self.handles;
        #[cfg(target_os = "macos")]
        let escrow = !fds.is_empty();
        OutgoingHandles {
            fds,
            #[cfg(target_os = "macos")]
            escrow,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(max_handles: usize) -> Self {
        Self {
            handles: Vec::new(),
            max_handles,
            supported: true,
        }
    }
}

impl EncodeHandles {
    pub(crate) fn put_handle(&mut self, handle: &dyn ErasedHandle) -> io::Result<u32> {
        if !self.supported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "generic byte-stream transport does not support handle attachments",
            ));
        }
        if self.handles.len() == self.max_handles {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message contains too many handle attachments",
            ));
        }
        let index = u32::try_from(self.handles.len()).unwrap();
        let handle = handle.steal_handle().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the same handle was serialized more than once",
            )
        })?;
        self.handles.push(handle);
        Ok(index)
    }
}

#[derive(Default)]
pub(crate) struct OutgoingHandles {
    pub(crate) fds: Vec<OwnedFd>,
    #[cfg(target_os = "macos")]
    escrow: bool,
}

#[cfg(target_os = "macos")]
impl OutgoingHandles {
    pub(crate) fn needs_ack(&self) -> bool {
        self.escrow
    }

    pub(crate) fn finish_attached(&mut self, count: usize) -> Vec<OwnedFd> {
        self.fds.drain(..count).collect()
    }

    pub(crate) fn escrow_tracking(&self) -> bool {
        self.escrow
    }
}

#[derive(Default)]
pub(crate) struct ReceivedHandles {
    fds: Vec<Option<OwnedFd>>,
}

impl ReceivedHandles {
    pub(crate) fn extend(&mut self, fds: Vec<OwnedFd>) {
        self.fds.extend(fds.into_iter().map(Some));
    }

    pub(crate) fn len(&self) -> usize {
        self.fds.len()
    }
}

impl ReceivedHandles {
    pub(crate) fn take_handle(&mut self, index: u32) -> io::Result<OwnedFd> {
        let index = usize::try_from(index).unwrap();
        self.fds
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
            })
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if self.fds.iter().any(Option::is_some) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "message contains unused file descriptor attachments",
            ));
        }
        Ok(())
    }
}

/// Hard upper bound accepted by `SCM_RIGHTS` on the supported Unix kernels.
pub(crate) const MAX_FDS_PER_FRAGMENT: usize = 253;

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
    max_fds_per_fragment: usize,
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
            max_fds_per_fragment: 0,
        },
    ))
}

impl UnixReceiver {
    pub(crate) fn set_max_fds_per_fragment(&mut self, value: usize) {
        self.max_fds_per_fragment = value.min(MAX_FDS_PER_FRAGMENT);
    }
}

pub(crate) struct UnixSend<'a> {
    sender: &'a mut UnixSender,
    fds: Vec<BorrowedFd<'a>>,
    /// Whether descriptors (if any) have already ridden along with a
    /// successful `sendmsg`. `SCM_RIGHTS` ancillary data is not chunked
    /// like the byte stream — it either accompanies a syscall or it
    /// doesn't — so it must never be attached to more than the first one.
    attached: bool,
}

pub(crate) struct UnixRecv<'a> {
    receiver: &'a mut UnixReceiver,
    incoming: Vec<OwnedFd>,
}

impl Sender for UnixSender {
    type Send<'a> = UnixSend<'a>;

    fn send(&mut self) -> Self::Send<'_> {
        UnixSend {
            sender: self,
            fds: Vec::new(),
            attached: false,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        shutdown(self.common.socket.as_raw_fd(), Shutdown::Write)?;
        Ok(())
    }
}

impl Receiver for UnixReceiver {
    type Recv<'a> = UnixRecv<'a>;

    fn recv(&mut self) -> Self::Recv<'_> {
        UnixRecv {
            receiver: self,
            incoming: Vec::new(),
        }
    }
}

impl RecvFrame for UnixRecv<'_> {
    fn poll_read_once<B: BufMut>(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut B,
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = match self.receiver.common.socket.poll_read_ready(cx) {
                Poll::Ready(Ok(ready)) => ready,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            };
            let max_fds = self.receiver.max_fds_per_fragment;
            let result = ready.try_io(|socket| recv_once(socket.as_raw_fd(), buffer, max_fds));
            let (bytes, fds) = match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => continue,
            };
            let received_fds = !fds.is_empty();
            if received_fds {
                self.incoming.extend(fds);
            }
            if bytes == 0 && received_fds {
                continue;
            }
            return Poll::Ready(Ok(bytes));
        }
    }

    fn drain_fds(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.incoming)
    }
}

impl<'frame> SendFrame<'frame> for UnixSend<'frame> {
    fn attach_fds(&mut self, fds: &'frame [OwnedFd]) -> io::Result<usize> {
        let count = fds
            .len()
            .min(MAX_FDS_PER_FRAGMENT.saturating_sub(self.fds.len()));
        self.fds.extend(fds[..count].iter().map(AsFd::as_fd));
        Ok(count)
    }

    fn poll_write_once(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.poll_write_vectored_once(cx, &[IoSlice::new(buf)])
    }

    fn poll_write_vectored_once(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = match self.sender.common.socket.poll_write_ready(cx) {
                Poll::Ready(Ok(ready)) => ready,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            };
            let attachments: Vec<RawFd> = if self.attached {
                Vec::new()
            } else {
                self.fds.iter().map(AsRawFd::as_raw_fd).collect()
            };
            let result = ready.try_io(|socket| send_once(socket.as_raw_fd(), bufs, &attachments));
            let sent = match result {
                Ok(result) => result,
                // Spurious readiness: `try_io` already cleared it, so
                // looping back to `poll_write_ready` correctly re-arms.
                Err(_would_block) => continue,
            };
            if sent.is_ok() {
                self.attached = true;
            }
            return Poll::Ready(sent);
        }
    }
}

fn send_once(fd: RawFd, iov: &[IoSlice<'_>], fds: &[RawFd]) -> io::Result<usize> {
    loop {
        let result = if fds.is_empty() {
            sendmsg::<()>(fd, iov, &[], SEND_FLAGS, None)
        } else {
            sendmsg::<()>(fd, iov, &[ControlMessage::ScmRights(fds)], SEND_FLAGS, None)
        };
        match result {
            Err(Errno::EINTR) => {}
            Ok(bytes) => return Ok(bytes),
            Err(error) => return Err(error.into()),
        }
    }
}

fn recv_once<B: BufMut>(
    fd: RawFd,
    buffer: &mut B,
    max_fds: usize,
) -> io::Result<(usize, Vec<OwnedFd>)> {
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
    // Include the platform-required trailing alignment. The padding can make
    // room for an extra descriptor, but the reassembler validates the actual
    // count against the negotiated per-fragment limit.
    let cmsg_len = unsafe {
        libc::CMSG_SPACE((max_fds * std::mem::size_of::<RawFd>()) as libc::c_uint) as usize
    };
    let mut cmsg = vec![0; cmsg_len];
    // Held across the recvmsg retry loop and the FIOCLEX fixup loop below:
    // on macOS, recvmsg can't set CLOEXEC atomically, so this serializes
    // fd receipt against every posix_spawn/posix_spawnp/fork in the process
    // (see transport::macos) to close the window where a leaked fd could be
    // inherited by a concurrently spawned child. No-op on other platforms,
    // where MSG_CMSG_CLOEXEC already makes this atomic.
    #[cfg(target_os = "macos")]
    let _read_guard = super::macos::ReadGuard::acquire();
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
    if fds.len() > max_fds {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ancillary data exceeds the negotiated file descriptor limit",
        ));
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
        let mut receiver = receiver;
        receiver.set_max_fds_per_fragment(64);
        (sender, receiver)
    }

    #[test]
    fn rejects_unconsumed_file_descriptor_attachments() {
        let (read, _write) = pipe().unwrap();
        let mut handles = ReceivedHandles::default();
        handles.extend(vec![read]);
        let error = handles.finish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
    async fn poll_write_once_writes_directly() {
        let (mut sender, mut receiver) = pair();
        let mut send = sender.send();
        std::future::poll_fn(|cx| send.poll_write_once(cx, b"direct"))
            .await
            .unwrap();
        assert_eq!(&receive(&mut receiver.recv(), 6).await[..], b"direct");
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
        assert_eq!(frame.attach_fds(std::slice::from_ref(&read_fd)).unwrap(), 1);
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        drop(read_fd);
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        let received = frame.drain_fds().remove(0);
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
        let handles = [read_a, read_b];
        let mut frame = sender.send();
        assert_eq!(frame.attach_fds(&handles).unwrap(), 2);
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        let mut received = frame.drain_fds();
        let received_a = received.remove(0);
        let received_b = received.remove(0);
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
    async fn keeps_descriptors_on_their_protocol_frame() {
        let (mut sender, mut receiver) = pair();
        let (read_a, _write_a) = pipe().unwrap();
        let (read_b, _write_b) = pipe().unwrap();
        for fd in [read_a, read_b] {
            let mut frame = sender.send();
            frame.attach_fds(std::slice::from_ref(&fd)).unwrap();
            let mut sent = Bytes::from_static(b"x");
            frame.finish(&mut sent).await.unwrap();
        }
        let mut first = receiver.recv();
        receive(&mut first, 1).await;
        assert_eq!(first.drain_fds().len(), 1);
        drop(first);
        let mut second = receiver.recv();
        receive(&mut second, 1).await;
        assert_eq!(second.drain_fds().len(), 1);
    }

    #[tokio::test]
    async fn draining_descriptors_is_idempotent() {
        let (mut sender, mut receiver) = pair();
        let (read_fd, _write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        frame.attach_fds(std::slice::from_ref(&read_fd)).unwrap();
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        assert_eq!(frame.drain_fds().len(), 1);
        assert!(frame.drain_fds().is_empty());
    }

    #[tokio::test]
    async fn rejects_ancillary_data_larger_than_the_receive_cap() {
        let (mut sender, mut receiver) = pair();
        receiver.set_max_fds_per_fragment(1);
        let (read_a, _write_a) = pipe().unwrap();
        let (read_b, _write_b) = pipe().unwrap();
        let handles = [read_a, read_b];
        let mut frame = sender.send();
        assert_eq!(frame.attach_fds(&handles).unwrap(), 2);
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();

        let mut frame = receiver.recv();
        let mut received = BytesMut::with_capacity(1);
        assert_eq!(
            frame.recv(&mut received).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn received_descriptors_are_close_on_exec() {
        let (mut sender, mut receiver) = pair();
        let (read_fd, _write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        frame.attach_fds(std::slice::from_ref(&read_fd)).unwrap();
        let mut sent = Bytes::from_static(b"x");
        frame.finish(&mut sent).await.unwrap();
        let mut frame = receiver.recv();
        receive(&mut frame, 1).await;
        let received = frame.drain_fds().remove(0);
        let flags = fcntl(received.as_fd(), FcntlArg::F_GETFD).unwrap();
        assert!(FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC));
    }

    #[tokio::test]
    async fn dropping_an_unfinished_send_closes_staged_descriptors() {
        let (mut sender, _receiver) = pair();
        let (read_fd, write_fd) = pipe().unwrap();
        let mut frame = sender.send();
        frame.attach_fds(std::slice::from_ref(&write_fd)).unwrap();
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

    #[test]
    fn put_handle_on_generic_transport_reports_unsupported_instead_of_panicking() {
        let (stream, _) = crate::tests::duplex(64);
        let (sender, _) = super::super::generic_duplex(stream);
        let any = super::super::AnySender::Generic(sender);
        let mut encode = EncodeHandles::new(&any, 1);
        let (fd, _) = std::os::unix::net::UnixStream::pair().unwrap();
        let handle = crate::handle::OsHandle::new(OwnedFd::from(fd));
        let err = encode.put_handle(&handle).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }
}
