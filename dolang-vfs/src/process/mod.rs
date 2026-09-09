use std::{
    io::{self, IoSlice},
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};

use dolang_rpc::handle::DefaultHandle;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    task::JoinHandle,
};

use crate::{
    STREAM_CHUNK_SIZE, SessionMode, Vfs, VfsInner, client, direct,
    error::{Error, ErrorKind, Result},
    path,
    target::OperatingSystem,
};

mod foreign;

pub(crate) use foreign::ProcessFamily;
pub use foreign::{Process, ProcessExit, ProcessInfo, Processes, StartTime};

/// Terminal status of a spawned process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessStatus {
    Exited(i32),
    Signaled(i32),
}

impl ProcessStatus {
    /// Returns whether the process exited successfully.
    pub const fn success(self) -> bool {
        matches!(self, Self::Exited(0))
    }

    /// Returns the numeric exit code, if the process exited normally.
    pub const fn code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Signaled(_) => None,
        }
    }

    /// Returns the native signal number, if a signal terminated the process.
    pub const fn signal(self) -> Option<i32> {
        match self {
            Self::Exited(_) => None,
            Self::Signaled(signal) => Some(signal),
        }
    }

    pub(crate) fn from_native(status: std::process::ExitStatus) -> io::Result<Self> {
        if let Some(code) = status.code() {
            return Ok(Self::Exited(code));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ok(Self::Signaled(signal));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process returned an unrepresentable terminal status",
        ))
    }
}

/// Whether a spawned process is attached to the foreground process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessControl {
    /// Run in the foreground process group.
    Foreground,
    /// Run without foreground process-group control.
    Background,
}

/// A Unix process signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Signal {
    Hup,
    Int,
    Quit,
    Ill,
    Trap,
    Abrt,
    Emt,
    Fpe,
    Kill,
    Bus,
    Segv,
    Sys,
    Pipe,
    Alrm,
    Term,
    Urg,
    Stop,
    Tstp,
    Cont,
    Chld,
    Ttin,
    Ttou,
    Io,
    Xcpu,
    Xfsz,
    Vtalrm,
    Prof,
    Winch,
    Info,
    Usr1,
    Usr2,
    Stkflt,
    Pwr,
    Thr,
    Librt,
    Number(i32),
}

impl Signal {
    /// Returns whether this signal exists on an operating system.
    pub fn is_supported(self, operating_system: OperatingSystem) -> bool {
        use OperatingSystem::{FreeBsd, Linux, Macos, Windows};
        match self {
            Self::Emt | Self::Info => matches!(operating_system, FreeBsd | Macos),
            Self::Stkflt | Self::Pwr => operating_system == Linux,
            Self::Thr | Self::Librt => operating_system == FreeBsd,
            Self::Number(_) => operating_system != Windows,
            _ => operating_system != Windows,
        }
    }
}
enum CommandInner<'a> {
    Client(client::Command<'a>),
    Direct(direct::Command<'a>),
}

/// Builds a process to spawn, relaying stdio across VFS domains when a
/// given endpoint cannot be consumed directly by the spawn target.
pub struct Command<'a> {
    inner: CommandInner<'a>,
    vfs: Vfs,
    stdin: Option<StdioRecv>,
    stdout: Option<StdioSend>,
    stderr: Option<StdioSend>,
}

impl<'a> Command<'a> {
    pub(crate) fn new(vfs: &'a Vfs, program: path::Path<'_>) -> Self {
        let inner = match &vfs.inner {
            VfsInner::Client(client) => CommandInner::Client(client.command(program)),
            VfsInner::Direct(direct) => CommandInner::Direct(direct.command(program)),
        };
        Self {
            inner,
            vfs: vfs.clone(),
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }
}

enum ChildInner {
    Client(client::Child),
    Direct(Box<direct::Child>),
}

/// Tasks pumping bytes between a foreign-domain stdio endpoint and a pipe
/// created in the spawn target's own domain, not yet started.
///
/// Relay tasks are only started once the underlying process has actually
/// been spawned, so a failure setting up the spawn itself just drops the
/// held endpoints (triggering their own best-effort cleanup) instead of
/// leaking a running task.
#[derive(Default)]
struct PendingRelays {
    inputs: Vec<(StdioRecv, StdioSend)>,
    outputs: Vec<(StdioRecv, StdioSend)>,
}

impl PendingRelays {
    fn start(self) -> ActiveRelays {
        let spawn_all = |pairs: Vec<(StdioRecv, StdioSend)>| {
            pairs
                .into_iter()
                .map(|(src, dst)| tokio::spawn(relay(src, dst)))
                .collect()
        };
        ActiveRelays {
            inputs: spawn_all(self.inputs),
            outputs: spawn_all(self.outputs),
        }
    }
}

#[derive(Default)]
struct ActiveRelays {
    inputs: Vec<JoinHandle<()>>,
    outputs: Vec<JoinHandle<()>>,
}

impl ActiveRelays {
    fn abort_inputs(&mut self) {
        for handle in self.inputs.drain(..) {
            handle.abort();
        }
    }

    /// Waits for output relays to finish flushing everything the child
    /// already produced before it exited. Input relays are aborted instead
    /// of awaited, since no further input can matter once the child is gone.
    ///
    /// Once the child has exited, its end of each output pipe is closed, so
    /// the relay's copy loop reaches EOF and returns on its own; this just
    /// ensures the caller can't observe a partially-relayed destination.
    async fn finish(&mut self) {
        self.abort_inputs();
        for handle in self.outputs.drain(..) {
            let _ = handle.await;
        }
    }

    /// Aborts every relay task without waiting, for use when giving up on
    /// the child without having confirmed it actually exited (e.g. `Drop`).
    fn abandon(&mut self) {
        self.abort_inputs();
        for handle in self.outputs.drain(..) {
            handle.abort();
        }
    }
}

impl Drop for ActiveRelays {
    fn drop(&mut self) {
        self.abandon();
    }
}

/// A spawned process, possibly relaying stdio across VFS domains.
pub struct Child {
    inner: ChildInner,
    relays: ActiveRelays,
}

impl Child {
    /// Waits for the process to exit.
    pub async fn wait(&mut self) -> Result<ProcessStatus> {
        let status = match &mut self.inner {
            ChildInner::Client(child) => child.wait().await,
            ChildInner::Direct(child) => child.wait().await,
        }?;
        self.relays.finish().await;
        Ok(status)
    }

    /// Terminates the process and returns its status when it has exited.
    pub async fn terminate(mut self) -> Result<Option<ProcessStatus>> {
        self.relays.abort_inputs();
        let result = match self.inner {
            ChildInner::Client(child) => child.terminate().await,
            ChildInner::Direct(child) => child.terminate().await,
        };
        if result.as_ref().is_ok_and(Option::is_some) {
            self.relays.finish().await;
        } else {
            self.relays.abandon();
        }
        result
    }
}

/// Returns whether `stdio` can be handed directly to a process spawned in
/// `target`'s domain without relaying.
fn is_direct_recv(target: &Vfs, stdio: &StdioRecv) -> bool {
    match (&target.inner, &stdio.0) {
        (VfsInner::Direct(_), StdioRecvInner::Native(_)) => true,
        (VfsInner::Client(client), StdioRecvInner::Remote(remote)) => {
            client.is_same_vfs(remote.client())
        }
        (VfsInner::Client(client), StdioRecvInner::Native(_)) => {
            client.mode() == SessionMode::Native
        }
        _ => false,
    }
}

/// Returns whether `stdio` can be handed directly to a process spawned in
/// `target`'s domain without relaying.
fn is_direct_send(target: &Vfs, stdio: &StdioSend) -> bool {
    match (&target.inner, &stdio.0) {
        (VfsInner::Direct(_), StdioSendInner::Native(_)) => true,
        (VfsInner::Client(client), StdioSendInner::Remote(remote)) => {
            client.is_same_vfs(remote.client())
        }
        (VfsInner::Client(client), StdioSendInner::Native(_)) => {
            client.mode() == SessionMode::Native
        }
        _ => false,
    }
}

/// Classifies a stdin endpoint for a process about to be spawned in
/// `target`'s domain, creating a relay pipe and queuing a pump task in
/// `relays` if the endpoint cannot be consumed directly.
async fn classify_recv(
    target: &Vfs,
    stdio: StdioRecv,
    relays: &mut PendingRelays,
) -> Result<StdioRecv> {
    if is_direct_recv(target, &stdio) {
        return Ok(stdio);
    }
    let (send, recv) = target.pipe(None).await?;
    relays.inputs.push((stdio, send));
    Ok(recv)
}

/// Classifies a stdout/stderr endpoint for a process about to be spawned in
/// `target`'s domain, creating a relay pipe and queuing a pump task in
/// `relays` if the endpoint cannot be consumed directly.
async fn classify_send(
    target: &Vfs,
    stdio: StdioSend,
    relays: &mut PendingRelays,
) -> Result<StdioSend> {
    if is_direct_send(target, &stdio) {
        return Ok(stdio);
    }
    let (send, recv) = target.pipe(None).await?;
    relays.outputs.push((recv, stdio));
    Ok(send)
}

impl<'a> Command<'a> {
    /// Appends an argument to the program invocation.
    pub fn arg(&mut self, arg: &str) -> &mut Self {
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.arg(arg);
            }
            CommandInner::Direct(builder) => {
                builder.arg(arg);
            }
        }
        self
    }

    /// Sets an environment variable for the child.
    pub fn env(&mut self, key: &str, val: &str) -> &mut Self {
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.env(key, val);
            }
            CommandInner::Direct(builder) => {
                builder.env(key, val);
            }
        }
        self
    }

    /// Removes an environment variable from the child.
    pub fn env_remove(&mut self, key: &str) -> &mut Self {
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.env_remove(key);
            }
            CommandInner::Direct(builder) => {
                builder.env_remove(key);
            }
        }
        self
    }

    /// Sets the child's working directory.
    pub fn current_dir(&mut self, dir: path::Path<'_>) -> &mut Self {
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.current_dir(dir);
            }
            CommandInner::Direct(builder) => {
                builder.current_dir(dir);
            }
        }
        self
    }

    /// Sets the child's standard input.
    pub fn stdin(&mut self, stdio: StdioRecv) -> Result<&mut Self> {
        self.stdin = Some(stdio);
        Ok(self)
    }

    /// Sets the child's standard output.
    pub fn stdout(&mut self, stdio: StdioSend) -> Result<&mut Self> {
        self.stdout = Some(stdio);
        Ok(self)
    }

    /// Inherit the host process's standard input.
    ///
    /// Opaque remote clients treat terminal input as null because Tokio cannot
    /// cancel an outstanding terminal read. Redirected input is relayed to the
    /// remote process.
    pub fn stdin_inherit(&mut self) -> Result<&mut Self> {
        self.stdin = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stdin_inherit()?;
            }
            CommandInner::Direct(builder) => {
                builder.stdin_inherit()?;
            }
        }
        Ok(self)
    }

    /// Inherits the host process's standard output.
    pub fn stdout_inherit(&mut self) -> Result<&mut Self> {
        self.stdout = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stdout_inherit()?;
            }
            CommandInner::Direct(builder) => {
                builder.stdout_inherit()?;
            }
        }
        Ok(self)
    }

    /// Connects the child's standard output to the parent process's standard
    /// error.
    pub fn stdout_inherit_stderr(&mut self) -> Result<&mut Self> {
        self.stdout = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stdout_inherit_stderr()?;
            }
            CommandInner::Direct(builder) => {
                builder.stdout_inherit_stderr()?;
            }
        }
        Ok(self)
    }

    /// Connects the child's standard input to the null device.
    pub fn stdin_null(&mut self) -> &mut Self {
        self.stdin = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stdin_null();
            }
            CommandInner::Direct(builder) => {
                builder.stdin_null();
            }
        }
        self
    }

    /// Connects the child's standard output to the null device.
    pub fn stdout_null(&mut self) -> &mut Self {
        self.stdout = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stdout_null();
            }
            CommandInner::Direct(builder) => {
                builder.stdout_null();
            }
        }
        self
    }

    /// Sets the child's standard error.
    pub fn stderr(&mut self, stdio: StdioSend) -> Result<&mut Self> {
        self.stderr = Some(stdio);
        Ok(self)
    }

    /// Inherits the host process's standard error.
    pub fn stderr_inherit(&mut self) -> Result<&mut Self> {
        self.stderr = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stderr_inherit()?;
            }
            CommandInner::Direct(builder) => {
                builder.stderr_inherit()?;
            }
        }
        Ok(self)
    }

    /// Connects the child's standard error to the same destination as its
    /// configured standard output.
    pub fn stderr_to_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stderr_to_stdout()?;
            }
            CommandInner::Direct(builder) => {
                builder.stderr_to_stdout()?;
            }
        }
        Ok(self)
    }

    /// Connects the child's standard error to the parent process's standard
    /// output.
    pub fn stderr_inherit_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stderr_inherit_stdout()?;
            }
            CommandInner::Direct(builder) => {
                builder.stderr_inherit_stdout()?;
            }
        }
        Ok(self)
    }

    /// Connects the child's standard error to the null device.
    pub fn stderr_null(&mut self) -> &mut Self {
        self.stderr = None;
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.stderr_null();
            }
            CommandInner::Direct(builder) => {
                builder.stderr_null();
            }
        }
        self
    }

    /// Sets foreground or background process control behavior.
    pub fn process_control(&mut self, control: ProcessControl) -> &mut Self {
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.process_control(control);
            }
            CommandInner::Direct(builder) => {
                builder.process_control(control);
            }
        }
        self
    }

    /// Sets the policy used to terminate the child.
    pub fn termination_policy(&mut self, policy: TerminationPolicy) -> &mut Self {
        match &mut self.inner {
            CommandInner::Client(builder) => {
                builder.termination_policy(policy);
            }
            CommandInner::Direct(builder) => {
                builder.termination_policy(policy);
            }
        }
        self
    }

    /// Spawns the configured process.
    pub async fn spawn(mut self) -> Result<Child> {
        let mut relays = PendingRelays::default();
        if let Some(stdio) = self.stdin.take() {
            let stdio = classify_recv(&self.vfs, stdio, &mut relays).await?;
            match &mut self.inner {
                CommandInner::Client(builder) => {
                    builder.stdin(stdio)?;
                }
                CommandInner::Direct(builder) => {
                    builder.stdin(stdio)?;
                }
            }
        }
        if let Some(stdio) = self.stdout.take() {
            let stdio = classify_send(&self.vfs, stdio, &mut relays).await?;
            match &mut self.inner {
                CommandInner::Client(builder) => {
                    builder.stdout(stdio)?;
                }
                CommandInner::Direct(builder) => {
                    builder.stdout(stdio)?;
                }
            }
        }
        if let Some(stdio) = self.stderr.take() {
            let stdio = classify_send(&self.vfs, stdio, &mut relays).await?;
            match &mut self.inner {
                CommandInner::Client(builder) => {
                    builder.stderr(stdio)?;
                }
                CommandInner::Direct(builder) => {
                    builder.stderr(stdio)?;
                }
            }
        }
        let inner = match self.inner {
            CommandInner::Client(builder) => builder.spawn().await.map(ChildInner::Client),
            CommandInner::Direct(builder) => builder
                .spawn()
                .await
                .map(|x| ChildInner::Direct(Box::new(x))),
        }?;
        Ok(Child {
            inner,
            relays: relays.start(),
        })
    }
}

/// Policy used to terminate a spawned process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminationPolicy {
    /// Signal sent to request graceful termination.
    pub(crate) signal: Signal,
    /// Time to wait before optional forced termination.
    pub(crate) grace: Duration,
    /// Whether to force termination after the grace period.
    pub(crate) force: bool,
}

impl TerminationPolicy {
    /// Creates a process-termination policy.
    pub const fn new(signal: Signal, grace: Duration, force: bool) -> Self {
        Self {
            signal,
            grace,
            force,
        }
    }
    /// Returns the signal used to request graceful termination.
    pub const fn signal(self) -> Signal {
        self.signal
    }
    /// Returns the grace period before forced termination.
    pub const fn grace(self) -> Duration {
        self.grace
    }
    /// Returns whether the process is forcibly terminated after the grace period.
    pub const fn force(self) -> bool {
        self.force
    }
    /// Sets the graceful-termination signal.
    pub fn set_signal(&mut self, signal: Signal) -> &mut Self {
        self.signal = signal;
        self
    }
    /// Sets the grace period before forced termination.
    pub fn set_grace(&mut self, grace: Duration) -> &mut Self {
        self.grace = grace;
        self
    }
    /// Selects whether to force termination after the grace period.
    pub fn set_force(&mut self, force: bool) -> &mut Self {
        self.force = force;
        self
    }
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            signal: Signal::Term,
            grace: Duration::from_secs(5),
            force: true,
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::{Signal, TerminationPolicy};
    use std::time::Duration;

    #[test]
    fn termination_policy_construction_and_mutation() {
        let mut policy = TerminationPolicy::new(Signal::Int, Duration::from_secs(2), false);
        assert_eq!(policy.signal(), Signal::Int);
        assert_eq!(policy.grace(), Duration::from_secs(2));
        assert!(!policy.force());
        policy
            .set_signal(Signal::Term)
            .set_grace(Duration::from_secs(3))
            .set_force(true);
        assert_eq!(
            policy,
            TerminationPolicy::new(Signal::Term, Duration::from_secs(3), true)
        );
    }
}

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::{
    io::{PipeReader, PipeWriter, Read as _, Write as _},
    os::windows::io::OwnedHandle,
    sync::Arc,
};
/// A writable standard-I/O endpoint from either a local or remote VFS.
#[derive(Debug)]
pub struct StdioSend(pub(crate) StdioSendInner);

#[derive(Debug)]
pub(crate) enum StdioSendInner {
    /// Endpoint backed by a local OS resource.
    Native(NativeStdioSend),
    /// Endpoint retained by a remote VFS session.
    Remote(crate::client::RemoteStdioSend),
}

/// A readable standard-I/O endpoint from either a local or remote VFS.
#[derive(Debug)]
pub struct StdioRecv(pub(crate) StdioRecvInner);

#[derive(Debug)]
pub(crate) enum StdioRecvInner {
    /// Endpoint backed by a local OS resource.
    Native(NativeStdioRecv),
    /// Endpoint retained by a remote VFS session.
    Remote(crate::client::RemoteStdioRecv),
}

#[cfg(unix)]
/// Local writable standard-I/O resource on Unix.
#[derive(Debug)]
pub(crate) enum NativeStdioSend {
    /// Unix pipe sender.
    Pipe(tokio::net::unix::pipe::Sender),
    /// Asynchronous file.
    File(File),
}

#[cfg(unix)]
/// Local readable standard-I/O resource on Unix.
#[derive(Debug)]
pub(crate) enum NativeStdioRecv {
    /// Unix pipe receiver.
    Pipe(tokio::net::unix::pipe::Receiver),
    /// Asynchronous file.
    File(File),
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) enum NativeStdioSend {
    Pipe {
        inner: Arc<PipeWriter>,
        pending: Option<JoinHandle<io::Result<usize>>>,
    },
    File(File),
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) enum NativeStdioRecv {
    Pipe {
        inner: Arc<PipeReader>,
        pending: Option<JoinHandle<(Vec<u8>, io::Result<usize>)>>,
        ready: Option<(Vec<u8>, usize)>,
    },
    File(File),
}

/// Pumps bytes from `src` to `dst` until EOF or error, then shuts `dst` down.
pub(crate) async fn relay<R, W>(src: R, mut dst: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut src = BufReader::with_capacity(STREAM_CHUNK_SIZE, src);
    let _ = tokio::io::copy_buf(&mut src, &mut dst).await;
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
            StdioSend(StdioSendInner::Native(NativeStdioSend::Pipe(send))),
            StdioRecv(StdioRecvInner::Native(NativeStdioRecv::Pipe(recv))),
        ))
    }
    #[cfg(windows)]
    {
        let (recv, send) = match buf_size {
            Some(size) => create_pipe_sized(size)?,
            None => std::io::pipe()?,
        };
        Ok((
            StdioSend(StdioSendInner::Native(NativeStdioSend::Pipe {
                inner: Arc::new(send),
                pending: None,
            })),
            StdioRecv(StdioRecvInner::Native(NativeStdioRecv::Pipe {
                inner: Arc::new(recv),
                pending: None,
                ready: None,
            })),
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
    /// Creates a writable child-stdio endpoint from a VFS file.
    pub fn from_file(file: File) -> Self {
        Self(StdioSendInner::Native(NativeStdioSend::File(file)))
    }

    pub(crate) fn remote(remote: crate::client::RemoteStdioSend) -> Self {
        Self(StdioSendInner::Remote(remote))
    }

    /// Clones this writable child-stdio endpoint.
    pub async fn try_clone(&self) -> Result<Self> {
        match &self.0 {
            #[cfg(unix)]
            StdioSendInner::Native(NativeStdioSend::Pipe(pipe)) => {
                let fd = pipe.as_fd().try_clone_to_owned()?;
                Ok(Self(StdioSendInner::Native(NativeStdioSend::Pipe(
                    tokio::net::unix::pipe::Sender::from_owned_fd_unchecked(fd)?,
                ))))
            }
            #[cfg(windows)]
            StdioSendInner::Native(NativeStdioSend::Pipe { inner, .. }) => {
                Ok(Self(StdioSendInner::Native(NativeStdioSend::Pipe {
                    inner: Arc::new(inner.try_clone()?),
                    pending: None,
                })))
            }
            StdioSendInner::Native(NativeStdioSend::File(file)) => Ok(Self(
                StdioSendInner::Native(NativeStdioSend::File(file.try_clone().await?)),
            )),
            StdioSendInner::Remote(remote) => Ok(Self::remote(remote.try_clone().await?)),
        }
    }

    /// Converts this endpoint into native process stdio.
    ///
    /// Remote endpoints cannot be converted.
    pub async fn into_stdio(self) -> Result<Stdio> {
        match self.0 {
            StdioSendInner::Native(NativeStdioSend::File(file)) => {
                Ok(Stdio::from(file.into_std().await))
            }
            #[cfg(unix)]
            StdioSendInner::Native(NativeStdioSend::Pipe(pipe)) => {
                let fd: OwnedFd = pipe.into_blocking_fd()?;
                Ok(Stdio::from(fd))
            }
            #[cfg(windows)]
            StdioSendInner::Native(NativeStdioSend::Pipe { inner, pending }) => {
                if pending.is_some() {
                    return Err(Error::new(
                        ErrorKind::ResourceBusy,
                        "cannot convert StdioSend while an async write is in flight",
                    ));
                }
                Ok(Arc::try_unwrap(inner)
                    .or_else(|inner| inner.try_clone())
                    .map(Stdio::from)?)
            }
            StdioSendInner::Remote(_) => Err(Error::new(
                ErrorKind::InvalidInput,
                "remote stdio cannot be converted to a native handle",
            )),
        }
    }

    pub(crate) async fn into_blocking_handle(self) -> io::Result<DefaultHandle> {
        match self.0 {
            StdioSendInner::Native(NativeStdioSend::File(file)) => Ok(file.into_std().await.into()),
            #[cfg(unix)]
            StdioSendInner::Native(NativeStdioSend::Pipe(pipe)) => pipe.into_blocking_fd(),
            #[cfg(windows)]
            StdioSendInner::Native(NativeStdioSend::Pipe { inner, pending }) => {
                if pending.is_some() {
                    return Err(io::Error::other(
                        "cannot convert StdioSend while an async write is in flight",
                    ));
                }
                let pipe = Arc::try_unwrap(inner).or_else(|inner| inner.try_clone())?;
                Ok(OwnedHandle::from(pipe))
            }
            StdioSendInner::Remote(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote stdio has no native handle",
            )),
        }
    }
}

impl StdioRecv {
    /// Creates a readable child-stdio endpoint from a VFS file.
    pub fn from_file(file: File) -> Self {
        Self(StdioRecvInner::Native(NativeStdioRecv::File(file)))
    }

    pub(crate) fn remote(remote: crate::client::RemoteStdioRecv) -> Self {
        Self(StdioRecvInner::Remote(remote))
    }

    /// Clones this readable child-stdio endpoint.
    pub async fn try_clone(&self) -> Result<Self> {
        match &self.0 {
            #[cfg(unix)]
            StdioRecvInner::Native(NativeStdioRecv::Pipe(pipe)) => {
                let fd = pipe.as_fd().try_clone_to_owned()?;
                Ok(Self(StdioRecvInner::Native(NativeStdioRecv::Pipe(
                    tokio::net::unix::pipe::Receiver::from_owned_fd_unchecked(fd)?,
                ))))
            }
            #[cfg(windows)]
            StdioRecvInner::Native(NativeStdioRecv::Pipe { inner, .. }) => {
                Ok(Self(StdioRecvInner::Native(NativeStdioRecv::Pipe {
                    inner: Arc::new(inner.try_clone()?),
                    pending: None,
                    ready: None,
                })))
            }
            StdioRecvInner::Native(NativeStdioRecv::File(file)) => Ok(Self(
                StdioRecvInner::Native(NativeStdioRecv::File(file.try_clone().await?)),
            )),
            StdioRecvInner::Remote(remote) => Ok(Self::remote(remote.try_clone().await?)),
        }
    }

    /// Converts this endpoint into native process stdio.
    ///
    /// Remote endpoints cannot be converted.
    pub async fn into_stdio(self) -> Result<Stdio> {
        match self.0 {
            StdioRecvInner::Native(NativeStdioRecv::File(file)) => {
                Ok(Stdio::from(file.into_std().await))
            }
            #[cfg(unix)]
            StdioRecvInner::Native(NativeStdioRecv::Pipe(pipe)) => {
                let fd: OwnedFd = pipe.into_blocking_fd()?;
                Ok(Stdio::from(fd))
            }
            #[cfg(windows)]
            StdioRecvInner::Native(NativeStdioRecv::Pipe { inner, pending, .. }) => {
                if pending.is_some() {
                    return Err(Error::new(
                        ErrorKind::ResourceBusy,
                        "cannot convert StdioRecv while an async read is in flight",
                    ));
                }
                Ok(Arc::try_unwrap(inner)
                    .or_else(|inner| inner.try_clone())
                    .map(Stdio::from)?)
            }
            StdioRecvInner::Remote(_) => Err(Error::new(
                ErrorKind::InvalidInput,
                "remote stdio cannot be converted to a native handle",
            )),
        }
    }

    pub(crate) async fn into_blocking_handle(self) -> io::Result<DefaultHandle> {
        match self.0 {
            StdioRecvInner::Native(NativeStdioRecv::File(file)) => Ok(file.into_std().await.into()),
            #[cfg(unix)]
            StdioRecvInner::Native(NativeStdioRecv::Pipe(pipe)) => pipe.into_blocking_fd(),
            #[cfg(windows)]
            StdioRecvInner::Native(NativeStdioRecv::Pipe { inner, pending, .. }) => {
                if pending.is_some() {
                    return Err(io::Error::other(
                        "cannot convert StdioRecv while an async read is in flight",
                    ));
                }
                let pipe = Arc::try_unwrap(inner).or_else(|inner| inner.try_clone())?;
                Ok(OwnedHandle::from(pipe))
            }
            StdioRecvInner::Remote(_) => Err(io::Error::new(
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
        match &mut self.0 {
            StdioSendInner::Native(native) => Pin::new(native).poll_write(cx, buf),
            StdioSendInner::Remote(remote) => Pin::new(remote).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut self.0 {
            StdioSendInner::Native(native) => Pin::new(native).poll_write_vectored(cx, bufs),
            StdioSendInner::Remote(remote) => Pin::new(remote).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match &self.0 {
            StdioSendInner::Native(native) => native.is_write_vectored(),
            StdioSendInner::Remote(remote) => remote.is_write_vectored(),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            StdioSendInner::Native(native) => Pin::new(native).poll_flush(cx),
            StdioSendInner::Remote(remote) => Pin::new(remote).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.0 {
            StdioSendInner::Native(native) => Pin::new(native).poll_shutdown(cx),
            StdioSendInner::Remote(remote) => Pin::new(remote).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for StdioRecv {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.0 {
            StdioRecvInner::Native(native) => Pin::new(native).poll_read(cx, buf),
            StdioRecvInner::Remote(remote) => Pin::new(remote).poll_read(cx, buf),
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
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Pipe(pipe) => Pin::new(pipe).poll_write_vectored(cx, bufs),
            Self::File(file) => Pin::new(file).poll_write_vectored(cx, bufs),
        }
    }
    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Pipe(pipe) => pipe.is_write_vectored(),
            Self::File(file) => file.is_write_vectored(),
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
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::File(file) => Pin::new(file).poll_write_vectored(cx, bufs),
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
                let mut data = Vec::new();
                for buf in bufs {
                    data.extend_from_slice(buf);
                }
                if data.is_empty() {
                    return Poll::Ready(Ok(0));
                }
                let inner = Arc::clone(inner);
                *pending = Some(tokio::task::spawn_blocking(move || (&*inner).write(&data)));
                self.poll_write_vectored(cx, &[])
            }
        }
    }
    fn is_write_vectored(&self) -> bool {
        true
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
