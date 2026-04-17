use std::io;
use std::io::{IoSlice, IoSliceMut};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::slice;

use nix::errno::Errno;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};

use tokio::io::unix::AsyncFd;

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
))]
const MSG_FLAGS: MsgFlags = MsgFlags::MSG_CMSG_CLOEXEC;

#[cfg(target_os = "macos")]
const MSG_FLAGS: MsgFlags = MsgFlags::empty();

#[repr(C)]
#[derive(Default, Debug)]
struct MsgHeader {
    payload_len: u32,
    fd_count: u32,
}

impl MsgHeader {
    pub fn as_buf(&self) -> &[u8] {
        unsafe { slice::from_raw_parts((self as *const _) as *const u8, mem::size_of_val(self)) }
    }

    pub fn as_buf_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut((self as *mut _) as *mut u8, mem::size_of_val(self)) }
    }

    pub fn make_buffer(&self) -> Vec<u8> {
        vec![0u8; self.payload_len as usize]
    }
}

/// Data received via `SCM_CREDENTIALS` from a remote process.
#[derive(Debug, Clone)]
pub struct Credentials {
    pid: libc::pid_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
}

impl Credentials {
    /// The remote process identifier.
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// The remote process user ID.
    pub fn uid(&self) -> libc::uid_t {
        self.uid
    }

    /// The remote process group ID.
    pub fn gid(&self) -> libc::gid_t {
        self.gid
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
impl From<nix::sys::socket::UnixCredentials> for Credentials {
    fn from(c: nix::sys::socket::UnixCredentials) -> Self {
        Self {
            pid: c.pid(),
            uid: c.uid(),
            gid: c.gid(),
        }
    }
}

macro_rules! fd_impl {
    ($ty:ty) => {
        #[allow(dead_code)]
        impl $ty {
            pub(crate) unsafe fn from_raw_fd(fd: RawFd) -> io::Result<Self> {
                Ok(Self {
                    inner: AsyncFd::new(unsafe { OwnedFd::from_raw_fd(fd) })?,
                })
            }

            /// Convert from a standard stream.  This is a fallible
            /// operation because registering the file descriptor with
            /// the async runtime may fail.
            ///
            /// # Panics
            ///
            /// This function panics if it is not called from within a runtime with
            /// IO enabled.
            pub fn from_std(stream: UnixStream) -> io::Result<Self> {
                Ok(Self {
                    inner: AsyncFd::new(OwnedFd::from(stream))?,
                })
            }
        }

        impl From<OwnedFd> for $ty {
            fn from(fd: OwnedFd) -> Self {
                Self {
                    inner: AsyncFd::new(fd)
                        .expect("conversion from OwnedFd requires an active tokio runtime"),
                }
            }
        }

        impl From<$ty> for OwnedFd {
            fn from(val: $ty) -> OwnedFd {
                val.inner.into_inner()
            }
        }

        impl AsFd for $ty {
            fn as_fd(&self) -> BorrowedFd<'_> {
                unsafe { BorrowedFd::borrow_raw(self.inner.as_raw_fd()) }
            }
        }

        impl AsRawFd for $ty {
            fn as_raw_fd(&self) -> RawFd {
                self.inner.as_raw_fd()
            }
        }
    };
}

fd_impl!(RawReceiver);
fd_impl!(RawSender);

macro_rules! nix_eintr {
    ($expr:expr) => {
        loop {
            match $expr {
                Err(Errno::EINTR) => continue,
                other => break other,
            }
        }
    };
}

fn recv_impl(
    fd: RawFd,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    fd_count: usize,
    _want_creds: bool,
) -> io::Result<(usize, Option<Credentials>)> {
    let mut iov = [IoSliceMut::new(buf)];

    #[allow(unused_mut)]
    let mut creds = None;

    // Compute the size of ancillary data, combining expected number of file descriptors
    // with any space needed for credentials.  Subtract already-accumulated fds so the
    // cmsg buffer shrinks appropriately on retries.
    let msgspace_size = {
        let remaining = fd_count.saturating_sub(fds.len());
        let fd_size = nix::sys::socket::cmsg_space::<[RawFd; 1]>() * remaining;
        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            let cred_size: usize = _want_creds
                .then(nix::sys::socket::cmsg_space::<nix::sys::socket::UnixCredentials>)
                .unwrap_or_default();
            fd_size + cred_size + 32
        }
        #[cfg(not(any(target_os = "android", target_os = "linux")))]
        {
            fd_size
        }
    };
    let mut cmsgspace = vec![0u8; msgspace_size];

    let msg = nix_eintr!(recvmsg::<()>(fd, &mut iov, Some(&mut cmsgspace), MSG_FLAGS))?;
    if msg.flags.contains(MsgFlags::MSG_CTRUNC) {
        return Err(io::Error::other("control message truncated"));
    }

    let mut received_fds = false;
    for cmsg in msg.cmsgs()? {
        match cmsg {
            ControlMessageOwned::ScmRights(new_fds) if !new_fds.is_empty() => {
                #[cfg(target_os = "macos")]
                unsafe {
                    for &fd in &new_fds {
                        // as per documentation this does not ever fail
                        // with EINTR
                        libc::ioctl(fd, libc::FIOCLEX);
                    }
                }
                fds.extend(
                    new_fds
                        .into_iter()
                        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                );
                received_fds = true;
            }
            ControlMessageOwned::ScmRights(_) => {}
            #[cfg(any(target_os = "android", target_os = "linux"))]
            ControlMessageOwned::ScmCredentials(c) => {
                creds = Some(c.into());
            }
            _ => {}
        }
    }

    if msg.bytes == 0 && !received_fds {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "could not read",
        ));
    }

    Ok((msg.bytes, creds))
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn send_impl(fd: RawFd, data: &[u8], fds: &[RawFd], creds: bool) -> io::Result<usize> {
    let iov = [IoSlice::new(data)];
    let creds = creds.then(nix::sys::socket::UnixCredentials::new);
    let sent = match (fds, creds.as_ref()) {
        ([], None) => nix_eintr!(sendmsg::<()>(fd, &iov, &[], MsgFlags::empty(), None))?,
        ([], Some(creds)) => nix_eintr!(sendmsg::<()>(
            fd,
            &iov,
            &[ControlMessage::ScmCredentials(creds),],
            MsgFlags::empty(),
            None,
        ))?,
        (fds, Some(creds)) => {
            let cmsgs = &[
                ControlMessage::ScmRights(fds),
                ControlMessage::ScmCredentials(creds),
            ];
            nix_eintr!(sendmsg::<()>(fd, &iov, cmsgs, MsgFlags::empty(), None,))?
        }
        (fds, None) => {
            let cmsgs = &[ControlMessage::ScmRights(fds)];
            nix_eintr!(sendmsg::<()>(fd, &iov, cmsgs, MsgFlags::empty(), None,))?
        }
    };
    if sent == 0 {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "could not send"));
    }
    Ok(sent)
}

#[cfg(not(any(target_os = "android", target_os = "linux")))]
fn send_impl(fd: RawFd, data: &[u8], fds: &[RawFd], _creds: bool) -> io::Result<usize> {
    let iov = [IoSlice::new(data)];
    let sent = if !fds.is_empty() {
        nix_eintr!(sendmsg::<()>(
            fd,
            &iov,
            &[ControlMessage::ScmRights(fds)],
            MsgFlags::empty(),
            None,
        ))?
    } else {
        nix_eintr!(sendmsg::<()>(fd, &iov, &[], MsgFlags::empty(), None))?
    };
    if sent == 0 {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "could not send"));
    }
    Ok(sent)
}

/// Creates a raw connected channel.
pub fn raw_channel() -> io::Result<(RawSender, RawReceiver)> {
    let (sender, receiver) = tokio::net::UnixStream::pair()?;
    Ok((
        RawSender::from_std(sender.into_std()?)?,
        RawReceiver::from_std(receiver.into_std()?)?,
    ))
}

/// Creates a raw connected channel from an already extant socket.
pub fn raw_channel_from_std(sender: UnixStream) -> io::Result<(RawSender, RawReceiver)> {
    let receiver = sender.try_clone()?;
    Ok((
        RawSender::from_std(sender)?,
        RawReceiver::from_std(receiver)?,
    ))
}

/// An async raw receiver.
#[derive(Debug)]
pub struct RawReceiver {
    inner: AsyncFd<OwnedFd>,
}

impl RawReceiver {
    /// Connects a receiver to a named unix socket.
    pub async fn connect<P: AsRef<Path>>(p: P) -> io::Result<RawReceiver> {
        let stream = tokio::net::UnixStream::connect(p).await?;
        RawReceiver::from_std(stream.into_std()?)
    }

    /// Receives raw bytes from the socket.
    pub async fn recv(&self) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
        let mut header = MsgHeader::default();
        self.recv_impl(header.as_buf_mut(), 0, false).await?;
        let mut buf = header.make_buffer();
        let (_, fds, _) = self
            .recv_impl(&mut buf, header.fd_count as usize, false)
            .await?;
        Ok((buf, fds))
    }

    /// Receives raw bytes and credentials from the socket.
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub async fn recv_with_credentials(&self) -> io::Result<(Vec<u8>, Vec<OwnedFd>, Credentials)> {
        nix::sys::socket::setsockopt(&self.inner, nix::sys::socket::sockopt::PassCred, &true)?;
        let mut header = MsgHeader::default();
        let (_, _, creds) = self.recv_impl(header.as_buf_mut(), 0, true).await?;
        let creds = creds.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Remote did not provide credentials",
            )
        })?;
        let mut buf = header.make_buffer();
        let (_, fds, _) = self
            .recv_impl(&mut buf, header.fd_count as usize, false)
            .await?;
        Ok((buf, fds, creds))
    }

    async fn recv_impl(
        &self,
        buf: &mut [u8],
        fd_count: usize,
        want_creds: bool,
    ) -> io::Result<(usize, Vec<OwnedFd>, Option<Credentials>)> {
        let mut pos = 0;
        let mut fds = Vec::new();
        let mut last_creds = None;

        loop {
            let mut guard = self.inner.readable().await?;
            let (bytes, creds) = match guard.try_io(|inner| {
                recv_impl(
                    inner.as_raw_fd(),
                    &mut buf[pos..],
                    &mut fds,
                    fd_count,
                    want_creds,
                )
            }) {
                Ok(result) => result,
                Err(_would_block) => continue,
            }?;

            if creds.is_some() {
                last_creds = creds;
            }
            pos += bytes;
            if pos >= buf.len() && fds.len() >= fd_count {
                return Ok((pos, fds, last_creds));
            }
        }
    }
}

unsafe impl Send for RawReceiver {}
unsafe impl Sync for RawReceiver {}

/// An async raw sender.
#[derive(Debug)]
pub struct RawSender {
    inner: AsyncFd<OwnedFd>,
}

impl RawSender {
    /// Sends raw bytes and fds.
    pub async fn send(&self, data: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        let header = MsgHeader {
            payload_len: data.len() as u32,
            fd_count: fds.len() as u32,
        };
        self.send_impl(header.as_buf(), &[][..], false).await?;
        self.send_impl(data, fds, false).await
    }

    /// Sends raw bytes and fds along with current process credentials.
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub async fn send_with_credentials(
        &self,
        data: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> io::Result<usize> {
        let header = MsgHeader {
            payload_len: data.len() as u32,
            fd_count: fds.len() as u32,
        };
        self.send_impl(header.as_buf(), &[][..], true).await?;
        self.send_impl(data, fds, false).await
    }

    async fn send_impl(
        &self,
        data: &[u8],
        fds: &[BorrowedFd<'_>],
        creds: bool,
    ) -> io::Result<usize> {
        // SAFETY: BorrowedFd is #[repr(transparent)] over RawFd
        let raw_fds: &[RawFd] =
            unsafe { slice::from_raw_parts(fds.as_ptr() as *const RawFd, fds.len()) };
        let mut pos = 0;
        let mut send_fds = raw_fds;
        loop {
            let mut guard = self.inner.writable().await?;
            let sent = match guard
                .try_io(|inner| send_impl(inner.as_raw_fd(), &data[pos..], send_fds, creds))
            {
                Ok(result) => result,
                Err(_would_block) => continue,
            }?;
            pos += sent;
            send_fds = &[][..];
            if pos >= data.len() {
                return Ok(pos);
            }
        }
    }
}

unsafe impl Send for RawSender {}
unsafe impl Sync for RawSender {}
