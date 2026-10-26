use std::{
    collections::HashMap,
    future::Future,
    io, mem,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bytes::{Bytes, BytesMut};

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;

use tokio::{fs, process::Command as TokioCommand, sync::Mutex};

use wax::{
    Glob, Program as _,
    query::DepthVariance,
    walk::{DepthBehavior, DepthMax, Entry, LinkBehavior, WalkBehavior},
};

use crate::{
    Vfs, directory,
    error::{Error, ErrorKind, Result},
    extension::ExtensionSet,
    extension::{self, DirectContext, ExtContext, VfsExtension},
    file::{self, AccessFlags, FileId, FileLockRequest, StreamEntry},
    file::{XattrEntry, XattrNamespace},
    metadata::{FsMetadata, Metadata, MetadataPatch},
    path::{self, WellKnownPath},
    process::{
        self, PipeRecv, PipeSend, ProcessControl, ProcessStatus, SpawnIn, SpawnOut,
        TerminationPolicy,
    },
    security::{Acl, AclKind, PrincipalId, PrincipalIdKind, SecurityInfo, SidName},
    session::{self, Query},
    target::TargetInfo,
};
use dolang_winterop::security::{SecDesc, Sid};

use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos",
    windows
))]
mod copy;
// Named for the concept rather than "process", which `use crate::process` has
// already claimed at this module's scope.
mod foreign;
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

pub(crate) use foreign::{Process, Processes};
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
    stdin_resource: Option<SpawnIn>,
    stdout_resource: Option<SpawnOut>,
    stderr_resource: Option<SpawnOut>,
    stderr_to_stdout: bool,
    process_control: ProcessControl,
    termination_policy: TerminationPolicy,
    error: Option<Error>,
}

/// A process spawned by [`Direct`].
pub struct Child {
    inner: tokio::process::Child,
    /// Files whose cursors follow the child once it exits.
    files: Vec<ChildFile>,
    process_control: ProcessControl,
    termination_policy: TerminationPolicy,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

/// A local asynchronous file handle.
///
/// Clones share one open file. Byte I/O is positional: every read and write
/// names the offset it acts on, takes `&self`, and holds no state on the
/// handle, so any number of them may be in flight at once. The kernel's own
/// file offset is left unused except where a descriptor is handed to another
/// process. The cursor position is stored here, but moved by
/// [`crate::file::File`].
#[derive(Debug, Clone)]
pub struct File {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    /// The descriptor, until the file is closed or handed off. Operations
    /// clone it under the lock and run on their clone, so taking it waits for
    /// no one.
    file: std::sync::Mutex<Option<Arc<std::fs::File>>>,
    flags: FileFlags,
    locks: FileLocks,
    pos: AtomicU64,
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

/// Largest transfer handed to a single blocking worker.
///
/// This sizes a *syscall*, which is why it is not
/// [`crate::STREAM_CHUNK_SIZE`] — that constant sizes an RPC fragment, and
/// borrowing it here silently turned one large write into several, costing a
/// round trip to the blocking pool for each. Matches what `tokio::fs` uses for
/// the same job.
const MAX_BLOCKING_IO: usize = 2 * 1024 * 1024;

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

/// A cursor read handed to a blocking worker. The buffer is returned by the
/// worker rather than borrowed from the caller, because the task cannot be
/// cancelled.
pub(crate) struct ReadOp(tokio::task::JoinHandle<(Result<usize>, BytesMut)>);

impl Future for ReadOp {
    type Output = io::Result<Body>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (result, buf) = ready!(Pin::new(&mut self.0).poll(cx)).unwrap_or_else(|_| {
            (
                Err(Error::other("file read worker failed")),
                BytesMut::new(),
            )
        });
        result?;
        Poll::Ready(Ok(Body { buf, pos: 0 }))
    }
}

/// Bytes a cursor read brought back, delivered from `buf[pos..]`.
///
/// Consumption is tracked with an index rather than `Buf::advance` on
/// purpose: advancing walks the buffer's start pointer forward, and `clear`
/// does not walk it back, so a recycled buffer would lose its capacity a
/// little at a time and reallocate on every read.
pub(crate) struct Body {
    buf: BytesMut,
    pos: usize,
}

impl Body {
    pub(crate) fn into_scratch(self) -> BytesMut {
        self.buf
    }
}

impl Body {
    /// Copies the next bytes into `buf`, returning how many. Zero means the
    /// body is exhausted.
    pub(crate) fn read_uninit(&mut self, buf: &mut [mem::MaybeUninit<u8>]) -> usize {
        let take = (self.buf.len() - self.pos).min(buf.len());
        buf[..take].write_copy_of_slice(&self.buf[self.pos..self.pos + take]);
        self.pos += take;
        take
    }
}

/// A blocking write, handing back its byte count, the resulting position and
/// the buffer to recycle.
type WriteWorker = tokio::task::JoinHandle<(Result<(usize, u64)>, BytesMut)>;

/// A cursor write. The bytes are copied into `scratch` because the worker
/// cannot be cancelled and so must not borrow the caller's buffer.
pub(crate) struct WriteOp {
    file: Arc<std::fs::File>,
    offset: u64,
    append: bool,
    seekable: bool,
    scratch: BytesMut,
    worker: Option<WriteWorker>,
}

impl WriteOp {
    pub(crate) fn into_scratch(self) -> BytesMut {
        self.scratch
    }
}

impl file::WriteOp for WriteOp {
    fn poll_send(&mut self, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.scratch.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(usize, u64)>> {
        let worker = self.worker.get_or_insert_with(|| {
            let file = Arc::clone(&self.file);
            let scratch = mem::take(&mut self.scratch);
            let (offset, append, seekable) = (self.offset, self.append, self.seekable);
            tokio::task::spawn_blocking(move || {
                write_blocking_owned(&file, scratch, offset, append, seekable)
            })
        });
        let (result, scratch) = ready!(Pin::new(worker).poll(cx)).unwrap_or_else(|_| {
            (
                Err(Error::other("file write worker failed")),
                BytesMut::new(),
            )
        });
        self.worker = None;
        self.scratch = scratch;
        Poll::Ready(result.map_err(Into::into))
    }
}

/// An end-relative seek resolving the file's current length.
pub(crate) struct SizeOp(tokio::task::JoinHandle<Result<u64>>);

impl Future for SizeOp {
    type Output = io::Result<u64>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let len = ready!(Pin::new(&mut self.0).poll(cx))
            .unwrap_or_else(|_| Err(Error::other("file size worker failed")))?;
        Poll::Ready(Ok(len))
    }
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
            shared: Arc::new(Shared {
                file: std::sync::Mutex::new(Some(Arc::new(file))),
                flags,
                locks: FileLocks::new(),
                pos: AtomicU64::new(0),
            }),
        }
    }

    /// Returns the descriptor, unless the file has been closed.
    fn fd(&self) -> Result<Arc<std::fs::File>> {
        self.shared
            .file
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::Closed, "file is closed"))
    }

    /// Takes the descriptor from every clone, unless an operation still holds
    /// it.
    fn take_exclusive(&self) -> Result<std::fs::File> {
        let mut slot = self.shared.file.lock().unwrap();
        let file = slot
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Closed, "file is closed"))?;
        Arc::try_unwrap(file).map_err(|file| {
            *slot = Some(file);
            io::Error::new(io::ErrorKind::ResourceBusy, "file is in use").into()
        })
    }

    /// Puts back a descriptor taken by a surrender that failed.
    fn restore(&self, file: std::fs::File) {
        *self.shared.file.lock().unwrap() = Some(Arc::new(file));
    }

    /// The cursor position, shared by every clone.
    pub(crate) fn position(&self) -> &AtomicU64 {
        &self.shared.pos
    }

    /// Starts a cursor read of up to `len` bytes at `offset`, filling
    /// `scratch`.
    pub(crate) fn start_read(
        &self,
        offset: u64,
        len: usize,
        scratch: &mut BytesMut,
    ) -> Result<ReadOp> {
        let file = self.fd()?;
        let seekable = self.shared.flags.contains(FileFlags::SEEKABLE);
        let mut scratch = mem::take(scratch);
        scratch.clear();
        scratch.reserve(len.min(MAX_BLOCKING_IO));
        Ok(ReadOp(tokio::task::spawn_blocking(move || {
            read_blocking(&file, scratch, offset, seekable)
        })))
    }

    /// Starts a cursor write at `offset`, returning it with how many of `len`
    /// bytes it will take.
    pub(crate) fn start_write(
        &self,
        offset: u64,
        len: usize,
        scratch: &mut BytesMut,
    ) -> Result<(WriteOp, usize)> {
        let file = self.fd()?;
        let target = len.min(MAX_BLOCKING_IO);
        let mut scratch = mem::take(scratch);
        scratch.clear();
        scratch.reserve(target);
        let op = WriteOp {
            file,
            offset,
            append: self.shared.flags.contains(FileFlags::APPEND),
            seekable: self.shared.flags.contains(FileFlags::SEEKABLE),
            scratch,
            worker: None,
        };
        Ok((op, target))
    }

    /// Starts resolving the file's current length.
    pub(crate) fn start_size(&self) -> Result<SizeOp> {
        let file = self.fd()?;
        Ok(SizeOp(tokio::task::spawn_blocking(move || {
            Ok(file.metadata()?.len())
        })))
    }

    /// Opens a handle for a child process's standard stream, positioned at
    /// the cursor, along with what moves the cursor to where the child left
    /// off, unless the file has no position to follow.
    ///
    /// The file stays open here, and so do its locks.
    pub(crate) fn child_stdio(&self, send: bool) -> Result<(std::fs::File, Option<ChildFile>)> {
        let file = self.fd()?;
        let flags = self.shared.flags;
        let offset = self.shared.pos.load(Ordering::Relaxed);
        // An append-mode child writes at the end whatever the position, and a
        // stream has no position to follow.
        let tracked = flags.contains(FileFlags::SEEKABLE) && !flags.contains(FileFlags::APPEND);
        #[cfg(unix)]
        {
            let _ = send;
            // The child reads from the *kernel's* offset, which positional I/O
            // has been bypassing. Its descriptor shares the description with
            // this one, so the offset it leaves can be read back from here.
            materialize(&file, flags.contains(FileFlags::SEEKABLE), offset)?;
            let handle = file.try_clone()?;
            let sync = tracked.then(|| ChildFile { file: self.clone() });
            Ok((handle, sync))
        }
        #[cfg(windows)]
        {
            // `ReOpenFile` cannot inherit the original handle's access, so this
            // is a second description with a pointer of its own; the position
            // the child leaves is read back through a duplicate of it.
            let handle = reopen_for_stdio(&file, flags, send, offset)?;
            let sync = if tracked {
                Some(ChildFile {
                    file: self.clone(),
                    handle: handle.try_clone()?,
                })
            } else {
                None
            };
            Ok((handle, sync))
        }
    }
}

/// A file handed to a child process as a standard stream, whose cursor follows
/// the child once it has exited.
pub(crate) struct ChildFile {
    file: File,
    /// The child's own description, which on Windows has a pointer of its own.
    #[cfg(windows)]
    handle: std::fs::File,
}

impl ChildFile {
    /// Moves the cursor to where the child left the OS file position.
    ///
    /// Call only once the child has exited, so the position has settled. A
    /// file closed in the meantime is left alone.
    pub(crate) fn sync(&self) {
        #[cfg(unix)]
        let Ok(mut handle) = self.file.fd() else {
            return;
        };
        #[cfg(windows)]
        let mut handle = &self.handle;
        use std::io::Seek as _;
        // Querying the position of an open regular file does not fail in
        // practice, and the status is worth more than this.
        if let Ok(pos) = handle.stream_position() {
            self.file.shared.pos.store(pos, Ordering::Relaxed);
        }
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
    /// Whether this descriptor names a regular file suitable for extent and
    /// range-copy syscalls.
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "macos",
        windows
    ))]
    pub(crate) async fn is_regular(&self) -> Result<bool> {
        let file = self.fd()?;
        tokio::task::spawn_blocking(move || Ok(file.metadata()?.is_file()))
            .await
            .unwrap_or_else(|_| Err(Error::other("file metadata worker failed")))
    }

    /// Identifies the file behind this handle, when the platform reports one.
    ///
    /// `None` is "could not tell", never "different file": a redirector may
    /// decline to supply an index, and callers must not read a missing
    /// identity as proof that two handles are distinct.
    pub(crate) async fn id(&self) -> Option<FileId> {
        let file = self.fd().ok()?;
        tokio::task::spawn_blocking(move || Self::impl_id(&file))
            .await
            .ok()?
            .ok()
    }

    pub(crate) fn read_at<'b>(
        &self,
        buf: &'b mut BytesMut,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        let file = self.fd();
        let seekable = self.shared.flags.contains(FileFlags::SEEKABLE);
        async move {
            let file = file?;
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
        let file = self.fd();
        let seekable = self.shared.flags.contains(FileFlags::SEEKABLE);
        let want = buf.len().min(MAX_BLOCKING_IO);
        async move {
            let file = file?;
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
        let file = self.fd();
        let append = self.shared.flags.contains(FileFlags::APPEND);
        async move {
            let file = file?;
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
        let file = self.fd();
        let seekable = self.shared.flags.contains(FileFlags::SEEKABLE);
        async move {
            let file = file?;
            tokio::task::spawn_blocking(move || write_blocking(&file, &data, 0, true, seekable))
                .await
                .unwrap_or_else(|_| Err(Error::other("file write worker failed")))
        }
    }

    /// Closes the file for every clone. Closing it again does nothing.
    pub(crate) async fn close(self) -> Result<()> {
        let Some(file) = self.shared.file.lock().unwrap().take() else {
            return Ok(());
        };
        // Unlock in band rather than leaving it to the handles being closed:
        // duplicates of this open file description may still be alive
        // elsewhere, which would keep the locks in force past this point.
        // Now that outstanding operations hold their own reference to the
        // file, this is the *only* reliable release — the descriptor itself
        // may outlive this call.
        let released = self.shared.locks.release_all().await.map_err(Error::from);
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
        let file = self.fd()?;
        Ok(tokio::task::spawn_blocking(move || file.set_len(size))
            .await
            .unwrap_or_else(|_| Err(io::Error::other("failed to join file resize task")))?)
    }

    pub(crate) async fn sync(&self, data: bool) -> Result<()> {
        let file = self.fd()?;
        Ok(tokio::task::spawn_blocking(move || match data {
            true => file.sync_data(),
            false => file.sync_all(),
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("failed to join file sync task")))?)
    }

    pub(crate) async fn metadata(&self) -> Result<Metadata> {
        let file = self.fd()?;
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
        let file = self.fd()?;
        tokio::task::spawn_blocking(move || Direct::fs_metadata_from_file(&file))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join fs metadata query task")))
    }

    pub(crate) async fn acl(&self, kind: AclKind, default: bool) -> Result<Option<Acl>> {
        let file = self.fd()?;
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
        let file = self.fd()?;
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
        let file = self.fd()?;
        tokio::task::spawn_blocking(move || Direct::sec_desc_from_file(&file, mask))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn update_sec_desc(&self, sec_desc: &SecDesc) -> Result<()> {
        let file = self.fd()?;
        let sec_desc = sec_desc.clone();
        tokio::task::spawn_blocking(move || Direct::update_sec_desc_file(&file, &sec_desc))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn xattrs(&self, namespace: XattrNamespace<'_>) -> Result<Vec<XattrEntry>> {
        Direct::impl_file_xattrs(&self.fd()?, namespace).await
    }

    pub(crate) async fn xattr(&self, name: &str, namespace: Option<&str>) -> Result<Vec<u8>> {
        Direct::impl_file_xattr(&self.fd()?, name, namespace).await
    }

    pub(crate) async fn streams(&self) -> Result<Vec<StreamEntry>> {
        Direct::impl_file_streams(&self.fd()?).await
    }

    pub(crate) async fn set_xattr(
        &self,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
    ) -> Result<()> {
        Direct::impl_file_set_xattr(&self.fd()?, name, namespace, value).await
    }

    pub(crate) async fn remove_xattr(&self, name: &str, namespace: Option<&str>) -> Result<()> {
        Direct::impl_file_remove_xattr(&self.fd()?, name, namespace).await
    }

    pub(crate) async fn lock(&self, request: FileLockRequest) -> Result<Option<file::FileLock>> {
        // A duplicate would be a second descriptor on the same open file
        // description, which is what the lock is keyed on anyway; sharing the
        // original avoids the `dup` and, on Windows, keeps `LockFileEx`
        // operating on the very handle it will later be released through.
        let file = self.fd()?;
        #[cfg(unix)]
        let handle = file.as_fd().try_clone_to_owned()?;
        #[cfg(windows)]
        let handle = file.as_handle().try_clone_to_owned()?;
        Ok(self
            .shared
            .locks
            .acquire(handle, request)
            .await
            .map(|lock| lock.map(file::FileLock::direct))?)
    }

    /// Surrenders the descriptor positioned at `offset`, closing every clone.
    pub(crate) async fn try_into_std(
        self,
        offset: u64,
    ) -> std::result::Result<std::fs::File, Self> {
        // Fails while operations are still in flight against the descriptor,
        // since it cannot be given away exclusively.
        let Ok(file) = self.take_exclusive() else {
            return Err(self);
        };
        // Another API reads from the *kernel's* offset, which positional I/O
        // has been bypassing.
        if materialize(
            &file,
            self.shared.flags.contains(FileFlags::SEEKABLE),
            offset,
        )
        .is_err()
        {
            self.restore(file);
            return Err(self);
        }
        Ok(file)
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
    fn new(direct: &'a Direct, program: path::Path<'_>) -> Self {
        let program = program.to_native();
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
            files: Vec::new(),
            process_control,
            termination_policy,
            #[cfg(windows)]
            job,
        }
    }
}

impl Child {
    pub(crate) async fn wait(&mut self) -> Result<ProcessStatus> {
        let status = self.inner.wait().await?;
        self.sync_files();
        Ok(ProcessStatus::from_native(status)?)
    }

    pub(crate) async fn terminate(mut self) -> Result<Option<ProcessStatus>> {
        let files = mem::take(&mut self.files);
        let status = self.impl_terminate().await?;
        if status.is_some() {
            files.iter().for_each(ChildFile::sync);
        }
        Ok(status.map(ProcessStatus::from_native).transpose()?)
    }

    /// Moves the cursors of files handed to the child to where it left them.
    fn sync_files(&mut self) {
        for file in mem::take(&mut self.files) {
            file.sync();
        }
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

    pub(crate) fn current_dir(&mut self, dir: path::Path<'_>) -> &mut Self {
        match dir.to_native() {
            Ok(dir) => self.cwd = Some(dir),
            Err(error) => self.error = Some(error),
        }
        self
    }

    pub(crate) fn stdin(&mut self, stdio: SpawnIn) -> Result<&mut Self> {
        self.stdin = None;
        self.stdin_resource = Some(stdio);
        Ok(self)
    }

    pub(crate) fn stdout(&mut self, stdio: SpawnOut) -> Result<&mut Self> {
        self.stdout = None;
        self.stdout_resource = Some(stdio);
        Ok(self)
    }

    pub(crate) fn stdin_native(&mut self, stdio: Stdio) {
        self.stdin_resource = None;
        self.stdin = Some(stdio);
    }

    pub(crate) fn stdout_native(&mut self, stdio: Stdio) {
        self.stdout_resource = None;
        self.stdout = Some(stdio);
    }

    pub(crate) fn stderr_native(&mut self, stdio: Stdio) {
        self.stderr_resource = None;
        self.stderr = Some(stdio);
        self.stderr_to_stdout = false;
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

    pub(crate) fn stderr(&mut self, stdio: SpawnOut) -> Result<&mut Self> {
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

        let mut files = Vec::new();
        if let Some(stdin) = self.stdin_resource.take() {
            self.stdin = Some(match stdin {
                SpawnIn::Pipe(pipe) => pipe.into_stdio().await?,
                SpawnIn::File(file) => Stdio::from(child_file(&file, false, &mut files)?),
            });
        }
        if self.stderr_to_stdout {
            if let Some(stdout) = self.stdout_resource.take() {
                let (stdout, stderr) = match stdout {
                    SpawnOut::Pipe(pipe) => {
                        let stderr = pipe.try_clone().await?;
                        (pipe.into_stdio().await?, stderr.into_stdio().await?)
                    }
                    SpawnOut::File(file) => {
                        let handle = child_file(&file, true, &mut files)?;
                        let stderr = handle.try_clone()?;
                        (Stdio::from(handle), Stdio::from(stderr))
                    }
                };
                self.stdout = Some(stdout);
                self.stderr = Some(stderr);
            } else if self.stdout.is_some() {
                self.impl_stderr_inherit_stdout()?;
            }
        } else if let Some(stderr) = self.stderr_resource.take() {
            self.stderr = Some(child_out(stderr, &mut files).await?);
        }
        if let Some(stdout) = self.stdout_resource.take() {
            self.stdout = Some(child_out(stdout, &mut files).await?);
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
        let mut child = self.finish_spawn(child)?;
        child.files = files;
        Ok(child)
    }
}

/// Opens `file` for a child's standard stream, collecting what moves its
/// cursor afterwards into `files`.
fn child_file(file: &file::File, send: bool, files: &mut Vec<ChildFile>) -> Result<std::fs::File> {
    let file::FileInner::Direct(file) = &file.inner else {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "file belongs to a different VFS",
        ));
    };
    let (handle, sync) = file.child_stdio(send)?;
    files.extend(sync);
    Ok(handle)
}

/// Converts a standard output or error stream for the child.
async fn child_out(stdio: SpawnOut, files: &mut Vec<ChildFile>) -> Result<Stdio> {
    Ok(match stdio {
        SpawnOut::Pipe(pipe) => pipe.into_stdio().await?,
        SpawnOut::File(file) => Stdio::from(child_file(&file, true, files)?),
    })
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

    pub(crate) async fn open(&self, path: path::Path<'_>) -> Result<File> {
        // `as_tokio` carries the platform-specific open flags, so go through
        // it and then unwrap: the handle is driven positionally from here on
        // and has no use for tokio's cursor bookkeeping.
        let file = self
            .as_tokio()
            .open(path.to_native()?)
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

    pub(crate) fn cwd(&self) -> path::Path<'_> {
        self.initial.cwd.to_path()
    }

    pub(crate) fn current_exe(&self) -> path::Path<'_> {
        self.initial.current_exe.to_path()
    }

    pub(crate) fn target(&self) -> &TargetInfo {
        &self.initial.target
    }

    pub(crate) fn session(&self) -> uuid::Uuid {
        self.initial.session
    }

    pub(crate) fn pid(&self) -> u32 {
        self.initial.pid
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

    pub(crate) fn command(&self, program: path::Path<'_>) -> Command<'_> {
        Command::new(self, program)
    }

    pub(crate) async fn unix_socket(
        &self,
        path: path::Path<'_>,
        key: Option<&[u8]>,
    ) -> Result<Vfs> {
        #[cfg(unix)]
        {
            let key = key
                .map(dolang_rpc::auth::AuthKey::new)
                .transpose()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            Ok(Vfs::from_client(
                crate::client::Client::connect_with_key(path.to_native()?, key).await?,
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
        cwd: path::Path<'_>,
        env: HashMap<String, Option<String>>,
        elevate: bool,
    ) -> Result<Vfs> {
        #[cfg(windows)]
        {
            let cwd = cwd.to_native()?;
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

    pub(crate) async fn pipe(&self, buf_size: Option<usize>) -> Result<(PipeSend, PipeRecv)> {
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

    pub(crate) async fn processes(&self) -> Result<process::Processes> {
        Processes::open(self.session())
            .await
            .map(process::Processes::direct)
    }

    pub(crate) async fn describe_process(&self, pid: u32) -> Result<process::ProcessInfo> {
        Processes::describe_one(self.session(), pid).await
    }

    pub(crate) async fn open_process(
        &self,
        pid: u32,
        start: Option<process::StartTime>,
    ) -> Result<process::Process> {
        Process::open(self.session(), pid, start)
            .await
            .map(|handle| process::Process::direct(pid, handle))
    }

    pub(crate) async fn read_dir(&self, path: path::Path<'_>) -> Result<directory::ReadDir> {
        ReadDir::open(&path.to_native()?)
            .await
            .map(directory::ReadDir::direct)
    }

    pub(crate) async fn which(
        &self,
        program: path::Path<'_>,
        path: Option<&str>,
        cwd: Option<path::Path<'_>>,
    ) -> Result<Option<path::PathBuf>> {
        let program = program.to_native()?;
        let cwd = cwd.map(|cwd| cwd.to_native()).transpose()?;
        self.path_cache
            .resolve(&program, path, cwd.as_deref())
            .await
            .map(path::PathBuf::from_native)
            .transpose()
    }

    pub(crate) async fn well_known_path(
        &self,
        key: WellKnownPath,
        app: Option<&str>,
        env: &HashMap<String, Option<String>>,
    ) -> Result<Option<path::PathBuf>> {
        let path = match key {
            WellKnownPath::HomeDir => Self::home_dir_platform(env),
            WellKnownPath::CacheDir => Self::cache_dir_platform(app, env),
            WellKnownPath::TempDir => Self::temp_dir_platform(env),
            WellKnownPath::DataDir => Self::data_dir_platform(app, env, false),
            WellKnownPath::DataLocalDir => Self::data_dir_platform(app, env, true),
            WellKnownPath::StateDir => {
                return Self::state_dir_platform(app, env)?
                    .map(path::PathBuf::from_native)
                    .transpose();
            }
        }?;
        path::PathBuf::from_native(path).map(Some)
    }

    pub(crate) async fn clear_cache(&self) -> Result<()> {
        self.path_cache.clear().await;
        #[cfg(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos",
            windows
        ))]
        copy::clear_cache();
        Ok(())
    }

    pub(crate) async fn xattrs(
        &self,
        path: path::Path<'_>,
        namespace: XattrNamespace<'_>,
        follow: bool,
    ) -> Result<Vec<XattrEntry>> {
        self.impl_xattrs(&path.to_native()?, namespace, follow)
            .await
    }

    pub(crate) async fn streams(
        &self,
        path: path::Path<'_>,
        follow: bool,
    ) -> Result<Vec<StreamEntry>> {
        self.impl_streams(&path.to_native()?, follow).await
    }

    pub(crate) async fn xattr(
        &self,
        path: path::Path<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<Vec<u8>> {
        self.impl_xattr(&path.to_native()?, name, namespace, follow)
            .await
    }

    pub(crate) async fn set_xattr(
        &self,
        path: path::Path<'_>,
        name: &str,
        namespace: Option<&str>,
        value: &[u8],
        follow: bool,
    ) -> Result<()> {
        self.impl_set_xattr(&path.to_native()?, name, namespace, value, follow)
            .await
    }

    pub(crate) async fn remove_xattr(
        &self,
        path: path::Path<'_>,
        name: &str,
        namespace: Option<&str>,
        follow: bool,
    ) -> Result<()> {
        self.impl_remove_xattr(&path.to_native()?, name, namespace, follow)
            .await
    }

    pub(crate) async fn remove(&self, path: path::Path<'_>, all: bool, ignore: bool) -> Result<()> {
        let path = path.to_native()?;
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

    pub(crate) async fn metadata(&self, path: path::Path<'_>) -> Result<Metadata> {
        #[cfg(unix)]
        {
            let path = path.to_native()?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, true))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
        #[cfg(windows)]
        {
            let path = path.to_native()?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, true))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
    }

    pub(crate) async fn fs_metadata(
        &self,
        path: path::Path<'_>,
        follow: bool,
    ) -> Result<FsMetadata> {
        let path = path.to_native()?;
        tokio::task::spawn_blocking(move || Self::fs_metadata_from_path(&path, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join fs metadata query task")))
    }

    pub(crate) async fn acl(
        &self,
        path: path::Path<'_>,
        kind: AclKind,
        default: bool,
        follow: bool,
    ) -> Result<Option<Acl>> {
        let path = path.to_native()?;
        tokio::task::spawn_blocking(move || Self::acl_from_path(&path, kind, default, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join ACL query task")))
    }

    pub(crate) async fn set_acl(
        &self,
        path: path::Path<'_>,
        kind: AclKind,
        acl: Option<&Acl>,
        default: bool,
        follow: bool,
    ) -> Result<()> {
        let path = path.to_native()?;
        let acl = acl.cloned();
        tokio::task::spawn_blocking(move || {
            Self::set_acl_path(&path, kind, acl.as_ref(), default, follow)
        })
        .await
        .unwrap_or_else(|_| Err(Error::other("failed to join ACL update task")))
    }

    pub(crate) async fn sec_desc(
        &self,
        path: path::Path<'_>,
        mask: dolang_winterop::security::SecInfo,
        follow: bool,
    ) -> Result<SecDesc> {
        let path = path.to_native()?;
        tokio::task::spawn_blocking(move || Self::sec_desc_from_path(&path, mask, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn update_sec_desc(
        &self,
        path: path::Path<'_>,
        sec_desc: &SecDesc,
        follow: bool,
    ) -> Result<()> {
        let path = path.to_native()?;
        let sec_desc = sec_desc.clone();
        tokio::task::spawn_blocking(move || Self::update_sec_desc_path(&path, &sec_desc, follow))
            .await
            .unwrap_or_else(|_| Err(Error::other("failed to join security descriptor task")))
    }

    pub(crate) async fn create_dir(&self, path: path::Path<'_>, all: bool) -> Result<()> {
        let path = path.to_native()?;
        if all {
            Ok(fs::create_dir_all(path).await?)
        } else {
            Ok(fs::create_dir(path).await?)
        }
    }

    pub(crate) async fn remove_dir(
        &self,
        path: path::Path<'_>,
        all: bool,
        ignore: bool,
    ) -> Result<()> {
        let path = path.to_native()?;
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
        from: path::Path<'_>,
        to: path::Path<'_>,
        all: bool,
    ) -> Result<()> {
        Self::copy_local(&from.to_native()?, &to.to_native()?, all).await
    }

    pub(crate) async fn rename(
        &self,
        from: path::Path<'_>,
        to: path::Path<'_>,
        replace: bool,
    ) -> Result<()> {
        Self::impl_rename(from.to_native()?, to.to_native()?, replace).await
    }

    pub(crate) async fn move_(
        &self,
        from: path::Path<'_>,
        to: path::Path<'_>,
        all: bool,
    ) -> Result<()> {
        Self::move_local(&from.to_native()?, &to.to_native()?, all).await
    }

    pub(crate) async fn symlink(
        &self,
        cwd: path::Path<'_>,
        src: path::Path<'_>,
        dst: path::Path<'_>,
    ) -> Result<()> {
        Self::impl_symlink(&cwd.to_native()?, &src.to_native()?, &dst.to_native()?).await
    }

    pub(crate) async fn hard_link(&self, src: path::Path<'_>, dst: path::Path<'_>) -> Result<()> {
        Ok(fs::hard_link(src.to_native()?, dst.to_native()?).await?)
    }

    pub(crate) async fn symlink_dir(&self, src: path::Path<'_>, dst: path::Path<'_>) -> Result<()> {
        Self::impl_symlink_dir(&src.to_native()?, &dst.to_native()?).await
    }

    pub(crate) async fn symlink_file(
        &self,
        src: path::Path<'_>,
        dst: path::Path<'_>,
    ) -> Result<()> {
        Self::impl_symlink_file(&src.to_native()?, &dst.to_native()?).await
    }

    pub(crate) async fn symlink_metadata(&self, path: path::Path<'_>) -> Result<Metadata> {
        #[cfg(unix)]
        {
            let path = path.to_native()?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, false))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
        #[cfg(windows)]
        {
            let path = path.to_native()?;
            tokio::task::spawn_blocking(move || Self::metadata_from_path(&path, false))
                .await
                .unwrap_or_else(|_| Err(Error::other("failed to join metadata query task")))
        }
    }

    pub(crate) async fn update_metadata(
        &self,
        paths: &[path::PathBuf],
        patch: MetadataPatch,
    ) -> Result<()> {
        let paths = paths
            .iter()
            .map(|path| path.to_path().to_native())
            .collect::<Result<Vec<_>>>()?;
        self.impl_update_metadata(&paths, patch).await
    }

    pub(crate) async fn canonicalize(&self, path: path::Path<'_>) -> Result<path::PathBuf> {
        path::PathBuf::from_native(self.impl_canonicalize(&path.to_native()?).await?)
    }

    pub(crate) async fn read_link(&self, path: path::Path<'_>) -> Result<path::PathBuf> {
        path::PathBuf::from_native(fs::read_link(path.to_native()?).await?)
    }

    pub(crate) async fn access(&self, path: path::Path<'_>, mode: AccessFlags) -> Result<()> {
        Self::impl_access(path.to_native()?, mode).await
    }

    pub(crate) async fn glob(
        &self,
        pattern: impl Into<String>,
        root: path::Path<'_>,
        follow_symlinks: bool,
        max_depth: Option<usize>,
    ) -> Result<Vec<path::PathBuf>> {
        let pattern = pattern.into();
        let root = root.to_native()?;
        tokio::task::spawn_blocking(move || {
            let (prefix, glob) = Glob::new(&pattern)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid glob pattern"))?
                .partition();
            let walk_root = root.join(&prefix);

            let mut paths = Vec::new();

            // A pattern with no wildcards is partitioned entirely into
            // `prefix`, leaving no glob; it matches if it exists, or else not
            let Some(glob) = glob else {
                if std::fs::symlink_metadata(&walk_root).is_ok() {
                    paths.push(prefix.clone());
                }
                return paths
                    .into_iter()
                    .map(path::PathBuf::from_native)
                    .collect::<Result<_>>();
            };

            let mut behavior = WalkBehavior::default();
            if follow_symlinks {
                behavior.link = LinkBehavior::ReadTarget;
            }
            // wax prunes directories that fail to match their pattern
            // component, but keeps descending below one that matches even
            // when the path is already deeper than the pattern can reach, so
            // bound the walk by the deepest match the pattern allows
            let pattern_depth = match glob.depth() {
                DepthVariance::Invariant(depth) => Some(depth),
                DepthVariance::Variant(range) => range.upper().bounded().map(usize::from),
            };
            let depth = [
                max_depth.map(|depth| depth.saturating_sub(prefix.components().count())),
                pattern_depth,
            ]
            .into_iter()
            .flatten()
            .min();
            if let Some(depth) = depth {
                behavior.depth = DepthBehavior::Max(DepthMax(depth));
            }

            for entry in glob.walk_with_behavior(&walk_root, behavior) {
                let entry = entry.map_err(io::Error::other)?;
                let relative = entry.root_relative_paths().1;
                // The walk root reports itself with an empty root-relative
                // path, and `PathBuf::join("")` appends a trailing separator,
                // producing a path every later `stat` of a non-directory
                // rejects with ENOTDIR.
                paths.push(if relative.as_os_str().is_empty() {
                    prefix.clone()
                } else {
                    prefix.join(relative)
                });
            }

            paths.sort();
            paths
                .into_iter()
                .map(path::PathBuf::from_native)
                .collect::<Result<_>>()
        })
        .await
        .unwrap_or_else(|e| Err(Error::new(ErrorKind::Other, e.to_string())))
    }
}
