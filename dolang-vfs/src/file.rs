//! File-specific values shared by VFS implementations.

use std::{
    future::Future,
    mem::MaybeUninit,
    pin::Pin,
    sync::atomic::Ordering,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use dolang_winterop::security::SecDesc;
use serde::{Deserialize, Serialize};

use crate::{
    client, direct,
    error::{Error, ErrorKind, Result},
    metadata::{AttrFlags, FsMetadata, Metadata, Mode, Resolve},
    path,
    security::{Acl, AclKind},
};

mod copy;
mod cursor;

pub(crate) use copy::{COPY_LIMIT, FileId};
pub use cursor::Cursor;
pub(crate) use cursor::WriteOp;

/// Selects an extended-attribute namespace when listing attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrNamespace<'a> {
    /// The target's default namespace.
    Default,
    /// One named target-specific namespace.
    Named(&'a str),
    /// Every namespace supported by the target.
    Any,
}

/// Describes one extended attribute without reading its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XattrEntry {
    /// Attribute name within its namespace.
    pub(crate) name: String,
    /// Namespace, when the target reports one separately.
    pub(crate) namespace: Option<String>,
    /// Value size, when available without reading it.
    pub(crate) size: Option<u64>,
    /// Target-specific attribute flags.
    pub(crate) flags: Option<u8>,
}

impl XattrEntry {
    /// Returns the attribute name without its namespace prefix.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Returns the attribute namespace, if one was reported separately.
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }
    /// Returns the attribute value size in bytes, if available.
    pub const fn size(&self) -> Option<u64> {
        self.size
    }
    /// Returns the platform-specific attribute flags, if available.
    pub const fn flags(&self) -> Option<u8> {
        self.flags
    }
}

/// Describes one alternate data stream associated with a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
    /// Stream name.
    pub(crate) name: String,
    /// Stream type reported by the target.
    pub(crate) r#type: String,
    /// Logical stream length in bytes.
    pub(crate) size: u64,
    /// Allocated stream size in bytes.
    pub(crate) alloc_size: u64,
}

impl StreamEntry {
    /// Returns the stream name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Returns the stream type reported by the target.
    pub fn stream_type(&self) -> &str {
        &self.r#type
    }
    /// Returns the logical stream length in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }
    /// Returns the allocated stream size in bytes.
    pub const fn alloc_size(&self) -> u64 {
        self.alloc_size
    }
}

bitflags::bitflags! {
    /// Permissions checked by [`Vfs::access`](crate::Vfs::access).
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct AccessFlags: i32 {
        /// Checks execute permission.
        const X_OK = 1;
        /// Checks write permission.
        const W_OK = 2;
        /// Checks read permission.
        const R_OK = 4;
        /// Checks only whether the path exists.
        const F_OK = 0;
    }
}

bitflags::bitflags! {
    /// What other opens of a file may do while it is open. Only Windows
    /// enforces this; elsewhere only [`ShareMode::all`] is accepted.
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct ShareMode: u8 {
        /// Other opens may read.
        const READ = 1 << 0;
        /// Other opens may write.
        const WRITE = 1 << 1;
        /// Other opens may delete or rename.
        const DELETE = 1 << 2;
    }
}

/// Windows file access rights, by their `winnt.h` values, that decide what a
/// handle opened with an explicit access mask can do.
pub(crate) mod rights {
    pub(crate) const FILE_READ_DATA: u32 = 0x0001;
    pub(crate) const FILE_WRITE_DATA: u32 = 0x0002;
    pub(crate) const FILE_APPEND_DATA: u32 = 0x0004;
    pub(crate) const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    pub(crate) const GENERIC_ALL: u32 = 0x1000_0000;
    pub(crate) const GENERIC_WRITE: u32 = 0x4000_0000;
    pub(crate) const GENERIC_READ: u32 = 0x8000_0000;
}

/// Open settings beyond the portable access and disposition flags. They
/// travel to the target unchanged, which decides whether it supports them.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct OpenExtra {
    /// Windows access rights, added to those `read`/`write`/`append` imply.
    pub(crate) access: Option<u32>,
    /// Windows share mode; `None` shares everything.
    pub(crate) share: Option<ShareMode>,
    /// Windows backup semantics.
    pub(crate) backup: bool,
    /// Unix permission bits for a created file.
    pub(crate) mode: Option<Mode>,
    /// Windows security descriptor for a created file.
    pub(crate) sec_desc: Option<SecDesc>,
    /// Windows attributes for a created file.
    pub(crate) attrs: AttrFlags,
}

/// What a file opened with some [`OpenOptions`] can do, judged from the
/// rights requested. The target may still refuse an operation the handle
/// turns out not to allow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenCaps {
    /// Reads are possible.
    pub read: bool,
    /// Writes are possible.
    pub write: bool,
    /// Every write lands at the end of the file, so positions mean nothing to
    /// writes.
    pub append: bool,
}

impl OpenCaps {
    /// Judges the capabilities of an open with the portable `read`, `write`
    /// and `append` flags plus the explicit Windows rights in `access`.
    pub fn new(read: bool, write: bool, append: bool, access: Option<u32>) -> Self {
        use rights::*;
        let access = access.unwrap_or(0);
        let any = |bits: u32| access & bits != 0;
        let read = read || any(FILE_READ_DATA | GENERIC_READ | GENERIC_ALL | MAXIMUM_ALLOWED);
        let write_data = (write && !append)
            || any(FILE_WRITE_DATA | GENERIC_WRITE | GENERIC_ALL | MAXIMUM_ALLOWED);
        // As in std, append implies write
        let append_data = append || any(FILE_APPEND_DATA);
        Self {
            read,
            write: write_data || append_data,
            append: append_data && !write_data,
        }
    }
}

/// Lock access requested for a byte range of a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileLockMode {
    /// Prevents other exclusive or shared locks from overlapping this range.
    Exclusive,
    /// Allows other shared locks but not exclusive locks to overlap this range.
    Shared,
}

/// Whether acquiring a file lock may wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileLockBehavior {
    /// Waits until the lock can be acquired.
    Blocking,
    /// Returns without waiting when the lock cannot be acquired.
    Try,
}

/// A half-open byte range used for a file lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileLockRange {
    /// Inclusive byte offset at which the range starts.
    pub(crate) start: u64,
    /// Exclusive byte offset at which the range ends, or no end for EOF.
    pub(crate) end: Option<u64>,
}

impl FileLockRange {
    /// Creates a range from `start` to the exclusive `end`, or to EOF.
    ///
    /// Returns an error if `end` precedes `start`.
    pub fn new(start: u64, end: Option<u64>) -> Result<Self> {
        if end.is_some_and(|end| end < start) {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::InvalidInput,
                "lock range end precedes its start",
            ));
        }
        Ok(Self { start, end })
    }

    /// Creates a range extending from `start` to EOF.
    pub const fn to_eof(start: u64) -> Self {
        Self { start, end: None }
    }
    /// Returns the inclusive starting offset.
    pub const fn start(self) -> u64 {
        self.start
    }
    /// Returns the exclusive ending offset, or `None` for EOF.
    pub const fn end(self) -> Option<u64> {
        self.end
    }
    /// Returns whether this range contains no bytes.
    pub fn is_empty(self) -> bool {
        self.end == Some(self.start)
    }

    pub(crate) fn conflicts(self, other: Self) -> bool {
        match (self.is_empty(), other.is_empty()) {
            (true, true) => return false,
            (true, false) => {
                return other.start < self.start && self.start < other.end.unwrap_or(u64::MAX);
            }
            (false, true) => {
                return self.start < other.start && other.start < self.end.unwrap_or(u64::MAX);
            }
            (false, false) => {}
        }
        self.start < other.end.unwrap_or(u64::MAX) && other.start < self.end.unwrap_or(u64::MAX)
    }
}

/// A complete request to acquire a file lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileLockRequest {
    /// Byte range to lock.
    pub(crate) range: FileLockRange,
    /// Access mode to acquire.
    pub(crate) mode: FileLockMode,
    /// Whether acquisition may block.
    pub(crate) behavior: FileLockBehavior,
}

impl FileLockRequest {
    pub(crate) const fn new(
        range: FileLockRange,
        mode: FileLockMode,
        behavior: FileLockBehavior,
    ) -> Self {
        Self {
            range,
            mode,
            behavior,
        }
    }
}

/// A held file lock released explicitly or when dropped.
pub struct FileLock {
    inner: Option<FileLockInner>,
}

enum FileLockInner {
    Direct(direct::FileLock),
    Remote(client::FileLock),
}

impl FileLock {
    pub(crate) fn direct(lock: direct::FileLock) -> Self {
        Self {
            inner: Some(FileLockInner::Direct(lock)),
        }
    }

    pub(crate) fn remote(lock: client::FileLock) -> Self {
        Self {
            inner: Some(FileLockInner::Remote(lock)),
        }
    }

    /// Releases the lock. Calling this after a successful release is a no-op.
    pub async fn release(&mut self) -> Result<()> {
        let Some(lock) = self.inner.as_mut() else {
            return Ok(());
        };
        let result = match lock {
            FileLockInner::Direct(lock) => lock.release().await,
            FileLockInner::Remote(lock) => lock.release().await,
        };
        if result.is_ok() {
            self.inner = None;
        }
        result
    }
}

impl std::fmt::Debug for FileLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileLock")
            .field("released", &self.inner.is_none())
            .finish()
    }
}
/// Whether a copy may — or must — share blocks with its source rather than
/// duplicating them.
///
/// The distinction is not observable in the byte content either way, which is
/// why it needs asking for explicitly: a caller copying for deduplication has
/// no other way to learn whether it got sharing or a full duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyMode {
    /// Share blocks when the platform and both files allow it, and copy the
    /// data outright when they do not.
    Auto,
    /// Fail rather than copy the data outright.
    Require,
    /// Copy the data outright even where sharing is available.
    Never,
}

/// Where a copy's bytes land in the destination.
///
/// Not a bare offset: an append-mode handle has no offset to name — both
/// backends refuse a positional write on one, and the position is chosen by
/// the platform at write time — yet copying to the current position of such a
/// handle is perfectly meaningful.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CopyDest {
    /// At an absolute offset.
    At(u64),
    /// Through [`File::append`], which places bytes at the end of a handle
    /// opened for appending. Like `append` itself, this belongs to such a
    /// handle: on any other one it writes wherever the platform's own position
    /// happens to be.
    Append,
}

/// Result of one bounded [`File::copy_data`] operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyDataResult {
    /// Number of logical bytes copied, including preserved holes.
    pub count: u64,
    /// Actual destination end for an append copy.
    pub destination_end: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) enum FileInner {
    Client(client::File),
    Direct(direct::File),
}

/// An asynchronous file handle backed by either a remote VFS client or the local process.
///
/// File handles implement Tokio's asynchronous read, write, and seek traits.
///
/// Clones share one open file, as duplicated descriptors do: one cursor
/// position, one set of locks, and one closed state. Closing or handing off
/// the file through any clone closes it for all of them, and their later
/// operations fail with [`ErrorKind::Closed`]. Operations in flight are not
/// shared: a clone starts with none, and its reads and writes do not wait for
/// another clone's.
#[derive(Debug)]
pub struct File {
    pub(crate) inner: FileInner,
    cursor: Option<Box<cursor::CursorState>>,
}

impl Clone for File {
    fn clone(&self) -> Self {
        Self::with_cursor(self.inner.clone(), None)
    }
}

impl File {
    pub(crate) fn client(file: client::File) -> Self {
        Self::with_cursor(FileInner::Client(file), None)
    }

    pub(crate) fn direct(file: direct::File) -> Self {
        Self::with_cursor(FileInner::Direct(file), None)
    }

    fn with_cursor(inner: FileInner, cursor: Option<Box<cursor::CursorState>>) -> Self {
        Self { inner, cursor }
    }
}

/// One of two futures, chosen at dispatch time.
///
/// The positional operations return `impl Future`, so a backend that dispatches
/// between two implementations has two distinct future types to reconcile and
/// cannot simply `match` inside an `async` block: the returned future captures
/// no lifetimes, so it cannot borrow the handle it came from. Boxing would
/// work; this avoids the allocation on what is meant to be the hot path.
pub(crate) enum EitherFuture<L, R> {
    Left(L),
    Right(R),
}

impl<T, L: Future<Output = T>, R: Future<Output = T>> Future for EitherFuture<L, R> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        // SAFETY: the projection is structural and neither variant is ever
        // moved out of, so the pinning guarantee carries through to whichever
        // future is inside.
        unsafe {
            match self.get_unchecked_mut() {
                Self::Left(future) => Pin::new_unchecked(future).poll(cx),
                Self::Right(future) => Pin::new_unchecked(future).poll(cx),
            }
        }
    }
}

macro_rules! match_file {
    (move $self:expr, $file:ident => $body:expr) => {{
        match $self.inner {
            FileInner::Client($file) => $body,
            FileInner::Direct($file) => $body,
        }
    }};
    ($self:expr, $file:ident => $body:expr) => {{
        match &$self.inner {
            FileInner::Client($file) => $body,
            FileInner::Direct($file) => $body,
        }
    }};
}

impl File {
    /// Closes the file, for every clone. Closing a closed file does nothing.
    pub async fn close(mut self) -> Result<()> {
        self.cancel_cursor().await;
        match_file!(move self, file => file.close().await)
    }

    /// Reads into `buf`'s spare capacity starting at `offset`, returning the
    /// transfer count.
    ///
    /// Cancelling the read may leave `buf` empty.
    ///
    /// Fewer bytes than the spare capacity available may be read. `0` indicates
    /// either end-of-file or that `buf` had no spare capacity.
    pub fn read_at<'b>(
        &self,
        buf: &'b mut BytesMut,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        match &self.inner {
            FileInner::Client(file) => EitherFuture::Left(file.read_at(buf, offset)),
            FileInner::Direct(file) => EitherFuture::Right(file.read_at(buf, offset)),
        }
    }

    /// Writes `data` at `offset`, returning the byte count.
    ///
    /// May write less than all of `data` on success, but at least 1 byte unless `data` is empty.
    /// Use on an append-mode handle is an error; use [`append`](Self::append) there.
    pub fn write_at(
        &self,
        data: Bytes,
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<> {
        match &self.inner {
            FileInner::Client(file) => EitherFuture::Left(file.write_at(data, offset)),
            FileInner::Direct(file) => EitherFuture::Right(file.write_at(data, offset)),
        }
    }

    /// Appends `data`, returning the byte count and the position just past
    /// what was written.  Less data may be written than requested on success,
    /// but at least 1 byte will be written unless `data` was empty.
    pub fn append(&self, data: Bytes) -> impl Future<Output = Result<(usize, u64)>> + Send + use<> {
        match &self.inner {
            FileInner::Client(file) => EitherFuture::Left(file.append(data)),
            FileInner::Direct(file) => EitherFuture::Right(file.append(data)),
        }
    }

    /// Reads into the possibly uninitialized `buf``, returning how
    /// many bytes at its front were filled.
    pub fn read_at_into<'b>(
        &self,
        buf: &'b mut [MaybeUninit<u8>],
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        match &self.inner {
            FileInner::Client(file) => EitherFuture::Left(file.read_at_into(buf, offset)),
            FileInner::Direct(file) => EitherFuture::Right(file.read_at_into(buf, offset)),
        }
    }

    /// Writes `data` at `offset` from borrowed storage, returning the byte
    /// count.
    ///
    /// Less data may be written than requested on success, but always at least 1
    /// byte unless `data` is empty.
    pub fn write_at_from<'b>(
        &self,
        data: &'b [u8],
        offset: u64,
    ) -> impl Future<Output = Result<usize>> + Send + use<'b> {
        match &self.inner {
            FileInner::Client(file) => EitherFuture::Left(file.write_at_from(data, offset)),
            FileInner::Direct(file) => EitherFuture::Right(file.write_at_from(data, offset)),
        }
    }

    /// Copies at most 2 MiB from this file into `dst`.
    ///
    /// `len` of `None` copies toward end of source. A positive result may be
    /// short for any reason; callers that want a complete operation must loop.
    /// Zero indicates end of source (or an empty request).
    ///
    /// Nothing is promised about physical allocation: an accelerated route may
    /// preserve holes, and the fallback does not.
    pub async fn copy_data(
        &self,
        dst: &File,
        src_offset: u64,
        target: CopyDest,
        len: Option<u64>,
        mode: CopyMode,
    ) -> Result<CopyDataResult> {
        self.check_overlap(dst, src_offset, target, len).await?;
        let len = len.map(|len| len.min(copy::COPY_LIMIT));
        match (&self.inner, &dst.inner) {
            // `Never` still takes this route: it forbids block sharing, not
            // running the copy on the side that owns both files.
            (FileInner::Client(src), FileInner::Client(dst)) if src.can_copy_data_with(dst) => {
                src.copy_data(dst, src_offset, target, len, mode).await
            }
            #[cfg(any(
                target_os = "linux",
                target_os = "freebsd",
                target_os = "macos",
                windows
            ))]
            (FileInner::Direct(src_direct), FileInner::Direct(dst_direct))
                if matches!(target, CopyDest::At(_)) =>
            {
                if src_direct.is_regular().await? && dst_direct.is_regular().await? {
                    src_direct
                        .copy_data(dst_direct, src_offset, target, len, mode)
                        .await
                } else if mode == CopyMode::Require {
                    Err(Error::new(
                        ErrorKind::Unsupported,
                        "block sharing is not supported for this copy",
                    ))
                } else {
                    copy::copy_chunked(self, dst, src_offset, target, len).await
                }
            }
            _ => {
                // The refusal sits here, immediately above the fallback,
                // rather than at the top of the function: once a platform
                // route exists it is inserted above this point and the refusal
                // becomes "sharing was attempted and could not be had", with
                // nothing else to re-plumb.
                if mode == CopyMode::Require {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "block sharing is not supported for this copy",
                    ));
                }
                copy::copy_chunked(self, dst, src_offset, target, len).await
            }
        }
    }

    /// Rejects a copy whose source and destination regions overlap within one
    /// file.
    ///
    /// Best effort by necessity: identity is knowable for two local handles
    /// and for two citations of one opaque file, but a mixed local/opaque pair
    /// — which one session can hand out, since `Open` may answer with either —
    /// has nothing cheap in common to compare. An unknown identity is treated
    /// as "cannot tell" and lets the copy proceed; a same-session opaque pair
    /// with distinct citations is checked on the server, where both handles
    /// are local.
    async fn check_overlap(
        &self,
        dst: &File,
        src_offset: u64,
        target: CopyDest,
        len: Option<u64>,
    ) -> Result<()> {
        let same = copy::same_opaque(self, dst)
            || match (copy::identity(self).await, copy::identity(dst).await) {
                (Some(src), Some(dst)) => src == dst,
                _ => false,
            };
        if !same {
            return Ok(());
        }
        // Only now, when the regions are known to share a file, is the length
        // worth a round trip: an open-ended copy runs to end of source, and an
        // append lands there too.
        let size = match (len, target) {
            (Some(_), CopyDest::At(_)) => 0,
            _ => self.metadata().await?.len(),
        };
        let src_end = match len {
            Some(len) => src_offset.saturating_add(len),
            None => size.max(src_offset),
        };
        let dst_start = match target {
            CopyDest::At(offset) => offset,
            CopyDest::Append => size,
        };
        let dst_end = dst_start.saturating_add(src_end - src_offset);
        if dst_start < src_end && src_offset < dst_end {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "source and destination regions of the same file overlap",
            ));
        }
        Ok(())
    }

    /// Changes the file length to `size` bytes.
    pub async fn set_size(&self, size: u64) -> Result<()> {
        match_file!(self, file => file.set_size(size).await)
    }

    /// Flushes the file's written data to durable storage, returning once the
    /// device reports it committed.
    ///
    /// `data` selects a data-only flush (`fdatasync`), which may skip metadata
    /// the caller does not need — notably the modification time — and so can
    /// avoid a second write to the inode. Size changes are still flushed,
    /// since a reader could not find the data without them.
    ///
    /// This durably places the *contents*. It says nothing about the
    /// directory entry naming the file, which is a separate inode and needs
    /// its own flush to survive a crash.
    pub async fn sync(&self, data: bool) -> Result<()> {
        match_file!(self, file => file.sync(data).await)
    }

    /// Returns metadata for the open file.
    pub async fn metadata(&self) -> Result<Metadata> {
        match_file!(self, file => file.metadata().await)
    }

    /// Returns metadata for the filesystem containing the open file.
    pub async fn fs_metadata(&self) -> Result<FsMetadata> {
        match_file!(self, file => file.fs_metadata().await)
    }

    /// Returns the ACL of the requested `kind`. For a POSIX ACL, `default`
    /// selects the directory's default ACL rather than its access ACL; it
    /// must be `false` for `AclKind::Nfs4`.
    pub async fn acl(&self, kind: AclKind, default: bool) -> Result<Option<Acl>> {
        match_file!(self, file => file.acl(kind, default).await)
    }

    /// Sets or removes the ACL of `kind`. `acl`, if present, must match
    /// `kind`. `default` selects the POSIX default ACL, as in
    /// [`acl`](Self::acl); it must be `false` for `AclKind::Nfs4`.
    pub async fn set_acl(&self, kind: AclKind, acl: Option<&Acl>, default: bool) -> Result<()> {
        match_file!(self, file => file.set_acl(kind, acl, default).await)
    }

    /// Returns the Windows security descriptor selected by `mask`.
    pub async fn sec_desc(&self, mask: dolang_winterop::security::SecInfo) -> Result<SecDesc> {
        match_file!(self, file => file.sec_desc(mask).await)
    }

    /// Replaces the Windows security descriptor.
    pub async fn update_sec_desc(&self, sec_desc: &SecDesc) -> Result<()> {
        match_file!(self, file => file.update_sec_desc(sec_desc).await)
    }

    /// Lists extended attributes in `namespace`.
    pub async fn xattrs(&self, namespace: XattrNamespace<'_>) -> Result<Vec<XattrEntry>> {
        match_file!(self, file => file.xattrs(namespace).await)
    }

    /// Reads one extended attribute.
    pub async fn xattr(&self, name: &str, namespace: Option<&str>) -> Result<Vec<u8>> {
        match_file!(self, file => file.xattr(name, namespace).await)
    }

    /// Lists alternate data streams.
    pub async fn streams(&self) -> Result<Vec<StreamEntry>> {
        match_file!(self, file => file.streams().await)
    }

    /// Returns the directory the Windows junction this file refers to leads
    /// to. The file must have been opened as itself rather than followed.
    pub async fn read_junction(&self) -> Result<crate::path::PathBuf> {
        match_file!(self, file => file.read_junction().await)
    }

    /// Makes the empty directory this file refers to a Windows junction to
    /// the absolute path `target`, or with no target, removes its junction.
    pub async fn set_junction(&self, target: Option<crate::path::Path<'_>>) -> Result<()> {
        match_file!(self, file => file.set_junction(target).await)
    }

    /// Creates or replaces an extended attribute.
    pub async fn set_xattr(&self, name: &str, namespace: Option<&str>, value: &[u8]) -> Result<()> {
        match_file!(self, file => file.set_xattr(name, namespace, value).await)
    }

    /// Removes an extended attribute.
    pub async fn remove_xattr(&self, name: &str, namespace: Option<&str>) -> Result<()> {
        match_file!(self, file => file.remove_xattr(name, namespace).await)
    }

    /// Acquires a byte-range lock with the requested mode and behavior.
    pub async fn lock(
        &self,
        range: FileLockRange,
        mode: FileLockMode,
        behavior: FileLockBehavior,
    ) -> Result<Option<FileLock>> {
        let request = FileLockRequest::new(range, mode, behavior);
        match_file!(self, file => file.lock(request).await)
    }

    /// Converts this handle into a local standard-library file when possible,
    /// taking it from every clone.
    ///
    /// Surrendering the descriptor means surrendering the cursor with it, so
    /// the file comes back positioned where this handle is.
    pub async fn try_into_std(self) -> std::result::Result<std::fs::File, Self> {
        if self.check_idle().is_err() {
            return Err(self);
        }
        let pos = self.inner.position().load(Ordering::Relaxed);
        let File { inner, cursor } = self;
        match inner {
            FileInner::Client(file) => file
                .try_into_std()
                .await
                .map_err(|file| Self::with_cursor(FileInner::Client(file), cursor)),
            FileInner::Direct(file) => file
                .try_into_std(pos)
                .await
                .map_err(|file| Self::with_cursor(FileInner::Direct(file), cursor)),
        }
    }
}

/// Configures and opens a file on one [`Vfs`] backend.
enum OpenOptionsInner<'a> {
    Client(client::OpenOptions<'a>),
    Direct(direct::OpenOptions),
}

pub struct OpenOptions<'a> {
    inner: OpenOptionsInner<'a>,
}

impl<'a> OpenOptions<'a> {
    pub(crate) fn client(options: client::OpenOptions<'a>) -> Self {
        Self {
            inner: OpenOptionsInner::Client(options),
        }
    }

    pub(crate) fn direct(options: direct::OpenOptions) -> Self {
        Self {
            inner: OpenOptionsInner::Direct(options),
        }
    }
}

impl OpenOptions<'_> {
    /// Enables or disables read access.
    pub fn read(&mut self, read: bool) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.read(read);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.read(read);
            }
        }
        self
    }

    /// Enables or disables write access.
    pub fn write(&mut self, write: bool) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.write(write);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.write(write);
            }
        }
        self
    }

    /// Enables or disables append mode.
    pub fn append(&mut self, append: bool) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.append(append);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.append(append);
            }
        }
        self
    }

    /// Enables or disables creation when the file is absent.
    pub fn create(&mut self, create: bool) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.create(create);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.create(create);
            }
        }
        self
    }

    /// Enables or disables exclusive creation.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.create_new(create_new);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.create_new(create_new);
            }
        }
        self
    }

    /// Enables or disables truncation when opening.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.truncate(truncate);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.truncate(truncate);
            }
        }
        self
    }

    /// Selects how the final path component is resolved.
    pub fn resolve(&mut self, resolve: Resolve) -> &mut Self {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => {
                opts.resolve(resolve);
            }
            OpenOptionsInner::Direct(opts) => {
                opts.resolve(resolve);
            }
        }
        self
    }

    fn extra(&mut self) -> &mut OpenExtra {
        match &mut self.inner {
            OpenOptionsInner::Client(opts) => opts.extra(),
            OpenOptionsInner::Direct(opts) => opts.extra(),
        }
    }

    /// Replaces every setting beyond the portable flags, as a request
    /// received from a client carries them.
    pub(crate) fn set_extra(&mut self, extra: OpenExtra) -> &mut Self {
        *self.extra() = extra;
        self
    }

    /// Requests Windows access rights in addition to those `read`, `write`
    /// and `append` imply. Only Windows supports it.
    pub fn access(&mut self, access: u32) -> &mut Self {
        self.extra().access = Some(access);
        self
    }

    /// Sets what other opens may do while the file is open. Only Windows
    /// supports anything but [`ShareMode::all`].
    pub fn share(&mut self, share: ShareMode) -> &mut Self {
        self.extra().share = Some(share);
        self
    }

    /// Enables Windows backup semantics, which open directories and use
    /// backup and restore privileges. Only Windows supports it.
    pub fn backup(&mut self, backup: bool) -> &mut Self {
        self.extra().backup = backup;
        self
    }

    /// Sets the Unix permission bits of a created file, subject to the umask.
    /// Only Unix supports it.
    pub fn mode(&mut self, mode: Mode) -> &mut Self {
        self.extra().mode = Some(mode);
        self
    }

    /// Sets the security descriptor of a created file. Only Windows supports
    /// it.
    pub fn sec_desc(&mut self, sec_desc: SecDesc) -> &mut Self {
        self.extra().sec_desc = Some(sec_desc);
        self
    }

    /// Sets the attributes of a created file. Only Windows supports them,
    /// and only those `CreateFileW` accepts: read-only, hidden, system,
    /// archive, temporary, offline and not-content-indexed.
    pub fn attrs(&mut self, attrs: AttrFlags) -> &mut Self {
        self.extra().attrs = attrs;
        self
    }

    /// Returns what a file opened with these options can do.
    pub fn caps(&self) -> OpenCaps {
        match &self.inner {
            OpenOptionsInner::Client(opts) => opts.caps(),
            OpenOptionsInner::Direct(opts) => opts.caps(),
        }
    }

    /// Opens `path` using the configured options.
    pub async fn open(&self, path: path::Path<'_>) -> Result<File> {
        match &self.inner {
            OpenOptionsInner::Client(opts) => client::OpenOptions::open(opts, path).await,
            OpenOptionsInner::Direct(opts) => direct::OpenOptions::open(opts, path)
                .await
                .map(File::direct),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileLockRange;
    use crate::error::ErrorKind;

    #[test]
    fn lock_range_construction_validates_order() {
        let range = FileLockRange::new(4, Some(8)).unwrap();
        assert_eq!(range.start(), 4);
        assert_eq!(range.end(), Some(8));
        assert!(!range.is_empty());
        assert!(
            FileLockRange::new(8, Some(4))
                .is_err_and(|error| error.kind() == ErrorKind::InvalidInput)
        );
        assert_eq!(FileLockRange::to_eof(4).end(), None);
    }
}
