use std::{
    collections::HashMap,
    future::Future,
    io, mem,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
};

use bytes::{Bytes, BytesMut};
use typed_path::{Utf8TypedPath, Utf8TypedPathBuf};

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;

use tokio::{
    fs::{self, File as TokioFile},
    io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf},
    process::Command as TokioCommand,
    sync::Mutex,
};

use wax::{
    Glob,
    walk::{DepthBehavior, DepthMax, Entry, LinkBehavior, WalkBehavior},
};

use crate::{
    Vfs, directory,
    error::{Error, ErrorKind, HandoffError, Result},
    extension::ExtensionSet,
    extension::{self, DirectContext, ExtContext, VfsExtension},
    file::{self, AccessFlags, FileLockRequest, StreamEntry},
    file::{XattrEntry, XattrNamespace},
    metadata::{FsMetadata, Metadata, MetadataPatch},
    path::{WellKnownPath, native_path, typed_path},
    process::{self, ProcessControl, ProcessStatus, StdioRecv, StdioSend, TerminationPolicy},
    security::{Acl, AclKind, PrincipalId, PrincipalIdKind, SecurityInfo, SidName},
    session::{self, Query},
    target::TargetInfo,
};
use dolang_winterop::security::{SecDesc, Sid};

use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

mod lock;
#[cfg(unix)]
mod security;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::ReadDir;
#[cfg(windows)]
pub(crate) use windows::ReadDir;

pub(crate) use lock::{FileLock, FileLocks};

/// A [`Vfs`] that operates in the local process environment.
#[derive(Debug, Clone)]
pub struct Direct {
    path_cache: Arc<PathCache>,
    initial: Arc<Query>,
}

/// Local file-open options returned by [`Direct::open_options`](crate::Vfs::open_options).
#[derive(Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
    no_follow: bool,
}

/// Local process-spawn options returned by [`Direct::command`](crate::Vfs::command).
pub struct Command<'a> {
    direct: &'a Direct,
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, Option<String>>,
    cwd: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    stdin_resource: Option<StdioRecv>,
    stdout_resource: Option<StdioSend>,
    stderr_resource: Option<StdioSend>,
    stderr_to_stdout: bool,
    process_control: ProcessControl,
    termination_policy: TerminationPolicy,
    error: Option<Error>,
}

/// A process spawned by [`Direct`].
pub struct Child {
    inner: tokio::process::Child,
    process_control: ProcessControl,
    termination_policy: TerminationPolicy,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

/// A local asynchronous file handle.
///
/// The handle is a shared `std::fs::File` plus a cursor this crate maintains
/// itself. Byte I/O is positional: every read and write names the offset it
/// acts on, takes `&self`, and holds no state on the handle, so any number of
/// them may be in flight at once. The kernel's own file offset is left unused
/// except where a descriptor is handed to another process.
///
/// The cursor-based traits are layered on top for callers that want a stream.
/// Their state is allocated lazily and reachable for mutation only through
/// `&mut self`.
#[derive(Debug)]
pub struct File {
    file: Arc<std::fs::File>,
    flags: FileFlags,
    locks: FileLocks,
    cursor: OnceLock<Box<CursorState>>,
}

bitflags::bitflags! {
    /// Properties fixed when a file is opened.
    #[derive(Clone, Copy, Debug)]
    struct FileFlags: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const APPEND = 1 << 2;
        const SEEKABLE = 1 << 3;
    }
}

/// State used only by the cursor-based asynchronous I/O traits.
#[derive(Debug)]
struct CursorState {
    /// Offset of the next byte the cursor-based traits will hand to a caller.
    ///
    cursor: u64,
    /// Bytes read from the file but not yet delivered, starting at
    /// `pending_read[pending_pos..]` and living at file offsets
    /// `[cursor, cursor + undelivered)`.
    ///
    /// Because the cursor only advances as bytes reach the caller, a seek can
    /// simply discard this rather than rewinding the kernel by the unread
    /// amount, which is the bookkeeping `tokio::fs::File` needs and we do not.
    ///
    /// Consumption is tracked with an index rather than `Buf::advance` on
    /// purpose: advancing walks the buffer's start pointer forward, and
    /// `clear` does not walk it back, so a recycled buffer would lose its
    /// capacity a little at a time and reallocate on every read.
    pending_read: BytesMut,
    pending_pos: usize,
    /// Buffer recycled across writes, so streaming does not allocate per call.
    write_scratch: BytesMut,
    op: CursorOp,
    /// Position a started seek will report from `poll_complete`.
    seek_to: Option<u64>,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            cursor: 0,
            pending_read: BytesMut::new(),
            pending_pos: 0,
            write_scratch: BytesMut::new(),
            op: CursorOp::Idle,
            seek_to: None,
        }
    }
}

/// Largest transfer handed to a single blocking worker.
///
/// This sizes a *syscall*, which is why it is not
/// [`crate::STREAM_CHUNK_SIZE`] — that constant sizes an RPC fragment, and
/// borrowing it here silently turned one large write into several, costing a
/// round trip to the blocking pool for each. Matches what `tokio::fs` uses for
/// the same job.
const MAX_BLOCKING_IO: usize = 2 * 1024 * 1024;

/// Blocking work the cursor-based traits have outstanding.
#[derive(Debug)]
enum CursorOp {
    Idle,
    /// A read is in flight. The buffer is returned by the worker rather than
    /// borrowed from the caller, because the task cannot be cancelled.
    Reading(tokio::task::JoinHandle<(Result<usize>, BytesMut)>),
    /// A write is in flight, carrying the byte count, resulting position, and
    /// the buffer to recycle.
    Writing(tokio::task::JoinHandle<(Result<(usize, u64)>, BytesMut)>),
    /// An end-relative seek is resolving the file's current length.
    Sizing(tokio::task::JoinHandle<Result<u64>>, i64),
}

/// Applies `delta` to `base`, rejecting a negative result as `lseek` does.
fn offset_delta(base: u64, delta: i64) -> io::Result<u64> {
    let result = if delta >= 0 {
        base.checked_add(delta as u64)
    } else {
        base.checked_sub(delta.unsigned_abs())
    };
    result.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "seek would move the cursor outside the representable range",
        )
    })
}

/// Fills `buf`'s spare capacity from `offset` and hands the buffer back.
fn read_blocking(
    file: &std::fs::File,
    mut buf: BytesMut,
    offset: u64,
    seekable: bool,
) -> (Result<usize>, BytesMut) {
    let spare = buf.spare_capacity_mut();
    if spare.is_empty() {
        return (Ok(0), buf);
    }
    // SAFETY: the memory may be uninitialized, but the read only writes into
    // the slice and reports how much it wrote; nothing reads it beforehand.
    // This mirrors what `tokio::fs` does for the same reason.
    let dst = unsafe { &mut *(spare as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]) };
    let result = if seekable {
        File::pread(file, dst, offset)
    } else {
        // A stream has no offset to name; the kernel's own position is the
        // only one it has.
        use std::io::Read as _;
        match (&*file).read(dst) {
            Ok(read) => Ok(read),
            Err(error) => Err(error.into()),
        }
    };
    // The buffer goes back to the caller either way: on the error path there
    // is nothing wrong with the allocation, only with the read.
    let Ok(read) = result else {
        return (result, buf);
    };
    let filled = buf.len() + read;
    // SAFETY: `pread` initialized exactly `read` bytes of the spare capacity.
    unsafe { buf.set_len(filled) };
    (Ok(read), buf)
}

/// Writes `data` and reports the byte count along with the position after it.
///
/// An append-mode handle ignores `offset` entirely — the kernel places the
/// bytes at the end atomically — so the resulting position has to be read back
/// rather than computed.
fn write_blocking(
    file: &std::fs::File,
    data: &[u8],
    offset: u64,
    append: bool,
    seekable: bool,
) -> Result<(usize, u64)> {
    if append || !seekable {
        use std::io::Write as _;
        let written = (&*file).write(data)?;
        // An append lands at the end wherever that is, and a stream has no
        // position to report, so ask rather than compute — and settle for the
        // byte count when the file cannot answer.
        let end = {
            use std::io::Seek as _;
            (&*file)
                .stream_position()
                .unwrap_or(offset + written as u64)
        };
        Ok((written, end))
    } else {
        let written = File::pwrite(file, data, offset)?;
        Ok((written, offset + written as u64))
    }
}

/// Writes from an owned buffer and returns it, so the streaming path can
/// recycle one allocation instead of making a fresh one per call.
fn write_blocking_owned(
    file: &std::fs::File,
    buf: BytesMut,
    offset: u64,
    append: bool,
    seekable: bool,
) -> (Result<(usize, u64)>, BytesMut) {
    let result = write_blocking(file, &buf, offset, append, seekable);
    (result, buf)
}

impl File {
    pub(crate) fn from_std(file: std::fs::File, read: bool, write: bool, append: bool) -> Self {
        // One `fstat` on an already-open descriptor, to know up front whether
        // offsets mean anything for this file.
        let seekable = file.metadata().map(|meta| meta.is_file()).unwrap_or(false);
        let mut flags = FileFlags::empty();
        flags.set(FileFlags::READ, read);
        flags.set(FileFlags::WRITE, write);
        flags.set(FileFlags::APPEND, append);
        flags.set(FileFlags::SEEKABLE, seekable);
        Self {
            file: Arc::new(file),
            locks: FileLocks::new(),
            flags,
            cursor: OnceLock::new(),
        }
    }

    fn cursor_state(&mut self) -> &mut CursorState {
        if self.cursor.get().is_none() {
            self.cursor
                .set(Box::new(CursorState::default()))
                .expect("cursor state was checked above");
        }
        self.cursor.get_mut().expect("cursor state was initialized")
    }

    fn cursor_offset(&self) -> u64 {
        self.cursor.get().map_or(0, |state| state.cursor)
    }

    /// Writes `offset` into the kernel's file offset.
    ///
    /// Required before a descriptor reaches another process or another API:
    /// they read from the *kernel's* offset, which positional I/O has been
    /// bypassing.
    fn materialize(&self, offset: u64) -> io::Result<()> {
        materialize(&self.file, self.flags.contains(FileFlags::SEEKABLE), offset)
    }

    /// Surrenders the descriptor to hand it to another process.
    ///
    /// The handle is consumed rather than duplicated: on Unix the child can
    /// simply inherit *this* descriptor, so a `dup` would buy a second one only
    /// to close the first behind it. Taking it outright is also what makes the
    /// steal real — nothing on this side can still be reading or writing
    /// through the description the child is about to share.
    async fn into_stdio_file(
        self,
        send: bool,
        offset: u64,
    ) -> std::result::Result<std::fs::File, HandoffError<Self>> {
        let File {
            file,
            locks,
            flags,
            cursor,
        } = self;
        // Takes the locks as a parameter rather than capturing them, because
        // they have to be released between the two failure points below.
        let restore = move |file: Arc<std::fs::File>, locks, error: io::Error| {
            HandoffError::new(
                File {
                    file,
                    locks,
                    flags,
                    cursor,
                },
                error,
            )
        };
        let owned = match Arc::try_unwrap(file) {
            Ok(file) => file,
            // Operations are still in flight against the shared descriptor.
            // Nothing has been given away — the locks are still held and the
            // descriptor is untouched — so the caller gets the handle back and
            // may retry once they finish.
            Err(file) => {
                return Err(restore(
                    file,
                    locks,
                    io::Error::new(io::ErrorKind::ResourceBusy, "file is in use"),
                ));
            }
        };
        // Only now that the descriptor is exclusively ours. The locks this side
        // holds have to come off in band for the same reason as in `close`: the
        // description is about to outlive this handle in another process, so
        // closing our end would not lift them.
        if let Err(error) = locks.release_all().await {
            // Some locks may already be off, so the handle comes back degraded.
            // Still better than dropping it along with the error, since the
            // caller at least gets to close it.
            return Err(restore(Arc::new(owned), locks, error));
        }
        #[cfg(unix)]
        let result = {
            let _ = send;
            // The child reads from the *kernel's* offset, which positional I/O
            // has been bypassing.
            match materialize(&owned, flags.contains(FileFlags::SEEKABLE), offset) {
                Ok(()) => Ok(owned),
                Err(error) => Err(restore(Arc::new(owned), locks, error)),
            }
        };
        #[cfg(windows)]
        let result = match reopen_for_stdio(&owned, flags, send, offset) {
            // `ReOpenFile` cannot inherit the original handle's access, so
            // unlike the Unix path this really is a second description; the
            // original still has to go, and taking it above is what guarantees
            // it does.
            Ok(file) => {
                drop(owned);
                Ok(file)
            }
            Err(error) => Err(restore(Arc::new(owned), locks, error)),
        };
        result
    }
}

/// Writes `offset` into the kernel's file offset of `file`.
fn materialize(file: &std::fs::File, seekable: bool, offset: u64) -> io::Result<()> {
    if !seekable {
        // Nothing to reconcile: a stream's position was never ours to track,
        // and seeking it would fail.
        return Ok(());
    }
    use std::io::Seek as _;
    { file }.seek(io::SeekFrom::Start(offset))?;
    Ok(())
}

#[cfg(windows)]
fn reopen_for_stdio(
    file: &std::fs::File,
    flags: FileFlags,
    send: bool,
    offset: u64,
) -> io::Result<std::fs::File> {
    {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                FILE_WRITE_DATA, ReOpenFile,
            },
        };

        let access = if send {
            if flags.contains(FileFlags::APPEND) {
                FILE_GENERIC_WRITE & !FILE_WRITE_DATA
            } else if flags.contains(FileFlags::WRITE) {
                GENERIC_WRITE
            } else {
                0
            }
        } else if flags.contains(FileFlags::READ) {
            GENERIC_READ
        } else {
            0
        };
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let reopened = unsafe { std::fs::File::from_raw_handle(handle) };
        // `ReOpenFile` yields an independent file description whose pointer
        // starts at zero, so unlike the inherited descriptor on Unix it has to
        // be positioned explicitly to match the tracked cursor.
        materialize(&reopened, flags.contains(FileFlags::SEEKABLE), offset)?;
        Ok(reopened)
    }
}

impl File {
    /// Drives an in-flight cursor operation to completion.
    fn poll_settle(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let state = self.cursor_state();
        match &mut state.op {
            CursorOp::Idle => Poll::Ready(Ok(())),
            CursorOp::Reading(handle) => {
                let (result, buf) = ready!(Pin::new(handle).poll(cx)).unwrap_or_else(|_| {
                    (
                        Err(Error::other("file read worker failed")),
                        BytesMut::new(),
                    )
                });
                // Settle the state machine before reporting the failure: the
                // join handle has already completed, so leaving it in place
                // would panic the next poll rather than retry the read.
                state.op = CursorOp::Idle;
                state.pending_read = buf;
                state.pending_pos = 0;
                result?;
                Poll::Ready(Ok(()))
            }
            CursorOp::Writing(handle) => {
                let (result, buf) = ready!(Pin::new(handle).poll(cx)).unwrap_or_else(|_| {
                    (
                        Err(Error::other("file write worker failed")),
                        BytesMut::new(),
                    )
                });
                state.op = CursorOp::Idle;
                state.write_scratch = buf;
                let (_, end) = result?;
                state.cursor = end;
                Poll::Ready(Ok(()))
            }
            CursorOp::Sizing(handle, delta) => {
                let delta = *delta;
                let len = ready!(Pin::new(handle).poll(cx))
                    .unwrap_or_else(|_| Err(Error::other("file size worker failed")))?;
                state.op = CursorOp::Idle;
                let position = offset_delta(len, delta)?;
                state.cursor = position;
                state.seek_to = Some(position);
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let file = Arc::clone(&this.file);
        let seekable = this.flags.contains(FileFlags::SEEKABLE);
        loop {
            let state = this.cursor_state();
            let undelivered = state.pending_read.len() - state.pending_pos;
            if undelivered > 0 {
                let take = undelivered.min(buf.remaining());
                let from = state.pending_pos;
                buf.put_slice(&state.pending_read[from..from + take]);
                state.pending_pos += take;
                state.cursor += take as u64;
                return Poll::Ready(Ok(()));
            }
            match &state.op {
                CursorOp::Reading(_) => {
                    ready!(this.poll_settle(cx))?;
                    // An empty buffer back from the worker means end of file.
                    if this.cursor_state().pending_read.is_empty() {
                        return Poll::Ready(Ok(()));
                    }
                }
                CursorOp::Idle => {
                    if buf.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    let want = buf.remaining().min(MAX_BLOCKING_IO);
                    let state = this.cursor_state();
                    let offset = state.cursor;
                    let mut scratch = std::mem::take(&mut state.pending_read);
                    scratch.clear();
                    state.pending_pos = 0;
                    scratch.reserve(want);
                    let file = Arc::clone(&file);
                    state.op = CursorOp::Reading(tokio::task::spawn_blocking(move || {
                        read_blocking(&file, scratch, offset, seekable)
                    }));
                }
                _ => {
                    return Poll::Ready(Err(io::Error::other(
                        "file read polled while another operation is in progress",
                    )));
                }
            }
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let file = Arc::clone(&this.file);
        let append = this.flags.contains(FileFlags::APPEND);
        let seekable = this.flags.contains(FileFlags::SEEKABLE);
        loop {
            let state = this.cursor_state();
            match &mut state.op {
                CursorOp::Writing(handle) => {
                    let (result, scratch) =
                        ready!(Pin::new(handle).poll(cx)).unwrap_or_else(|_| {
                            (
                                Err(Error::other("file write worker failed")),
                                BytesMut::new(),
                            )
                        });
                    state.op = CursorOp::Idle;
                    state.write_scratch = scratch;
                    let (written, end) = result?;
                    state.cursor = end;
                    return Poll::Ready(Ok(written));
                }
                CursorOp::Idle => {
                    if buf.is_empty() {
                        return Poll::Ready(Ok(0));
                    }
                    // A write invalidates whatever was read ahead of it.
                    state.pending_read.clear();
                    state.pending_pos = 0;
                    let take = buf.len().min(MAX_BLOCKING_IO);
                    // The bytes are copied because the worker cannot be
                    // cancelled and so must not borrow the caller's buffer.
                    // The destination is recycled across writes.
                    let mut scratch = std::mem::take(&mut state.write_scratch);
                    scratch.clear();
                    scratch.extend_from_slice(&buf[..take]);
                    let offset = state.cursor;
                    let file = Arc::clone(&file);
                    state.op = CursorOp::Writing(tokio::task::spawn_blocking(move || {
                        write_blocking_owned(&file, scratch, offset, append, seekable)
                    }));
                }
                _ => {
                    return Poll::Ready(Err(io::Error::other(
                        "file write polled while another operation is in progress",
                    )));
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Each write reaches the file within its own blocking call, so there
        // is nothing buffered to push; flushing only has to wait for work
        // already handed to a worker.
        self.get_mut().poll_settle(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_settle(cx)
    }
}

impl AsyncSeek for File {
    fn start_seek(self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        let file = Arc::clone(&this.file);
        let state = this.cursor_state();
        if !matches!(state.op, CursorOp::Idle) {
            return Err(io::Error::other(
                "file seek started while another operation is in progress",
            ));
        }
        state.pending_read.clear();
        state.pending_pos = 0;
        match position {
            // Both of these resolve locally: no syscall, and for the remote
            // backend no round trip either.
            io::SeekFrom::Start(offset) => {
                state.cursor = offset;
                state.seek_to = Some(offset);
            }
            io::SeekFrom::Current(delta) => {
                let offset = offset_delta(state.cursor, delta)?;
                state.cursor = offset;
                state.seek_to = Some(offset);
            }
            // Only an end-relative seek needs the file's current length, and
            // that answer is stale the moment it is produced — as it is for
            // `lseek(SEEK_END)` too.
            io::SeekFrom::End(delta) => {
                state.op = CursorOp::Sizing(
                    tokio::task::spawn_blocking(move || -> Result<u64> {
                        Ok(file.metadata()?.len())
                    }),
                    delta,
                );
            }
        }
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        if matches!(this.cursor_state().op, CursorOp::Sizing(..)) {
            ready!(this.poll_settle(cx))?;
        }
        let state = this.cursor_state();
        let position = state.seek_to.take().unwrap_or(state.cursor);
        Poll::Ready(Ok(position))
    }
}

impl File {
    pub(crate) fn read_at<'b>(
        &self,
        buf: &'b mut BytesMut,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        let file = Arc::clone(&self.file);
        let seekable = self.flags.contains(FileFlags::SEEKABLE);
        async move {
            let taken = mem::take(buf);
            let (result, taken) = match tokio::task::spawn_blocking(move || {
                read_blocking(&file, taken, offset, seekable)
            })
            .await
            {
                Ok(outcome) => outcome,
                // The worker owned the buffer, so a panic there loses it;
                // the caller is left with the same empty buffer that
                // cancelling would have left.
                Err(_) => (
                    Err(Error::other("file read worker failed")),
                    BytesMut::new(),
                ),
            };
            *buf = taken;
            result
        }
    }

    pub(crate) fn read_at_into<'b>(
        &self,
        buf: &'b mut [std::mem::MaybeUninit<u8>],
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        let file = Arc::clone(&self.file);
        let seekable = self.flags.contains(FileFlags::SEEKABLE);
        let want = buf.len().min(MAX_BLOCKING_IO);
        async move {
            let owned = BytesMut::with_capacity(want);
            let (result, owned) = match tokio::task::spawn_blocking(move || {
                read_blocking(&file, owned, offset, seekable)
            })
            .await
            {
                Ok(outcome) => outcome,
                Err(_) => (
                    Err(Error::other("file read worker failed")),
                    BytesMut::new(),
                ),
            };
            let read = result?;
            buf[..read].write_copy_of_slice(&owned[..read]);
            Ok(read)
        }
    }

    pub(crate) fn write_at(
        &self,
        data: Bytes,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<> {
        let file = Arc::clone(&self.file);
        let append = self.flags.contains(FileFlags::APPEND);
        async move {
            if append {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "cannot write at an offset on a file opened for append",
                ));
            }
            tokio::task::spawn_blocking(move || File::pwrite(&file, &data, offset))
                .await
                .unwrap_or_else(|_| Err(Error::other("file write worker failed")))
        }
    }

    pub(crate) fn write_at_from<'b>(
        &self,
        data: &'b [u8],
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        // Eagerly, outside any async block: the copy is what this default
        // exists to perform, and `write_at`'s future does not borrow the
        // handle, which is the property the signature promises onward.
        self.write_at(Bytes::copy_from_slice(data), offset)
    }

    /// Appends `data` atomically, returning the byte count and the position
    /// just past what was written.
    pub fn append(&self, data: Bytes) -> impl Future<Output = Result<(usize, u64)>> + Send + use<> {
        let file = Arc::clone(&self.file);
        let seekable = self.flags.contains(FileFlags::SEEKABLE);
        async move {
            tokio::task::spawn_blocking(move || write_blocking(&file, &data, 0, true, seekable))
                .await
                .unwrap_or_else(|_| Err(Error::other("file write worker failed")))
        }
    }

    pub(crate) async fn into_stdio_send(
        self,
        offset: u64,
    ) -> std::result::Result<StdioSend, HandoffError<Self>> {
        let file = self.into_stdio_file(true, offset).await?;
        Ok(StdioSend::from_file(TokioFile::from_std(file)))
    }

    pub(crate) async fn into_stdio_recv(
        self,
        offset: u64,
    ) -> std::result::Result<StdioRecv, HandoffError<Self>> {
        let file = self.into_stdio_file(false, offset).await?;
        Ok(StdioRecv::from_file(TokioFile::from_std(file)))
    }

    pub(crate) async fn close(self) -> Result<()> {
        // Unlock in band rather than leaving it to the handles being closed:
        // duplicates of this open file description may still be alive
        // elsewhere, which would keep the locks in force past this point.
        // Now that outstanding operations hold their own reference to the
        // file, this is the *only* reliable release — the descriptor itself
        // may outlive this call.
        let released = self.locks.release_all().await.map_err(Error::from);
        let File { file, .. } = self;
        // Match what the remote backend reports for the same situation: the
        // handle is consumed and cleanup finishes as the outstanding
        // operations drop their references, but the caller is told the
        // resource was busy rather than being left to assume the descriptor
        // is already gone.
        let busy = Arc::strong_count(&file) > 1;
        match Arc::try_unwrap(file) {
            Ok(file) => {
                let _ = tokio::task::spawn_blocking(move || drop(file)).await;
            }
            Err(shared) => drop(shared),
        }
        released?;
        if busy {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "file is in use").into());
        }
        Ok(())
    }

    pub(crate) async fn set_size(&self, size: u64) -> Result<()> {
        let file = Arc::clone(&self.file);
        Ok(tokio::task::spawn_blocking(move || file.set_len(size))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join file resize task")))?)
    }

    pub(crate) async fn metadata(&self) -> Result<Metadata> {
        let file = Arc::clone(&self.file);
        #[cfg(unix)]
        {
            tokio::task::spawn_blocking(move || {
                let metadata = file.metadata()?;
                Direct::metadata_with_attrs(metadata, &file)
            })
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
        #[cfg(windows)]
        {
            tokio::task::spawn_blocking(move || {
                let metadata = file.metadata()?;
                Direct::metadata_with_security(metadata, &file)
            })
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
    }

    pub(crate) async fn fs_metadata(&self) -> Result<FsMetadata> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || Direct::fs_metadata_from_file(&file))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join fs metadata query task")))
    }

    pub(crate) async fn acl(&self, kind: AclKind, default: bool) -> Result<Option<Acl>> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || Direct::acl_from_file(&file, kind, default))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join ACL query task")))
    }

    pub(crate) async fn set_acl(
        &self,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
    ) -> Result<()> {
        let file = Arc::clone(&self.file);
        let acl = acl.cloned();
        tokio::task::spawn_blocking(move || {
            Direct::set_acl_file(&file, kind, acl.as_ref(), default)
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("failed to join ACL update task")))
    }

    pub(crate) async fn sec_desc(
        &self,
        mask: dolang_winterop::security::SecInfo,
    ) -> Result<SecDesc> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || Direct::sec_desc_from_file(&file, mask))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn set_sec_desc(&self, sec_desc: &SecDesc) -> Result<()> {
        let file = Arc::clone(&self.file);
        let sec_desc = sec_desc.clone();
        tokio::task::spawn_blocking(move || Direct::set_sec_desc_file(&file, &sec_desc))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn xattrs(&self, namespace: XattrNamespace<'_>) -> Result<Vec<XattrEntry>> {
        Direct::impl_file_xattrs(&self.file, namespace).await
    }

    pub(crate) async fn xattr(&self, name: &str, namespace: Option<&str>) -> Result<Vec<u8>> {
        Direct::impl_file_xattr(&self.file, name, namespace).await
    }

    pub(crate) async fn streams(&self) -> Result<Vec<StreamEntry>> {
        Direct::impl_file_streams(&self.file).await
    }

    pub(crate) async fn set_xattr(
        &self,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<()> {
        Direct::impl_file_set_xattr(&self.file, name, namespace, value).await
    }

    pub(crate) async fn remove_xattr(&self, name: &str, namespace: Option<&str>) -> Result<()> {
        Direct::impl_file_remove_xattr(&self.file, name, namespace).await
    }

    pub(crate) async fn lock(&self, request: FileLockRequest) -> Result<Option<file::FileLock>> {
        // A duplicate would be a second descriptor on the same open file
        // description, which is what the lock is keyed on anyway; sharing the
        // original avoids the `dup` and, on Windows, keeps `LockFileEx`
        // operating on the very handle it will later be released through.
        #[cfg(unix)]
        let handle = self.file.as_fd().try_clone_to_owned()?;
        #[cfg(windows)]
        let handle = self.file.as_handle().try_clone_to_owned()?;
        Ok(self
            .locks
            .acquire(handle, request)
            .await
            .map(|lock| lock.map(file::FileLock::direct))?)
    }

    pub(crate) async fn try_into_std(self) -> std::result::Result<std::fs::File, Self> {
        // Surrendering the descriptor means surrendering the cursor with it,
        // so hand over one positioned where this handle believes it is.
        if self.materialize(self.cursor_offset()).is_err() {
            return Err(self);
        }
        let File {
            file,
            locks,
            flags,
            cursor,
        } = self;
        match Arc::try_unwrap(file) {
            Ok(file) => Ok(file),
            // Operations are still in flight against the shared descriptor,
            // so it cannot be given away exclusively.
            Err(file) => Err(File {
                file,
                locks,
                flags,
                cursor,
            }),
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    program: PathBuf,
    path: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct PathCache {
    map: Mutex<HashMap<CacheKey, PathBuf>>,
}

impl PathCache {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    async fn resolve(
        &self,
        program: &Path,
        path: Option<&str>,
        cwd: Option<&Path>,
    ) -> Option<PathBuf> {
        let key = CacheKey {
            program: program.to_path_buf(),
            path: path.map(|p| p.to_string()),
            cwd: cwd.map(|p| p.to_path_buf()),
        };

        let cached = {
            let map = self.map.lock().await;
            map.get(&key).cloned()
        };

        if let Some(cached) = cached {
            return Some(cached);
        }

        let path_env = path
            .map(|p| p.into())
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_else(|| "".into());

        let program = program.to_path_buf();
        let cwd = cwd.map(|p| p.to_path_buf());

        let resolved = tokio::task::spawn_blocking(move || {
            which::which_in(
                &program,
                Some(path_env),
                cwd.as_deref().unwrap_or(Path::new("")),
            )
            .ok()
        })
        .await
        .unwrap_or(None);

        if let Some(ref resolved_path) = resolved {
            let mut map = self.map.lock().await;
            map.insert(key, resolved_path.clone());
        }

        resolved
    }

    async fn clear(&self) {
        self.map.lock().await.clear();
    }
}

impl Direct {
    /// Captures the process context used by this direct backend.
    pub fn new() -> Result<Self> {
        Ok(Self {
            path_cache: Arc::new(PathCache::new()),
            initial: Arc::new(Query::current()?),
        })
    }
}

impl<'a> Command<'a> {
    fn new(direct: &'a Direct, program: Utf8TypedPath<'_>) -> Self {
        let program = native_path(program);
        Self {
            direct,
            program: program.as_ref().cloned().unwrap_or_default(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
            stdin_resource: None,
            stdout_resource: None,
            stderr_resource: None,
            stderr_to_stdout: false,
            process_control: ProcessControl::Foreground,
            termination_policy: TerminationPolicy::default(),
            error: program.err(),
        }
    }
}

impl Child {
    fn new(
        child: tokio::process::Child,
        process_control: ProcessControl,
        termination_policy: TerminationPolicy,
        #[cfg(windows)] job: Option<std::os::windows::io::OwnedHandle>,
    ) -> Self {
        Self {
            inner: child,
            process_control,
            termination_policy,
            #[cfg(windows)]
            job,
        }
    }
}

impl Child {
    pub(crate) async fn wait(&mut self) -> Result<ProcessStatus> {
        Ok(ProcessStatus::from_native(self.inner.wait().await?)?)
    }

    pub(crate) async fn terminate(self) -> Result<Option<ProcessStatus>> {
        Ok(self
            .impl_terminate()
            .await?
            .map(ProcessStatus::from_native)
            .transpose()?)
    }
}

impl Command<'_> {
    pub(crate) fn arg(&mut self, arg: &str) -> &mut Self {
        self.args.push(arg.to_owned());
        self
    }

    pub(crate) fn env(&mut self, key: &str, val: &str) -> &mut Self {
        self.env.insert(key.to_owned(), Some(val.to_owned()));
        self
    }

    pub(crate) fn env_remove(&mut self, key: &str) -> &mut Self {
        self.env.insert(key.to_owned(), None);
        self
    }

    pub(crate) fn current_dir(&mut self, dir: Utf8TypedPath<'_>) -> &mut Self {
        match native_path(dir) {
            Ok(dir) => self.cwd = Some(dir),
            Err(error) => self.error = Some(error),
        }
        self
    }

    pub(crate) fn stdin(&mut self, stdio: StdioRecv) -> Result<&mut Self> {
        self.stdin = None;
        self.stdin_resource = Some(stdio);
        Ok(self)
    }

    pub(crate) fn stdout(&mut self, stdio: StdioSend) -> Result<&mut Self> {
        self.stdout = None;
        self.stdout_resource = Some(stdio);
        Ok(self)
    }

    pub(crate) fn stdin_inherit(&mut self) -> Result<&mut Self> {
        self.stdin_resource = None;
        self.stdin = Some(Stdio::inherit());
        Ok(self)
    }

    pub(crate) fn stdout_inherit(&mut self) -> Result<&mut Self> {
        self.stdout_resource = None;
        self.stdout = Some(Stdio::inherit());
        Ok(self)
    }

    pub(crate) fn stdout_inherit_stderr(&mut self) -> Result<&mut Self> {
        self.stdout_resource = None;
        self.impl_stdout_inherit_stderr()
    }

    pub(crate) fn stdin_null(&mut self) -> &mut Self {
        self.stdin = None;
        self.stdin_resource = None;
        self
    }

    pub(crate) fn stdout_null(&mut self) -> &mut Self {
        self.stdout = None;
        self.stdout_resource = None;
        self
    }

    pub(crate) fn stderr(&mut self, stdio: StdioSend) -> Result<&mut Self> {
        self.stderr = None;
        self.stderr_resource = Some(stdio);
        self.stderr_to_stdout = false;
        Ok(self)
    }

    pub(crate) fn stderr_inherit(&mut self) -> Result<&mut Self> {
        self.stderr_resource = None;
        self.stderr = Some(Stdio::inherit());
        self.stderr_to_stdout = false;
        Ok(self)
    }

    pub(crate) fn stderr_to_stdout(&mut self) -> Result<&mut Self> {
        self.stderr = None;
        self.stderr_resource = None;
        self.stderr_to_stdout = true;
        Ok(self)
    }

    pub(crate) fn stderr_inherit_stdout(&mut self) -> Result<&mut Self> {
        self.stderr_resource = None;
        self.stderr_to_stdout = false;
        self.impl_stderr_inherit_stdout()
    }

    pub(crate) fn stderr_null(&mut self) -> &mut Self {
        self.stderr = None;
        self.stderr_resource = None;
        self.stderr_to_stdout = false;
        self
    }

    pub(crate) fn process_control(&mut self, control: ProcessControl) -> &mut Self {
        self.process_control = control;
        self
    }

    pub(crate) fn termination_policy(&mut self, policy: TerminationPolicy) -> &mut Self {
        self.termination_policy = policy;
        self
    }

    pub(crate) async fn spawn(mut self) -> Result<Child> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let path_override = self
            .env
            .get("PATH")
            .map(|path| path.as_deref().unwrap_or(""));
        let resolved = self
            .direct
            .path_cache
            .resolve(&self.program, path_override, self.cwd.as_deref())
            .await;
        let resolved = resolved.ok_or_else(Direct::program_not_found_error)?;

        if let Some(stdin) = self.stdin_resource.take() {
            self.stdin = Some(stdin.into_stdio().await?);
        }
        if self.stderr_to_stdout {
            if let Some(stdout) = self.stdout_resource.take() {
                let stderr = stdout.try_clone().await?;
                self.stdout = Some(stdout.into_stdio().await?);
                self.stderr = Some(stderr.into_stdio().await?);
            } else if self.stdout.is_some() {
                self.impl_stderr_inherit_stdout()?;
            }
        } else if let Some(stderr) = self.stderr_resource.take() {
            self.stderr = Some(stderr.into_stdio().await?);
        }
        if let Some(stdout) = self.stdout_resource.take() {
            self.stdout = Some(stdout.into_stdio().await?);
        }

        let mut command = TokioCommand::new(&resolved);
        command.args(&self.args);

        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }

        for (k, v) in &self.env {
            match v {
                Some(val) => {
                    command.env(k, val);
                }
                None => {
                    command.env_remove(k);
                }
            }
        }

        if let Some(stdin) = self.stdin.take() {
            command.stdin(stdin);
        } else {
            command.stdin(Stdio::null());
        }
        if let Some(stdout) = self.stdout.take() {
            command.stdout(stdout);
        } else {
            command.stdout(Stdio::null());
        }
        if let Some(stderr) = self.stderr.take() {
            command.stderr(stderr);
        } else {
            command.stderr(Stdio::null());
        }

        self.configure_process(&mut command)?;
        let child = command.spawn()?;
        self.finish_spawn(child)
    }
}

impl OpenOptions {
    fn as_tokio(&self) -> fs::OpenOptions {
        let mut opts = fs::OpenOptions::new();
        opts.read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
        self.apply_no_follow_flags(&mut opts);
        opts
    }
}

impl OpenOptions {
    pub(crate) fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub(crate) fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub(crate) fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub(crate) fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub(crate) fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    pub(crate) fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub(crate) fn no_follow(&mut self, no_follow: bool) -> &mut Self {
        self.no_follow = no_follow;
        self
    }

    pub(crate) async fn open(&self, path: Utf8TypedPath<'_>) -> Result<File> {
        // `as_tokio` carries the platform-specific open flags, so go through
        // it and then unwrap: the handle is driven positionally from here on
        // and has no use for tokio's cursor bookkeeping.
        let file = self
            .as_tokio()
            .open(native_path(path)?)
            .await?
            .into_std()
            .await;
        Ok(File::from_std(file, self.read, self.write, self.append))
    }
}

impl Direct {
    /// Calls a registered VFS extension in-process, with no RPC session or
    /// serialization involved.
    pub async fn call_extension<T: VfsExtension>(
        &self,
        request: T::Request,
    ) -> Result<T::Response> {
        let ext = extension::lookup(T::NAME, T::VERSION)
            .filter(|extension| extension.available())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Unsupported,
                    format!("VFS extension {} v{} is not available", T::NAME, T::VERSION),
                )
            })?;
        let mut state = DirectContext::default();
        let mut ctx = ExtContext::direct(&mut state);
        let response = ext.dispatch(&mut ctx, Box::new(request)).await;
        Ok(*response
            .downcast::<T::Response>()
            .expect("response type matches the extension that produced it"))
    }

    async fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
        Self::impl_copy_symlink(src, dst).await
    }

    async fn copy_local(from: &Path, to: &Path, all: bool) -> Result<()> {
        let metadata = fs::symlink_metadata(from).await?;

        if metadata.is_dir() {
            if !all {
                return Err(Self::directory_requires_all_error());
            }

            fs::create_dir(to).await?;
            let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
            while let Some((src_dir, dst_dir)) = stack.pop() {
                let mut entries = fs::read_dir(&src_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let src_path = entry.path();
                    let dst_path = dst_dir.join(entry.file_name());
                    let metadata = fs::symlink_metadata(&src_path).await?;
                    if metadata.is_dir() {
                        fs::create_dir(&dst_path).await?;
                        stack.push((src_path, dst_path));
                    } else if metadata.is_file() {
                        fs::copy(&src_path, &dst_path).await?;
                    } else if metadata.file_type().is_symlink() {
                        Self::copy_symlink(&src_path, &dst_path).await?;
                    } else {
                        return Err(Error::other("unsupported file type"));
                    }
                }
            }
        } else if metadata.is_file() {
            fs::copy(from, to).await?;
        } else if metadata.file_type().is_symlink() {
            Self::copy_symlink(from, to).await?;
        } else {
            return Err(Error::other("unsupported file type"));
        }

        Ok(())
    }

    async fn move_local(from: &Path, to: &Path, all: bool) -> Result<()> {
        let metadata = fs::symlink_metadata(from).await?;
        let is_dir = metadata.is_dir();

        if is_dir && !all {
            return Err(Self::directory_requires_all_error());
        }

        match fs::rename(from, to).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
                Self::copy_local(from, to, all).await?;
                if is_dir {
                    Ok(fs::remove_dir_all(from).await?)
                } else {
                    Ok(fs::remove_file(from).await?)
                }
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn read_dir_paths(path: &Path) -> Result<Vec<PathBuf>> {
        let mut read_dir = fs::read_dir(path).await?;
        let mut paths = Vec::new();
        while let Some(entry) = read_dir.next_entry().await? {
            paths.push(entry.path());
        }
        Ok(paths)
    }

    async fn remove_dir_empty_tree_local(path: &Path, ignore: bool) -> Result<bool> {
        let metadata = fs::symlink_metadata(path).await?;
        if !metadata.is_dir() {
            return Err(Self::not_a_directory_error());
        }

        struct Frame {
            path: PathBuf,
            entries: Vec<PathBuf>,
            next: usize,
            removable: bool,
        }

        let mut stack = vec![Frame {
            path: path.to_owned(),
            entries: Self::read_dir_paths(path).await?,
            next: 0,
            removable: true,
        }];
        let mut last_result = None;

        while let Some(frame) = stack.last_mut() {
            if let Some(child_removed) = last_result.take() {
                frame.removable &= child_removed;
            }

            if frame.next == frame.entries.len() {
                let removable = frame.removable;
                let path = frame.path.clone();
                stack.pop();
                if removable {
                    fs::remove_dir(&path).await?;
                }
                last_result = Some(removable);
                continue;
            }

            let child_path = frame.entries[frame.next].clone();
            frame.next += 1;
            let metadata = fs::symlink_metadata(&child_path).await?;
            if metadata.is_dir() {
                stack.push(Frame {
                    path: child_path.clone(),
                    entries: Self::read_dir_paths(&child_path).await?,
                    next: 0,
                    removable: true,
                });
            } else if ignore {
                frame.removable = false;
            } else {
                return Err(Self::directory_not_empty_error());
            }
        }

        Ok(last_result.unwrap_or(false))
    }
}

impl Direct {
    pub(crate) fn env(&self) -> Box<dyn Iterator<Item = (String, String)> + '_> {
        Box::new(session::current_environment())
    }

    pub(crate) fn cwd(&self) -> Utf8TypedPath<'_> {
        self.initial.cwd.to_path()
    }

    pub(crate) fn current_exe(&self) -> Utf8TypedPath<'_> {
        self.initial.current_exe.to_path()
    }

    pub(crate) fn target(&self) -> &TargetInfo {
        &self.initial.target
    }

    pub(crate) fn security(&self) -> &SecurityInfo {
        &self.initial.security
    }

    pub(crate) fn extensions(&self) -> &ExtensionSet {
        &self.initial.extensions
    }

    pub(crate) fn open_options(&self) -> OpenOptions {
        OpenOptions::default()
    }

    pub(crate) fn command(&self, program: Utf8TypedPath<'_>) -> Command<'_> {
        Command::new(self, program)
    }

    pub(crate) async fn unix_socket(
        &self,
        path: Utf8TypedPath<'_>,
        key: Option<&[u8]>,
    ) -> Result<Vfs> {
        #[cfg(unix)]
        {
            let key = key
                .map(dolang_rpc::auth::AuthKey::new)
                .transpose()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            Ok(Vfs::from_client(
                crate::client::Client::connect_with_key(native_path(path)?, key).await?,
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, key);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix VFS connections are not supported by this direct backend",
            )
            .into())
        }
    }

    pub(crate) async fn windows_admin(
        &self,
        cwd: Utf8TypedPath<'_>,
        env: HashMap<String, Option<String>>,
        elevate: bool,
    ) -> Result<Vfs> {
        #[cfg(windows)]
        {
            let cwd = native_path(cwd)?;
            let client = if elevate {
                crate::windows::launch_admin(cwd, env).await
            } else {
                crate::windows::launch_unelevated(cwd, env).await
            }?;
            Ok(Vfs::from_client(client))
        }
        #[cfg(not(windows))]
        {
            let _ = (cwd, env, elevate);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows administrator VFS is not supported by this direct backend",
            )
            .into())
        }
    }

    pub(crate) async fn pipe(&self, buf_size: Option<usize>) -> Result<(StdioSend, StdioRecv)> {
        Ok(process::pipe(buf_size)?)
    }

    pub(crate) async fn user_name(&self, uid: u32) -> Result<String> {
        #[cfg(unix)]
        return self.impl_user_name(uid).await;
        #[cfg(windows)]
        {
            let _ = uid;
            Err(io::Error::new(io::ErrorKind::Unsupported, "Unix users are not supported").into())
        }
    }

    pub(crate) async fn user_id(&self, name: &str) -> Result<u32> {
        #[cfg(unix)]
        return self.impl_user_id(name).await;
        #[cfg(windows)]
        {
            let _ = name;
            Err(io::Error::new(io::ErrorKind::Unsupported, "Unix users are not supported").into())
        }
    }

    pub(crate) async fn group_name(&self, gid: u32) -> Result<String> {
        #[cfg(unix)]
        return self.impl_group_name(gid).await;
        #[cfg(windows)]
        {
            let _ = gid;
            Err(io::Error::new(io::ErrorKind::Unsupported, "Unix groups are not supported").into())
        }
    }

    pub(crate) async fn group_id(&self, name: &str) -> Result<u32> {
        #[cfg(unix)]
        return self.impl_group_id(name).await;
        #[cfg(windows)]
        {
            let _ = name;
            Err(io::Error::new(io::ErrorKind::Unsupported, "Unix groups are not supported").into())
        }
    }

    pub(crate) async fn sid_name(&self, sid: &Sid) -> Result<SidName> {
        #[cfg(windows)]
        return self.impl_sid_name(sid).await;
        #[cfg(unix)]
        {
            let _ = sid;
            Err(io::Error::new(io::ErrorKind::Unsupported, "Windows SIDs are not supported").into())
        }
    }

    pub(crate) async fn account_name(&self, name: &str) -> Result<SidName> {
        #[cfg(windows)]
        return self.impl_account_name(name).await;
        #[cfg(unix)]
        {
            let _ = name;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows accounts are not supported",
            )
            .into())
        }
    }

    pub(crate) async fn resolve_principal_id(
        &self,
        input: PrincipalId,
        want: PrincipalIdKind,
    ) -> Result<PrincipalId> {
        #[cfg(unix)]
        return Self::impl_resolve_principal_id(input, want);
        #[cfg(windows)]
        {
            let _ = (input, want);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "principal ID resolution is not supported on this platform",
            )
            .into())
        }
    }

    pub(crate) async fn read_dir(&self, path: Utf8TypedPath<'_>) -> Result<directory::ReadDir> {
        ReadDir::open(&native_path(path)?)
            .await
            .map(directory::ReadDir::direct)
    }

    pub(crate) async fn which(
        &self,
        program: Utf8TypedPath<'_>,
        path: Option<&str>,
        cwd: Option<Utf8TypedPath<'_>>,
    ) -> Result<Option<Utf8TypedPathBuf>> {
        let program = native_path(program)?;
        let cwd = cwd.map(native_path).transpose()?;
        self.path_cache
            .resolve(&program, path, cwd.as_deref())
            .await
            .map(typed_path)
            .transpose()
    }

    pub(crate) async fn well_known_path(
        &self,
        key: WellKnownPath,
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> Result<Utf8TypedPathBuf> {
        let path = match key {
            WellKnownPath::HomeDir => Self::home_dir_platform(env),
            WellKnownPath::CacheDir => Self::cache_dir_platform(app, env),
            WellKnownPath::TempDir => Self::temp_dir_platform(env),
        }?;
        typed_path(path)
    }

    pub(crate) async fn clear_cache(&self) -> Result<()> {
        self.path_cache.clear().await;
        Ok(())
    }

    pub(crate) async fn xattrs(
        &self,
        path: Utf8TypedPath<'_>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>> {
        self.impl_xattrs(&native_path(path)?, namespace, follow)
            .await
    }

    pub(crate) async fn streams(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> Result<Vec<StreamEntry>> {
        self.impl_streams(&native_path(path)?, follow).await
    }

    pub(crate) async fn xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>> {
        self.impl_xattr(&native_path(path)?, name, namespace, follow)
            .await
    }

    pub(crate) async fn set_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<()> {
        self.impl_set_xattr(&native_path(path)?, name, namespace, value, follow)
            .await
    }

    pub(crate) async fn remove_xattr(
        &self,
        path: Utf8TypedPath<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<()> {
        self.impl_remove_xattr(&native_path(path)?, name, namespace, follow)
            .await
    }

    pub(crate) async fn remove(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> Result<()> {
        let path = native_path(path)?;
        let path = path.as_path();
        let result = if all {
            match fs::symlink_metadata(path).await {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).await,
                Ok(_) => fs::remove_file(path).await,
                Err(e) => Err(e),
            }
        } else {
            fs::remove_file(path).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if ignore && e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata> {
        #[cfg(unix)]
        {
            let path = native_path(path)?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, true))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
        #[cfg(windows)]
        {
            let path = native_path(path)?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, true))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
    }

    pub(crate) async fn fs_metadata(
        &self,
        path: Utf8TypedPath<'_>,
        follow: bool,
    ) -> Result<FsMetadata> {
        let path = native_path(path)?;
        tokio::task::spawn_blocking(move || Self::fs_metadata_from_path(&path, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join fs metadata query task")))
    }

    pub(crate) async fn acl(
        &self,
        path: Utf8TypedPath<'_>,
        kind: AclKind,
        default: bool,
        follow: bool,
    ) -> Result<Option<Acl>> {
        let path = native_path(path)?;
        tokio::task::spawn_blocking(move || Self::acl_from_path(&path, kind, default, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join ACL query task")))
    }

    pub(crate) async fn set_acl(
        &self,
        path: Utf8TypedPath<'_>,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
        follow: bool,
    ) -> Result<()> {
        let path = native_path(path)?;
        let acl = acl.cloned();
        tokio::task::spawn_blocking(move || {
            Self::set_acl_path(&path, kind, acl.as_ref(), default, follow)
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("failed to join ACL update task")))
    }

    pub(crate) async fn sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        mask: dolang_winterop::security::SecInfo,
        follow: bool,
    ) -> Result<SecDesc> {
        let path = native_path(path)?;
        tokio::task::spawn_blocking(move || Self::sec_desc_from_path(&path, mask, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn set_sec_desc(
        &self,
        path: Utf8TypedPath<'_>,
        sec_desc: &SecDesc,
        follow: bool,
    ) -> Result<()> {
        let path = native_path(path)?;
        let sec_desc = sec_desc.clone();
        tokio::task::spawn_blocking(move || Self::set_sec_desc_path(&path, &sec_desc, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn create_dir(&self, path: Utf8TypedPath<'_>, all: bool) -> Result<()> {
        let path = native_path(path)?;
        if all {
            Ok(fs::create_dir_all(path).await?)
        } else {
            Ok(fs::create_dir(path).await?)
        }
    }

    pub(crate) async fn remove_dir(
        &self,
        path: Utf8TypedPath<'_>,
        all: bool,
        ignore: bool,
    ) -> Result<()> {
        let path = native_path(path)?;
        let result = if all {
            Self::remove_dir_empty_tree_local(&path, ignore)
                .await
                .map(|_| ())
        } else {
            Ok(fs::remove_dir(path).await?)
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if ignore && e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn copy(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> Result<()> {
        Self::copy_local(&native_path(from)?, &native_path(to)?, all).await
    }

    pub(crate) async fn rename(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        replace: bool,
    ) -> Result<()> {
        Self::impl_rename(native_path(from)?, native_path(to)?, replace).await
    }

    pub(crate) async fn move_(
        &self,
        from: Utf8TypedPath<'_>,
        to: Utf8TypedPath<'_>,
        all: bool,
    ) -> Result<()> {
        Self::move_local(&native_path(from)?, &native_path(to)?, all).await
    }

    pub(crate) async fn symlink(
        &self,
        cwd: Utf8TypedPath<'_>,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<()> {
        Self::impl_symlink(&native_path(cwd)?, &native_path(src)?, &native_path(dst)?).await
    }

    pub(crate) async fn hard_link(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<()> {
        Ok(fs::hard_link(native_path(src)?, native_path(dst)?).await?)
    }

    pub(crate) async fn symlink_dir(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<()> {
        Self::impl_symlink_dir(&native_path(src)?, &native_path(dst)?).await
    }

    pub(crate) async fn symlink_file(
        &self,
        src: Utf8TypedPath<'_>,
        dst: Utf8TypedPath<'_>,
    ) -> Result<()> {
        Self::impl_symlink_file(&native_path(src)?, &native_path(dst)?).await
    }

    pub(crate) async fn symlink_metadata(&self, path: Utf8TypedPath<'_>) -> Result<Metadata> {
        #[cfg(unix)]
        {
            let path = native_path(path)?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, false))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
        #[cfg(windows)]
        {
            let path = native_path(path)?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, false))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
    }

    pub(crate) async fn set_metadata(
        &self,
        paths: &[Utf8TypedPathBuf],
        patch: MetadataPatch,
    ) -> Result<()> {
        let paths = paths
            .iter()
            .map(|path| native_path(path.to_path()))
            .collect::<Result<Vec<_>>>()?;
        self.impl_set_metadata(&paths, patch).await
    }

    pub(crate) async fn canonicalize(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf> {
        typed_path(self.impl_canonicalize(&native_path(path)?).await?)
    }

    pub(crate) async fn read_link(&self, path: Utf8TypedPath<'_>) -> Result<Utf8TypedPathBuf> {
        typed_path(fs::read_link(native_path(path)?).await?)
    }

    pub(crate) async fn access(&self, path: Utf8TypedPath<'_>, mode: AccessFlags) -> Result<()> {
        Self::impl_access(native_path(path)?, mode).await
    }

    pub(crate) async fn glob(
        &self,
        pattern: impl Into<String>,
        root: Utf8TypedPath<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<Utf8TypedPathBuf>> {
        let pattern = pattern.into();
        let root = native_path(root)?;
        tokio::task::spawn_blocking(move || {
            let (prefix, glob) = Glob::new(&pattern)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid glob pattern"))?
                .partition();
            let walk_root = root.join(&prefix);

            let mut behavior = WalkBehavior::default();
            if follow_symlinks {
                behavior.link = LinkBehavior::ReadTarget;
            }
            if let Some(depth) = max_depth {
                behavior.depth =
                    DepthBehavior::Max(DepthMax(depth.saturating_sub(prefix.components().count())));
            }

            let mut paths = Vec::new();
            let walk = match glob {
                Some(g) => g.walk_with_behavior(&walk_root, behavior),
                None => Glob::tree().walk_with_behavior(&walk_root, behavior),
            };

            for entry in walk {
                let entry = entry.map_err(io::Error::other)?;
                paths.push(prefix.join(entry.root_relative_paths().1));
            }

            paths.sort();
            paths.into_iter().map(typed_path).collect::<Result<_>>()
        })
        .await
        .unwrap_or_else(|e| Err(Error::new(ErrorKind::Other, e.to_string())))
    }
}
