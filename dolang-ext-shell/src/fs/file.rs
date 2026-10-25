use std::{
    io::{self, IoSlice, SeekFrom},
    ops::{Deref, DerefMut},
};

use dolang::runtime::{
    AllocExt, Arg, Args, BYTE_STREAM_CHUNK_SIZE, Error, Instance, Object, Output, Result, Slot,
    State, Strand, Sym, Value, call,
    io::{ByteHandle, ByteIo, IntoByteIo, IoCaps, Shared},
    method,
    object::{Cast, Mut, Ref, Request, TypeBuilder},
    strand::InterruptMask,
    unpack,
    value::{BinEmbryo, PinBin, PinStr, TypeObject, View},
};
use dolang_util::{shared::Locked, utf8};
use dolang_vfs::file::{
    CopyDest, CopyMode, File as VfsFile, FileLockBehavior, FileLockMode, FileLockRange, OpenOptions,
};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::compat::TokioAsyncReadCompatExt as _;

use dolang_vfs::path as vfs_path;

use crate::{
    error::ResultExt as _,
    fs::{
        file_buf::{FileBuf, read_chunk},
        file_lock::FileLock as FileLockObject,
        fs_metadata::create_fs_metadata,
        metadata::create_metadata,
        read_into_spare, stream, xattr,
    },
    global::{FsGlobal, WindowsSecurityTag},
    io_mode::encode_value,
    util,
};

fn lock_endpoint<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Slot<'v, '_>,
    name: &str,
) -> Result<'v, 's, Option<u64>> {
    if value.is_nil() {
        return Ok(None);
    }
    let value = value
        .as_int(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{name} must be an integer or nil")))?;
    u64::try_from(value)
        .map(Some)
        .map_err(|_| Error::value(strand, format!("{name} must be non-negative")))
}

/// Decodes a byte range into `(start, end)`, `end` open meaning "to the end of
/// the file".
///
/// Shared by `lock` and `copy_data` so the two cannot drift on what a range
/// means: half-open, `..` total, a missing start is `0`, `step` must be `1`,
/// and endpoints counted from the end are rejected — a file's length is racy,
/// so unlike `Array` and `Str` slicing there is nothing stable to count back
/// from. `what` names the range in the error messages.
fn byte_range<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Slot<'v, '_>,
    [mut start, mut end, mut step]: [Slot<'v, '_>; 3],
    what: &str,
) -> Result<'v, 's, (u64, Option<u64>)> {
    let range = value
        .as_range(strand)
        .ok_or_else(|| Error::type_error(strand, format!("{what} must be a range")))?;
    range.parts(
        strand,
        [
            Slot::reborrow(&mut start),
            Slot::reborrow(&mut end),
            Slot::reborrow(&mut step),
        ],
    );
    if !step.is_nil() && step.as_int(strand) != Some(1) {
        return Err(Error::value(
            strand,
            format!("{what} step must be 1 or unspecified"),
        ));
    }
    let start = lock_endpoint(strand, &start, &format!("{what} start"))?.unwrap_or(0);
    let end = lock_endpoint(strand, &end, &format!("{what} end"))?;
    if end.is_some_and(|end| end < start) {
        return Err(Error::value(
            strand,
            format!("{what} end must not precede its start"),
        ));
    }
    Ok((start, end))
}

fn lock_range<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Slot<'v, '_>,
    parts: [Slot<'v, '_>; 3],
) -> Result<'v, 's, FileLockRange> {
    let (start, end) = byte_range(strand, value, parts, "lock range")?;
    FileLockRange::new(start, end).map_err(|error| Error::value(strand, error.to_string()))
}

/// Access modes `fs.open` accepts.
pub(crate) const MODES: [&str; 8] = ["r", "w", "a", "x", "r+", "w+", "a+", "x+"];

/// Configures `opts` for `mode`, one of [`MODES`].
fn configure_options(opts: &mut OpenOptions, mode: &str) {
    match mode {
        "r" => {
            opts.read(true);
        }
        "w" => {
            opts.write(true).truncate(true).create(true);
        }
        "a" => {
            opts.write(true).append(true).create(true);
        }
        "x" => {
            opts.write(true).create_new(true);
        }
        "r+" => {
            opts.read(true).write(true);
        }
        "w+" => {
            opts.read(true).write(true).truncate(true).create(true);
        }
        "a+" => {
            opts.read(true).write(true).append(true).create(true);
        }
        "x+" => {
            opts.read(true).write(true).create_new(true);
        }
        _ => {
            // Invalid mode - will be handled by the caller
        }
    }
}

/// Parses an `offset:` keyword argument.
fn read_offset<'v, 's>(
    strand: &mut Strand<'v, 's>,
    offset: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<u64>> {
    offset
        .map(|offset| {
            offset
                .to_i64(strand)
                .ok()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| Error::type_error(strand, "offset must be a non-negative integer"))
        })
        .transpose()
}

/// Parses `write`'s arguments: any number of `Str` or `Bin` pieces, pinned, and an
/// optional `offset:`.
fn write_args<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    args: Args<'v, 'a>,
    offset_sym: Sym<'v, '_>,
) -> Result<'v, 's, (Vec<Pinned<'v, 'a>>, Option<Slot<'v, 'a>>)> {
    let mut pieces = Vec::with_capacity(args.len());
    let mut offset = None;
    for arg in args {
        match arg {
            Arg::Pos(value) => {
                let piece = match value.into_ref().view(strand) {
                    View::Str(value) => Pinned::Str(value.pin()),
                    View::Bin(value) => Pinned::Bin(value.pin()),
                    _ => return Err(Error::type_error(strand, "expected `Str` or `Bin`")),
                };
                pieces.push(piece);
            }
            Arg::Key(key, value) if key == offset_sym => {
                if offset.replace(value).is_some() {
                    return Err(Error::duplicate_key(strand, key));
                }
            }
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        }
    }
    Ok((pieces, offset))
}

/// Reads `size` bytes at `offset`, or to the end of the file when `size` is
/// `None`, looping over short transfers.
///
/// [`FileHandle::read_at_into`] is allowed to come up short for reasons other than
/// the end of the file — a remote read is capped at one chunk, a local one at
/// whatever the platform chose to return — which is the right contract for Rust
/// callers but a trap in a script, where the shortfall would surface only on
/// some filesystems or some transports. So `offset:` in Do means "read this
/// much", and stops early only at the end of the file.
///
/// Fills `embryo` directly rather than accumulating into an owned buffer and
/// copying: the destination is the collector's memory the result will be built
/// from, and `read_at_into` places bytes there without an intermediate. It is
/// not the arena zero-copy the cursor path gets — a blocking backend still
/// stages through a temporary of its own — but the remote backend, where a
/// script moving bulk data actually spends its time, reads its reply trailer
/// straight in.
async fn read_at_looping<'v, 's>(
    strand: &mut Strand<'v, 's>,
    file: &VfsFile,
    embryo: &mut BinEmbryo<'v>,
    offset: u64,
    size: Option<usize>,
) -> Result<'v, 's, ()> {
    loop {
        let want = match size {
            Some(size) if embryo.len() >= size => break,
            Some(size) => (size - embryo.len()).min(BYTE_STREAM_CHUNK_SIZE),
            None => BYTE_STREAM_CHUNK_SIZE,
        };
        if embryo.spare_capacity_mut().len() < want {
            embryo.reserve(strand, want);
        }
        let at = offset + embryo.len() as u64;
        // Trimmed to what is still wanted, not to whatever `reserve` rounded
        // the capacity up to, so a read cannot overshoot the requested size and
        // there is nothing to truncate afterwards.
        let spare = embryo.spare_capacity_mut();
        let end = want.min(spare.len());
        let read = file
            .read_at_into(&mut spare[..end], at)
            .await
            .into_sys(strand)?;
        if read == 0 {
            break;
        }
        // SAFETY: `read_at_into` reports how many bytes at the front of the
        // slice it filled, and initializes exactly those.
        unsafe { embryo.advance(read) };
    }
    Ok(())
}

/// Writes all of `data` at `offset`, looping over short transfers.
///
/// Short for the same reasons as [`read_at_looping`], and looped for the same
/// reason: a script asking to write a buffer means all of it.
async fn write_at_looping(file: &VfsFile, data: &[u8], offset: u64) -> io::Result<usize> {
    let total = data.len();
    let mut written = 0usize;
    while written < total {
        let n = file
            .write_at_from(&data[written..], offset + written as u64)
            .await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "file write made no progress",
            ));
        }
        written += n;
    }
    Ok(total)
}

/// A pinned `Str` or `Bin` payload, held so the bytes underneath it keep their
/// address for as long as a write is borrowing them.
enum Pinned<'v, 'a> {
    Str(PinStr<'v, 'a>),
    Bin(PinBin<'v, 'a>),
}

impl<'v, 'a> Pinned<'v, 'a> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Str(value) => value.as_bytes(),
            Self::Bin(value) => value,
        }
    }
}

/// Parses the `clone:` keyword argument
/// (`:AUTO:`/`:REQUIRE:`/`:NEVER:`), defaulting to [`CopyMode::Auto`].
pub(crate) fn copy_mode_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, CopyMode> {
    let Some(slot) = slot else {
        return Ok(CopyMode::Auto);
    };
    let sym = slot.as_sym(strand).ok_or_else(|| {
        Error::type_error(strand, "clone: expected :AUTO:, :REQUIRE:, or :NEVER:")
    })?;
    if sym == global.syms.auto {
        Ok(CopyMode::Auto)
    } else if sym == global.syms.require {
        Ok(CopyMode::Require)
    } else if sym == global.syms.never {
        Ok(CopyMode::Never)
    } else {
        Err(Error::value(
            strand,
            "clone: expected :AUTO:, :REQUIRE:, or :NEVER:",
        ))
    }
}

/// A borrow of one side of a copy.
///
/// Which one it is follows the rule `read` and `write` already use: a side
/// named by an absolute position touches nothing shared and takes a shared
/// borrow, so several copies and positional reads can be in flight on one
/// handle at once; a side addressed through its cursor moves that cursor and
/// takes the handle exclusively.
enum CopySide<'v, 'a, T: FileKind<'v>> {
    Positional(Ref<'v, 'a, FileObject<T>>),
    Cursor(Mut<'v, 'a, FileObject<T>>),
}

impl<'v, T: FileKind<'v>> CopySide<'v, '_, T> {
    fn file<'b, 's>(&'b self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, &'b VfsFile> {
        match self {
            Self::Positional(borrow) => borrow.open_ref(strand),
            Self::Cursor(borrow) => borrow.open_ref(strand),
        }
    }
}

/// Implements `fs.copy_data` and `File.copy_data`.
///
/// Both entry points funnel here so the two cannot disagree about addressing.
/// Everything below this point is absolute: ranges and cursors are resolved
/// into `(offset, destination, length)` before anything reaches the VFS, which
/// knows nothing of either.
#[allow(clippy::too_many_arguments)]
async fn copy_data<'v, 's, S: FileKind<'v>, D: FileKind<'v>>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    src: Instance<'v, '_, FileObject<S>>,
    dst: Instance<'v, '_, FileObject<D>>,
    range: Option<Slot<'v, '_>>,
    size: Option<Slot<'v, '_>>,
    offset: Option<Slot<'v, '_>>,
    clone: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    if range.is_some() && size.is_some() {
        // A range already carries its length; honoring both would mean
        // choosing which one to believe.
        return Err(Error::value(
            strand,
            "range: and size: cannot both be given",
        ));
    }
    let mode = copy_mode_sym(strand, global, clone)?;
    let size = size
        .map(|size| {
            size.to_i64(strand)
                .ok()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| Error::type_error(strand, "size must be a non-negative integer"))
        })
        .transpose()?;
    let offset = read_offset(strand, offset)?;
    let dst_is_append = dst.annex().is_append;
    if offset.is_some() && dst_is_append {
        // Same impossibility as `write`: an append handle places its bytes at
        // the end whatever offset the platform is given.
        return Err(Error::state_error(
            strand,
            "cannot copy data at an offset on a file opened for appending",
        ));
    }
    let region = match range {
        Some(range) => Some(
            strand
                .with_slots(async move |strand, [start, end, step]| {
                    byte_range(strand, &range, [start, end, step], "copy range")
                })
                .await?,
        ),
        None => None,
    };

    // Two shared borrows of one instance coexist, so this is safe to ask
    // before deciding anything, and it keeps `$f.copy_data $f` from surfacing
    // as a borrow conflict when a cursor is involved — a conflict the script
    // cannot act on, since it names nothing the script wrote.
    let same_handle = {
        let src = src.borrow(strand)?;
        let dst = dst.borrow(strand)?;
        std::ptr::addr_eq(&*src, &*dst)
    };
    if same_handle && (region.is_none() || offset.is_none()) {
        return Err(Error::state_error(
            strand,
            "cannot copy data between a file handle and itself through its cursor; \
             give both a range: and an offset:",
        ));
    }

    // Pending writes would be invisible to the copy, and the destination's
    // read-ahead would go stale under copied bytes.
    S::flush_pending(src, strand).await?;
    D::drop_buffers(dst, strand).await?;

    // Each side is borrowed and resolved together, because resolving a cursor
    // requires the exclusive borrow that addressing through it implies.
    let (mut src_side, src_offset, len) = match region {
        Some((start, end)) => (
            CopySide::Positional(src.borrow(strand)?),
            start,
            end.map(|end| end - start),
        ),
        None => {
            let mut borrow = src.borrow_mut(strand)?;
            let position = borrow.stream_position(strand).await?;
            (CopySide::Cursor(borrow), position, size)
        }
    };
    let (mut dst_side, mut target) = match offset {
        Some(offset) => (
            CopySide::Positional(dst.borrow(strand)?),
            CopyDest::At(offset),
        ),
        None if dst_is_append => (CopySide::Cursor(dst.borrow_mut(strand)?), CopyDest::Append),
        None => {
            let mut borrow = dst.borrow_mut(strand)?;
            let position = borrow.stream_position(strand).await?;
            (CopySide::Cursor(borrow), CopyDest::At(position))
        }
    };

    // An open-ended copy is a snapshot, not a subscription to source growth.
    // Resolve EOF once before the first bounded VFS operation.
    let len = match len {
        Some(len) => len,
        None => src_side
            .file(strand)?
            .metadata()
            .await
            .into_sys(strand)?
            .len()
            .saturating_sub(src_offset),
    };

    let mut copied = 0u64;
    while copied < len {
        let remaining = len - copied;
        let result = {
            let src_file = src_side.file(strand)?;
            let dst_file = dst_side.file(strand)?;
            src_file
                .copy_data(dst_file, src_offset + copied, target, Some(remaining), mode)
                .await
                .into_sys(strand)?
        };
        if result.count == 0 {
            break;
        }
        copied += result.count;

        // Advance cursor-addressed sides after every successful bounded step,
        // so a later error leaves visible partial progress.
        if let CopySide::Cursor(borrow) = &mut src_side {
            borrow
                .seek_to(strand, SeekFrom::Start(src_offset + copied))
                .await?;
        }
        if let CopyDest::At(base) = &mut target {
            *base += result.count;
            if let CopySide::Cursor(borrow) = &mut dst_side {
                borrow.seek_to(strand, SeekFrom::Start(*base)).await?;
            }
        } else if let CopySide::Cursor(borrow) = &mut dst_side
            && let CopyDest::Append = target
        {
            let end = result.destination_end.ok_or_else(|| {
                Error::state_error(strand, "append copy did not report its destination end")
            })?;
            borrow.seek_to(strand, SeekFrom::Start(end)).await?;
        }
    }
    Output::set(strand, out, copied);
    Ok(())
}

/// `fs.BaseFile`, the nominal supertype of both kinds of file. It is never
/// instantiated.
pub(crate) struct BaseFile;

impl<'v> Object<'v> for BaseFile {
    const NAME: &'v str = "BaseFile";
    const MODULE: &'v str = "fs";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Io)
    }
}

/// `fs.File`, a buffered file.
///
/// It keeps its buffer in a [`Shared`] over a clone of the file, so the
/// handles it lends read and write through the same buffer. Operations that
/// are not streaming reads or writes *settle* the buffer first: pending writes
/// are written out, and the file is moved back over unread read-ahead. They
/// then use `file` directly, which never touches the cursor except to hand the
/// file off.
pub(crate) struct File {
    /// The open file, until it is closed.
    file: Option<VfsFile>,
    /// The buffer, over its own clone of `file`, while the file is open.
    buf: Option<Shared<FileBuf>>,
}

/// `fs.RawFile`, an unbuffered file, which reads and writes straight to the
/// file.
pub(crate) struct RawFile {
    /// The open file, until it is closed.
    file: Option<VfsFile>,
}

/// A native file object: `fs.File` or `fs.RawFile`, by its kind `T`.
pub(crate) struct FileObject<T>(T);

impl<T> Deref for FileObject<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for FileObject<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

pub(crate) struct FileAnnex<'v> {
    global: State<'v, FsGlobal<'v>>,
    /// Whether the file was opened for appending, in which case every write
    /// lands at the end and an explicit offset cannot be honored.
    is_append: bool,
    /// What the file offers as a byte I/O handle, fixed by the open mode.
    caps: IoCaps,
}

/// Capabilities of a file opened with `mode`.
///
/// An append-mode file is not seekable as a handle: its writes land at the end
/// whatever the position.
fn mode_caps(mode: &str) -> IoCaps {
    let mut caps = if mode.starts_with('r') {
        IoCaps::READ
    } else {
        IoCaps::WRITE
    };
    if mode.contains('+') {
        caps |= IoCaps::READ | IoCaps::WRITE;
    }
    if !mode.starts_with('a') {
        caps |= IoCaps::SEEK;
    }
    caps
}

/// What `fs.File` and `fs.RawFile` do differently. [`FileObject`] implements
/// everything they share once, over this.
///
/// The streaming primitives work in terms of [`io::Result`]: through the
/// buffer of a buffered file, and straight to the file otherwise. Their callers
/// check that the file is open.
pub(crate) trait FileKind<'v>: Sized + 'v {
    const NAME: &'v str;

    fn new(file: VfsFile) -> Self;

    /// The open file, until it is closed.
    fn file(&self) -> Option<&VfsFile>;

    fn file_slot(&mut self) -> &mut Option<VfsFile>;

    /// Writes out pending writes, for operations that see the file's contents
    /// but not its position.
    async fn flush_pending<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()>;

    /// Settles the file, for operations that change its contents or after
    /// which its read-ahead would be stale.
    async fn drop_buffers<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()>;

    /// Writes out pending writes.
    async fn flush_buf(&mut self) -> io::Result<()>;

    /// Leaves the file at the logical position, with nothing buffered.
    async fn settle(&mut self) -> io::Result<()>;

    /// Writes all of `data`, taken together.
    async fn write_all(&mut self, data: &mut [IoSlice<'_>]) -> io::Result<()>;

    async fn seek(&mut self, pos: SeekFrom) -> io::Result<u64>;

    /// The logical position.
    async fn tell(&mut self) -> io::Result<u64>;

    /// Reads `size` bytes from the cursor into the empty `out`, or the rest of
    /// the file, stopping short only at the end of the file.
    async fn read_into(
        &mut self,
        strand: &mut Strand<'v, '_>,
        out: &mut BinEmbryo<'v>,
        size: Option<usize>,
    ) -> io::Result<()>;

    /// Writes out and drops any buffer, once the file has been taken to be
    /// closed.
    async fn close_buf(&mut self) -> io::Result<()>;

    /// The handle the file lends as [`ByteIo`].
    fn byte_handle(&self, caps: IoCaps) -> ByteHandle;

    /// Iterates over the file.
    async fn next<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool>;

    /// Registers the methods only this kind of file has.
    fn build<'a>(
        builder: TypeBuilder<'v, 'a, FileObject<Self>>,
    ) -> TypeBuilder<'v, 'a, FileObject<Self>> {
        builder
    }

    fn open_ref<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, &VfsFile> {
        self.file()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))
    }

    fn check_open<'s>(&self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        self.open_ref(strand).map(|_| ())
    }

    fn vfs(&mut self) -> &mut VfsFile {
        self.file_slot().as_mut().expect("file is closed")
    }
}

impl<'v> FileKind<'v> for File {
    const NAME: &'v str = "File";

    fn new(file: VfsFile) -> Self {
        File {
            buf: Some(Shared::new(FileBuf::new(file.clone()))),
            file: Some(file),
        }
    }

    fn file(&self) -> Option<&VfsFile> {
        self.file.as_ref()
    }

    fn file_slot(&mut self) -> &mut Option<VfsFile> {
        &mut self.file
    }

    /// It holds the buffer without waiting under a shared borrow, so
    /// positional operations on a file with nothing to write out can still
    /// share it. It borrows exclusively when there are writes, or when a lent
    /// handle is using the buffer.
    async fn flush_pending<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        {
            let borrow = this.borrow(strand)?;
            let Some(buf) = &borrow.buf else {
                return Ok(());
            };
            if buf.try_lock().is_some_and(|held| held.pending() == 0) {
                return Ok(());
            }
        }
        this.borrow_mut(strand)?.flush_buf().await.into_sys(strand)
    }

    /// Read-ahead is always dropped. As with [`FileKind::flush_pending`], the
    /// borrow is exclusive only when there are writes to write out or the
    /// buffer is busy.
    async fn drop_buffers<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        {
            let borrow = this.borrow(strand)?;
            let Some(buf) = &borrow.buf else {
                return Ok(());
            };
            if let Some(mut held) = buf.try_lock()
                && held.pending() == 0
            {
                return held.unread_back().await.into_sys(strand);
            }
        }
        this.borrow_mut(strand)?.settle().await.into_sys(strand)
    }

    async fn flush_buf(&mut self) -> io::Result<()> {
        self.buffer().await?.flush_buf().await
    }

    async fn settle(&mut self) -> io::Result<()> {
        self.buffer().await?.settle().await
    }

    async fn write_all(&mut self, data: &mut [IoSlice<'_>]) -> io::Result<()> {
        self.buffer().await?.write_all(data).await
    }

    async fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.buffer().await?.seek(pos).await
    }

    async fn tell(&mut self) -> io::Result<u64> {
        self.buffer().await?.tell().await
    }

    async fn read_into(
        &mut self,
        strand: &mut Strand<'v, '_>,
        out: &mut BinEmbryo<'v>,
        size: Option<usize>,
    ) -> io::Result<()> {
        let mut held = self.buffer().await?;
        let res = held.read_into(strand, out, size).await;
        if res.is_err() {
            held.push_back(out.as_slice());
        }
        res
    }

    async fn close_buf(&mut self) -> io::Result<()> {
        match self.buf.take() {
            Some(mut buf) => match buf.close().await {
                Some(mut buf) => buf.flush_buf().await,
                None => Ok(()),
            },
            None => Ok(()),
        }
    }

    /// The buffer, so the consumer reads and writes through it.
    fn byte_handle(&self, caps: IoCaps) -> ByteHandle {
        let buf = self.buf.as_ref().expect("file is closed");
        ByteHandle::with_caps(buf.clone(), caps)
    }

    /// Lines, as `read_line` reads them.
    async fn next<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
        mut out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        borrow.read_line(strand, Slot::reborrow(&mut out)).await?;
        Ok(out.as_str(strand).is_some_and(|line| !line.is_empty()))
    }

    fn build<'a>(
        builder: TypeBuilder<'v, 'a, FileObject<Self>>,
    ) -> TypeBuilder<'v, 'a, FileObject<Self>> {
        builder
            .method("read_str", async move |this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size = size_arg(strand, size)?;
                this.borrow_mut(strand)?.read_str(strand, size, out).await
            })
            .method("read_line", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                this.borrow_mut(strand)?.read_line(strand, out).await
            })
            .method("lines", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                Output::set(strand, out, this);
                Ok(())
            })
            .method("flush", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check_open(strand)?;
                borrow
                    .buffer()
                    .await
                    .into_sys(strand)?
                    .flush()
                    .await
                    .into_sys(strand)
            })
    }
}

impl<'v> FileKind<'v> for RawFile {
    const NAME: &'v str = "RawFile";

    fn new(file: VfsFile) -> Self {
        RawFile { file: Some(file) }
    }

    fn file(&self) -> Option<&VfsFile> {
        self.file.as_ref()
    }

    fn file_slot(&mut self) -> &mut Option<VfsFile> {
        &mut self.file
    }

    async fn flush_pending<'s>(
        _this: Instance<'v, '_, FileObject<Self>>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        Ok(())
    }

    async fn drop_buffers<'s>(
        _this: Instance<'v, '_, FileObject<Self>>,
        _strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, ()> {
        Ok(())
    }

    async fn flush_buf(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn settle(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn write_all(&mut self, mut data: &mut [IoSlice<'_>]) -> io::Result<()> {
        IoSlice::advance_slices(&mut data, 0);
        while !data.is_empty() {
            let n = self.vfs().write_vectored(data).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            IoSlice::advance_slices(&mut data, n);
        }
        Ok(())
    }

    async fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.vfs().seek(pos).await
    }

    async fn tell(&mut self) -> io::Result<u64> {
        self.vfs().stream_position().await
    }

    async fn read_into(
        &mut self,
        strand: &mut Strand<'v, '_>,
        out: &mut BinEmbryo<'v>,
        size: Option<usize>,
    ) -> io::Result<()> {
        read_raw(self.vfs(), strand, out, size).await
    }

    async fn close_buf(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn byte_handle(&self, caps: IoCaps) -> ByteHandle {
        let file = self.file.as_ref().expect("file is closed");
        ByteHandle::with_caps(file.clone().compat(), caps)
    }

    /// Chunks, as one read returns them.
    async fn next<'s>(
        this: Instance<'v, '_, FileObject<Self>>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, bool> {
        let mut borrow = this.borrow_mut(strand)?;
        borrow.check_open(strand)?;
        let mut buf = BinEmbryo::new_with_capacity(strand, BYTE_STREAM_CHUNK_SIZE);
        let read = read_into_spare(borrow.vfs(), buf.spare_capacity_mut())
            .await
            .into_sys(strand)?;
        if read == 0 {
            return Ok(false);
        }
        unsafe { buf.advance(read) };
        buf.finish(strand, out);
        Ok(true)
    }
}

/// Lends a file as [`ByteIo`], with the capabilities its open mode fixed.
struct FileIo<'a, T> {
    file: &'a T,
    caps: IoCaps,
}

impl<'v, T: FileKind<'v>> IntoByteIo<'v> for FileIo<'_, T> {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(self.caps)
    }

    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        Ok(self.file.byte_handle(self.caps))
    }
}

pub(crate) async fn open<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    path: vfs_path::Path<'_>,
    mode: &str,
) -> Result<'v, 's, VfsFile> {
    let path = super::prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let mut opts = vfs.open_options();
    configure_options(&mut opts, mode);
    opts.open(path.to_path()).await.into_sys(strand)
}

pub(crate) async fn open_native<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
    path: vfs_path::Path<'_>,
    mode: &str,
) -> io::Result<VfsFile> {
    let local = global.local.get(strand);
    let path = local.cwd().join(path.as_str());
    let vfs = local.vfs();
    let mut opts = vfs.open_options();
    configure_options(&mut opts, mode);
    opts.open(path.to_path())
        .await
        .map_err(dolang_vfs::error::Error::into_io_error)
}

/// Implements `fs.open`: returns a buffered `File`, or a `RawFile` with
/// `buffer: false`.
pub(crate) async fn open_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    path: vfs_path::Path<'_>,
    opt1: Option<Slot<'v, '_>>,
    opt2: Option<Slot<'v, '_>>,
    buffer: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    // Determine mode and block
    let (mode, block) = match (&opt1, &opt2) {
        (None, None) => ("r".to_string(), None),
        (Some(slot), None) => {
            // Single arg: check if it's a mode string or block callable
            if let Some(mode) = slot.as_str(strand) {
                (mode.to_string(), None)
            } else {
                ("r".to_string(), Some(slot))
            }
        }

        (Some(slot1), Some(slot2)) => {
            // Two args: first must be mode, second is block
            let mode = slot1
                .as_str(strand)
                .ok_or_else(|| Error::type_error(strand, "mode must be a string"))?
                .to_string();
            (mode, Some(slot2))
        }
        (None, Some(_)) => unreachable!(),
    };

    if !MODES.contains(&mode.as_str()) {
        let message = match mode.strip_suffix('b') {
            Some(base) if MODES.contains(&base) => {
                format!("invalid mode: {mode}; for an unbuffered file, pass `buffer: false`")
            }
            _ => format!("invalid mode: {mode}"),
        };
        return Err(Error::value(strand, message));
    }
    let buffered = match buffer {
        None => true,
        Some(buffer) => buffer
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "buffer must be a Bool"))?,
    };

    let file = open(strand, global, path, &mode).await?;

    if let Some(block) = block {
        strand
            .with_slots(async move |strand, [mut handle, mut tmp]| {
                create_file(strand, global, file, &mode, buffered, &mut handle);
                let result = call!(strand, block, out, &handle).await;
                // Always closed, even on error. A close error matters only
                // when the block succeeded: it may be a buffered write failing
                // late.
                let closed = method!(strand, &handle, global.syms.close, &mut tmp).await;
                result.and(closed)
            })
            .await
    } else {
        create_file(strand, global, file, &mode, buffered, out);
        Ok(())
    }
}

fn invalid_utf8<'v, 's>(strand: &mut Strand<'v, 's>) -> Error<'v, 's> {
    Error::runtime(strand, "invalid UTF-8 data")
}

/// Parses an optional byte count.
fn size_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    size: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<usize>> {
    size.map(|size| {
        size.to_i64(strand)
            .ok()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Error::type_error(strand, "size must be a non-negative integer"))
    })
    .transpose()
}

/// Appends `n` bytes read straight from `file` to `out`, or the rest of the
/// file when `n` is `None`, stopping short only at the end of the file.
async fn read_raw<'v>(
    file: &mut VfsFile,
    strand: &mut Strand<'v, '_>,
    out: &mut BinEmbryo<'v>,
    n: Option<usize>,
) -> io::Result<()> {
    let start = out.len();
    loop {
        let want = match n {
            Some(n) => n - (out.len() - start),
            None => usize::MAX,
        };
        if want == 0 || read_chunk(file, strand, out, want).await? == 0 {
            return Ok(());
        }
    }
}

impl File {
    /// Holds the buffer, after any operation through a lent handle.
    async fn buffer(&mut self) -> io::Result<Locked<'_, FileBuf>> {
        self.buf.as_mut().expect("file is closed").lock().await
    }

    /// Reads a line as a `Str`, or `""` at the end of the file.
    async fn read_line<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.check_open(strand)?;
        let mut held = self.buffer().await.into_sys(strand)?;
        let mut buf = BinEmbryo::new();
        if let Err(err) = held.read_line_into(strand, &mut buf).await {
            held.push_back(buf.as_slice());
            return Err(err).into_sys(strand);
        }
        if std::str::from_utf8(buf.as_slice()).is_err() {
            held.push_back(buf.as_slice());
            return Err(invalid_utf8(strand));
        }
        // SAFETY: validated above.
        unsafe { buf.finish_str_unchecked(strand, out) };
        Ok(())
    }

    /// Reads up to `size` bytes, or the rest of the file, as a `Str`.
    ///
    /// A character split by `size` is left for the next read. When `size` is
    /// too small for the first character, the whole character is read.
    async fn read_str<'v, 's>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        size: Option<usize>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.check_open(strand)?;
        let mut held = self.buffer().await.into_sys(strand)?;
        let mut buf = BinEmbryo::new();
        if let Err(err) = held.read_into(strand, &mut buf, size).await {
            held.push_back(buf.as_slice());
            return Err(err).into_sys(strand);
        }
        loop {
            let eof = size.is_none_or(|size| buf.len() < size);
            let valid = match utf8::complete_prefix(buf.as_slice()) {
                Ok(text) if text.len() == buf.len() => break,
                Ok(text) if !eof => text.len(),
                _ => {
                    held.push_back(buf.as_slice());
                    return Err(invalid_utf8(strand));
                }
            };
            if valid > 0 {
                held.push_back(&buf.as_slice()[valid..]);
                buf.truncate(valid);
                break;
            }
            // The first character is incomplete, so read on until it is whole.
            let before = buf.len();
            if let Err(err) = held.read_into(strand, &mut buf, Some(1)).await {
                held.push_back(buf.as_slice());
                return Err(err).into_sys(strand);
            }
            if buf.len() == before {
                held.push_back(buf.as_slice());
                return Err(invalid_utf8(strand));
            }
        }
        // SAFETY: validated above.
        unsafe { buf.finish_str_unchecked(strand, out) };
        Ok(())
    }
}

impl<'v, T: FileKind<'v>> FileObject<T> {
    /// Hands this file to a child process as one of its standard streams.
    ///
    /// The child gets a clone starting at the logical position, with nothing
    /// held back in a buffer, and the file stays usable here. The clone shares
    /// the cursor, which the VFS moves to where the child left off once it
    /// exits, and a buffer reads and writes from the cursor.
    async fn command_file<'s>(
        this: Instance<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, VfsFile> {
        let mut borrow = this.borrow_mut(strand)?;
        borrow.check_open(strand)?;
        borrow.settle().await.into_sys(strand)?;
        Ok(borrow.vfs().clone())
    }

    /// Capabilities while open; a closed file has none.
    fn open_caps<'s>(
        this: Instance<'v, '_, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, IoCaps> {
        let open = this.borrow(strand)?.file().is_some();
        Ok(if open {
            this.annex().caps
        } else {
            IoCaps::empty()
        })
    }

    /// Reads at an explicit offset, leaving the cursor alone.
    async fn read_at<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        offset: u64,
        size: Option<usize>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file = self.open_ref(strand)?;
        let mut buf = BinEmbryo::new_with_capacity(strand, size.unwrap_or(0));
        read_at_looping(strand, file, &mut buf, offset, size).await?;
        buf.finish(strand, out);
        Ok(())
    }

    /// Writes at an explicit offset, leaving the cursor alone.
    ///
    /// The pieces are written one after another, at consecutive offsets.
    async fn write_at<'s>(
        &self,
        pieces: &[Pinned<'v, '_>],
        offset: u64,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        // Pinned and written from where they lie rather than copied into a
        // `Bytes` first: the pin holds the address stable across the write, and
        // the pieces are rooted argument slots, so they cannot go away
        // underneath it. A pin costs nothing today; against a moving collector
        // this would be worth deciding by payload size, since it trades a
        // memcpy for holding the address down across a whole transfer.
        let file = self.open_ref(strand)?;
        let mut written = 0;
        for piece in pieces {
            written += write_at_looping(file, piece.as_slice(), offset + written as u64)
                .await
                .into_sys(strand)?;
        }
        Output::set(strand, out, written);
        Ok(())
    }

    async fn stream_position<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, u64> {
        self.check_open(strand)?;
        self.tell().await.into_sys(strand)
    }

    async fn seek_to<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        seek_from: SeekFrom,
    ) -> Result<'v, 's, u64> {
        self.check_open(strand)?;
        self.seek(seek_from).await.into_sys(strand)
    }

    /// Reads `size` bytes, or the rest of the file, as a `Bin`.
    async fn read<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        size: Option<usize>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.check_open(strand)?;
        let mut buf = BinEmbryo::new();
        self.read_into(strand, &mut buf, size)
            .await
            .into_sys(strand)?;
        buf.finish(strand, out);
        Ok(())
    }

    async fn write<'s>(
        &mut self,
        pieces: &[Pinned<'v, '_>],
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.check_open(strand)?;
        let mut slices = pieces
            .iter()
            .map(|piece| IoSlice::new(piece.as_slice()))
            .collect::<Vec<_>>();
        let len = pieces
            .iter()
            .map(|piece| piece.as_slice().len())
            .sum::<usize>();
        self.write_all(&mut slices).await.into_sys(strand)?;
        Output::set(strand, out, len);
        Ok(())
    }

    /// Flushes and closes the file, dropping it even if flushing fails.
    ///
    /// Lent handles read as closed afterward.
    async fn close<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, ()> {
        let Some(file) = self.file_slot().take() else {
            return Ok(());
        };
        let flushed = self.close_buf().await;
        let closed = file.close().await;
        flushed.into_sys(strand)?;
        closed.into_sys(strand)
    }

    async fn metadata<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, FsGlobal<'v>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let metadata = self.open_ref(strand)?.metadata().await.into_sys(strand)?;
        create_metadata(strand, global, metadata, out);
        Ok(())
    }

    async fn fs_metadata<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, FsGlobal<'v>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let metadata = self
            .open_ref(strand)?
            .fs_metadata()
            .await
            .into_sys(strand)?;
        create_fs_metadata(strand, global, metadata, out);
        Ok(())
    }
}

impl<'v, T: FileKind<'v>> Object<'v> for FileObject<T> {
    const NAME: &'v str = T::NAME;
    const MODULE: &'v str = "fs";
    type Annex = FileAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        T::next(this, strand, out).await
    }

    async fn sink<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        Output::set(strand, out, this);
        Ok(())
    }

    async fn put<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        value: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let bytes = encode_value(strand, &value)?;
        let mut borrow = this.borrow_mut(strand)?;
        borrow.check_open(strand)?;
        borrow
            .write_all(&mut [IoSlice::new(&bytes)])
            .await
            .into_sys(strand)
    }

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let caps = this.annex().caps;
            let borrow = this.borrow_mut(request)?;
            if borrow.file().is_none() {
                return Err(Error::state_error(request, "file is closed"));
            }
            request.provide::<ByteIo>(&mut FileIo {
                file: &**borrow,
                caps,
            })?;
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let start_sym = builder.sym("start");
        let end_sym = builder.sym("end");
        let namespace = builder.sym("namespace");
        let any = builder.sym("ANY");
        let namespace_user = builder.sym("USER");
        let namespace_system = builder.sym("SYSTEM");
        let owner = builder.sym("owner");
        let group = builder.sym("group");
        let dacl = builder.sym("dacl");
        let sacl = builder.sym("sacl");
        let default_acl = builder.sym("default");
        let kind_acl = builder.sym("kind");
        let shared = builder.sym("shared");
        let offset_sym = builder.sym("offset");
        let data_sym = builder.sym("data");
        let range_sym = builder.sym("range");
        let size_sym = builder.sym("size");
        let clone_sym = builder.sym("clone");
        let builder = builder
            .supertype(TypeObject::Io)
            .supertype(TypeObject::Iter);
        let builder = builder
            .get("readable", |this, strand, out| {
                let caps = Self::open_caps(this, strand)?;
                Output::set(strand, out, caps.contains(IoCaps::READ));
                Ok(())
            })
            .get("writable", |this, strand, out| {
                let caps = Self::open_caps(this, strand)?;
                Output::set(strand, out, caps.contains(IoCaps::WRITE));
                Ok(())
            })
            .get("seekable", |this, strand, out| {
                let caps = Self::open_caps(this, strand)?;
                Output::set(strand, out, caps.contains(IoCaps::SEEK));
                Ok(())
            })
            .method("close", async move |this, strand, _args, _out| {
                this.borrow_mut(strand)?.close(strand).await
            })
            .method("lock", async move |this, strand, args, out| {
                let ([range, block], [shared_value]) = unpack!(strand, args, 2, 0, shared = None)?;
                let shared = shared_value
                    .map(|value| {
                        value
                            .as_bool(strand)
                            .ok_or_else(|| Error::type_error(strand, "shared must be a boolean"))
                    })
                    .transpose()?
                    .unwrap_or(false);
                T::drop_buffers(this, strand).await?;
                strand
                    .with_slots(async move |strand, [mut guard, start, end, step]| {
                        let range = lock_range(strand, &range, [start, end, step])?;
                        let mode = if shared {
                            FileLockMode::Shared
                        } else {
                            FileLockMode::Exclusive
                        };
                        let lock = {
                            let borrow = this.borrow(strand)?;
                            let file = borrow.open_ref(strand)?;
                            file.lock(range, mode, FileLockBehavior::Blocking)
                                .await
                                .into_sys(strand)?
                        };
                        let lock = lock.expect("blocking file lock did not acquire");
                        let lock_type = this.annex().global.types.file_lock;
                        FileLockObject::create(strand, lock_type, Some(lock), &mut guard);
                        let result = call!(strand, block, out, &guard).await;
                        let cleanup = strand
                            .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                                lock_type
                                    .cast(&guard)
                                    .unwrap()
                                    .enter(strand, async move |strand, guard| {
                                        FileLockObject::release(guard, strand).await
                                    })
                                    .await
                            })
                            .await;
                        match (result, cleanup) {
                            (Ok(()), Ok(())) => Ok(()),
                            (Err(error), Ok(())) => Err(error),
                            (Ok(()), Err(error)) => Err(error),
                            (Err(cause), Err(error)) => Err(error.caused_by(strand, cause)),
                        }
                    })
                    .await
            })
            .method("try_lock", async move |this, strand, args, out| {
                let ([range, block], [shared_value]) = unpack!(strand, args, 2, 0, shared = None)?;
                let shared = shared_value
                    .map(|value| {
                        value
                            .as_bool(strand)
                            .ok_or_else(|| Error::type_error(strand, "shared must be a boolean"))
                    })
                    .transpose()?
                    .unwrap_or(false);
                T::drop_buffers(this, strand).await?;
                strand
                    .with_slots(async move |strand, [mut guard, start, end, step]| {
                        let range = lock_range(strand, &range, [start, end, step])?;
                        let mode = if shared {
                            FileLockMode::Shared
                        } else {
                            FileLockMode::Exclusive
                        };
                        let lock = {
                            let borrow = this.borrow(strand)?;
                            let file = borrow.open_ref(strand)?;
                            file.lock(range, mode, FileLockBehavior::Try)
                                .await
                                .into_sys(strand)?
                        };
                        let lock_type = this.annex().global.types.file_lock;
                        FileLockObject::create(strand, lock_type, lock, &mut guard);
                        let result = call!(strand, block, out, &guard).await;
                        let cleanup = strand
                            .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                                lock_type
                                    .cast(&guard)
                                    .unwrap()
                                    .enter(strand, async move |strand, guard| {
                                        FileLockObject::release(guard, strand).await
                                    })
                                    .await
                            })
                            .await;
                        match (result, cleanup) {
                            (Ok(()), Ok(())) => Ok(()),
                            (Err(error), Ok(())) => Err(error),
                            (Ok(()), Err(error)) => Err(error),
                            (Err(cause), Err(error)) => Err(error.caused_by(strand, cause)),
                        }
                    })
                    .await
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [size, offset]) = unpack!(strand, args, 0, 1, offset_sym = None)?;
                let size = size_arg(strand, size)?;
                let offset = read_offset(strand, offset)?;

                // An explicit offset is a disjoint path: it neither consults
                // nor disturbs the cursor. So it takes
                // a *shared* borrow, and any number of positional operations
                // can be in flight on one handle at once — taking the
                // exclusive borrow here would serialize them for no reason,
                // and make two concurrent reads a borrow error rather than
                // two reads.
                if let Some(offset) = offset {
                    T::flush_pending(this, strand).await?;
                    let borrow = this.borrow(strand)?;
                    return borrow.read_at(strand, offset, size, out).await;
                }
                this.borrow_mut(strand)?.read(strand, size, out).await
            })
            .method("write", async move |this, strand, args, out| {
                let (pieces, offset) = write_args(strand, args, offset_sym)?;
                let offset = read_offset(strand, offset)?;
                match offset {
                    // An append handle writes at the end no matter what offset
                    // the platform is given, so honoring one is impossible
                    // rather than merely unimplemented.
                    Some(_) if this.annex().is_append => Err(Error::state_error(
                        strand,
                        "cannot write at an offset on a file opened for appending",
                    )),
                    // Shared borrow, for the reason given on `read`.
                    Some(offset) => {
                        T::drop_buffers(this, strand).await?;
                        let borrow = this.borrow(strand)?;
                        borrow.write_at(&pieces, offset, strand, out).await
                    }
                    None => {
                        let mut borrow = this.borrow_mut(strand)?;
                        borrow.write(&pieces, strand, out).await
                    }
                }
            })
            .method("copy_data", async move |this, strand, args, out| {
                let ([dst], [range, size, offset, clone]) = unpack!(
                    strand,
                    args,
                    1,
                    0,
                    range_sym = None,
                    size_sym = None,
                    offset_sym = None,
                    clone_sym = None
                )?;
                let global = this.annex().global;
                let dst = cast_file(global, &dst)
                    .ok_or_else(|| Error::type_error(strand, "expected fs.BaseFile"))?;
                copy_to(strand, global, this, dst, range, size, offset, clone, out).await
            })
            .method("set_size", async move |this, strand, args, _out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size.to_i64(strand).map_err(|_| {
                    Error::type_error(strand, "size must be a non-negative integer")
                })?;
                let size = u64::try_from(size).map_err(|_| {
                    Error::type_error(strand, "size must be a non-negative integer")
                })?;

                let mut borrow = this.borrow_mut(strand)?;
                borrow.check_open(strand)?;
                // Read-ahead could hold bytes the new size cuts off.
                borrow.settle().await.into_sys(strand)?;
                borrow.vfs().set_size(size).await.into_sys(strand)
            })
            .method("sync", async move |this, strand, args, _out| {
                let ([], [data]) = unpack!(strand, args, 0, 0, data_sym = None)?;
                let data = data
                    .map(|data| crate::util::bool(strand, data, "data"))
                    .transpose()?
                    .unwrap_or(false);
                T::flush_pending(this, strand).await?;
                let borrow = this.borrow(strand)?;
                let file = borrow.open_ref(strand)?;
                file.sync(data).await.into_sys(strand)?;
                Ok(())
            })
            .method("metadata", async move |this, strand, _args, out| {
                T::flush_pending(this, strand).await?;
                this.borrow(strand)?
                    .metadata(strand, this.annex().global, out)
                    .await
            })
            .method("fs_metadata", async move |this, strand, _args, out| {
                T::flush_pending(this, strand).await?;
                this.borrow(strand)?
                    .fs_metadata(strand, this.annex().global, out)
                    .await
            })
            .method("sec_desc", async move |this, strand, args, mut out| {
                let ([], [owner, group, dacl, sacl]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    owner = None,
                    group = None,
                    dacl = None,
                    sacl = None
                )?;
                let mask = super::sec_desc_mask(strand, owner, group, dacl, sacl)?;
                let descriptor = {
                    let borrow = this.borrow(strand)?;
                    let file = borrow.open_ref(strand)?;
                    file.sec_desc(mask).await.into_sys(strand)?
                };
                let windows = strand.force_state::<WindowsSecurityTag>();
                crate::security::create_sec_desc(strand, windows, descriptor, &mut out);
                Ok(())
            })
            .method("acl", async move |this, strand, args, mut out| {
                let ([], [kind, default]) =
                    unpack!(strand, args, 0, 0, kind_acl = None, default_acl = None)?;
                let kind = crate::security::acl_kind_sym(strand, kind)?;
                let default = super::acl_default(strand, default.as_deref())?;
                super::check_acl_default(strand, kind, default)?;
                let acl = {
                    let borrow = this.borrow(strand)?;
                    let file = borrow.open_ref(strand)?;
                    file.acl(kind, default).await.into_sys(strand)?
                };
                crate::security::create_any_acl(strand, acl, &mut out);
                Ok(())
            })
            .method("set_acl", async move |this, strand, args, _out| {
                let ([acl_value], [kind, default]) =
                    unpack!(strand, args, 1, 0, kind_acl = None, default_acl = None)?;
                let (kind, acl) = crate::security::resolve_acl_input(
                    strand,
                    &acl_value,
                    kind,
                    &crate::security::SpecPath::root("File.set_acl.acl"),
                )
                .await?;
                let default = super::acl_default(strand, default.as_deref())?;
                super::check_acl_default(strand, kind, default)?;
                let borrow = this.borrow(strand)?;
                let file = borrow.open_ref(strand)?;
                file.set_acl(kind, acl.as_ref(), default)
                    .await
                    .into_sys(strand)
            })
            .method("update_sec_desc", async move |this, strand, args, _out| {
                let windows = strand.force_state::<WindowsSecurityTag>();
                let descriptor = crate::security::sec_desc_from_args(
                    strand,
                    windows,
                    args,
                    &crate::security::SpecPath::root("update_sec_desc"),
                )
                .await?;
                let borrow = this.borrow(strand)?;
                let file = borrow.open_ref(strand)?;
                file.update_sec_desc(&descriptor).await.into_sys(strand)
            })
            .method("xattrs", async move |this, strand, args, out| {
                let ([], [namespace]) = unpack!(strand, args, 0, 0, namespace = None)?;
                let global = this.annex().global;
                let (namespace, any) = match namespace {
                    None => (None, false),
                    Some(namespace) => {
                        if let Some(sym) = namespace.as_sym(strand) {
                            if sym == any {
                                (None, true)
                            } else if sym == namespace_user {
                                (Some("user".to_owned()), false)
                            } else if sym == namespace_system {
                                (Some("system".to_owned()), false)
                            } else {
                                return Err(Error::value(
                                    strand,
                                    "namespace: expected Str, :ANY:, :USER:, or :SYSTEM:",
                                ));
                            }
                        } else if let Some(namespace) = namespace.as_str(strand) {
                            (Some(namespace.to_string()), false)
                        } else {
                            return Err(Error::type_error(
                                strand,
                                "namespace: expected Str or Sym",
                            ));
                        }
                    }
                };
                let entries = {
                    let borrow = this.borrow(strand)?;
                    let file = borrow.open_ref(strand)?;
                    file.xattrs(if any {
                        dolang_vfs::file::XattrNamespace::Any
                    } else if let Some(ref namespace) = namespace {
                        dolang_vfs::file::XattrNamespace::Named(namespace)
                    } else {
                        dolang_vfs::file::XattrNamespace::Default
                    })
                    .await
                    .into_sys(strand)?
                };
                xattr::create_xattr_iter(strand, global, entries, out)
            })
            .method("xattr", async move |this, strand, args, out| {
                let ([name], [namespace]) = unpack!(strand, args, 1, 0, namespace = None)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, namespace)?;
                let value = {
                    let borrow = this.borrow(strand)?;
                    let file = borrow.open_ref(strand)?;
                    file.xattr(&name, namespace.as_deref())
                        .await
                        .into_sys(strand)?
                };
                Output::set(strand, out, value.as_slice());
                Ok(())
            })
            .method("streams", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let global = this.annex().global;
                let entries = {
                    let borrow = this.borrow(strand)?;
                    let file = borrow.open_ref(strand)?;
                    file.streams().await.into_sys(strand)?
                };
                stream::create_stream_iter(strand, global, entries, out)
            })
            .method("set_xattr", async move |this, strand, args, _out| {
                let ([name, value], [namespace]) = unpack!(strand, args, 2, 0, namespace = None)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, namespace)?;
                let value = util::bytes(strand, &value, "value")?;
                let borrow = this.borrow(strand)?;
                let file = borrow.open_ref(strand)?;
                file.set_xattr(&name, namespace.as_deref(), &value)
                    .await
                    .into_sys(strand)
            })
            .method("remove_xattr", async move |this, strand, args, _out| {
                let ([name], [namespace]) = unpack!(strand, args, 1, 0, namespace = None)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, namespace)?;
                let borrow = this.borrow(strand)?;
                let file = borrow.open_ref(strand)?;
                file.remove_xattr(&name, namespace.as_deref())
                    .await
                    .into_sys(strand)
            })
            .method("tell", async move |this, strand, _args, out| {
                let mut borrow = this.borrow_mut(strand)?;
                borrow.check_open(strand)?;
                // Appended writes land at the end, which only writing them
                // out reveals.
                if this.annex().is_append {
                    borrow.flush_buf().await.into_sys(strand)?;
                }
                let pos = borrow.tell().await.into_sys(strand)?;
                Output::set(strand, out, i128::from(pos));
                Ok(())
            })
            .method("seek", async move |this, strand, args, out| {
                let ([], [offset, start, end]) =
                    unpack!(strand, args, 0, 1, start_sym = None, end_sym = None)?;
                let mut borrow = this.borrow_mut(strand)?;
                let seek_from = match (offset, start, end) {
                    (Some(offset), None, None) => {
                        let offset = offset.to_i64(strand).map_err(|_| {
                            Error::type_error(strand, "seek offset must be an integer")
                        })?;
                        SeekFrom::Current(offset)
                    }
                    (None, Some(start), None) => {
                        let start = start
                            .to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "start must be an integer"))?;
                        SeekFrom::Start(u64::try_from(start).map_err(|_| {
                            Error::runtime(strand, "start offset must be non-negative")
                        })?)
                    }
                    (None, None, Some(end)) => SeekFrom::End(
                        end.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "end must be an integer"))?,
                    ),
                    (None, None, None) => {
                        return Err(Error::missing_positional(strand, 0));
                    }
                    (Some(_), Some(_), _) => {
                        return Err(Error::unexpected_key(strand, start_sym));
                    }
                    (Some(_), None, Some(_)) | (None, Some(_), Some(_)) => {
                        return Err(Error::unexpected_key(strand, end_sym));
                    }
                };
                borrow.check_open(strand)?;
                let pos = borrow.seek(seek_from).await.into_sys(strand)?;

                Output::set(strand, out, i128::from(pos));
                Ok(())
            });
        T::build(builder)
    }
}

/// Creates a file `fs.open` opened with `mode`, buffered or raw.
pub(crate) fn create_file<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
    file: VfsFile,
    mode: &str,
    buffered: bool,
    out: impl Output<'v>,
) {
    let annex = FileAnnex {
        global,
        is_append: mode.starts_with('a'),
        caps: mode_caps(mode),
    };
    if buffered {
        let ty = global.types.file;
        ty.create_with_annex(strand, FileObject(File::new(file)), annex, out);
    } else {
        let ty = global.types.raw_file;
        ty.create_with_annex(strand, FileObject(RawFile::new(file)), annex, out);
    }
}

/// Native casts match representations, not nominal supertypes. Keep the
/// concrete witness so copies and process handoffs can use either kind of
/// file, including a native delegate held by a Do class.
pub(crate) enum FileCast<'v, 'a> {
    Buffered(Cast<'v, 'a, FileObject<File>>),
    Raw(Cast<'v, 'a, FileObject<RawFile>>),
}

pub(crate) fn cast_file<'v, 'a>(
    global: State<'v, FsGlobal<'v>>,
    value: &'a Value<'v>,
) -> Option<FileCast<'v, 'a>> {
    global
        .types
        .file
        .cast(value)
        .map(FileCast::Buffered)
        .or_else(|| global.types.raw_file.cast(value).map(FileCast::Raw))
}

/// Enters whichever kind of file `$cast` holds, binding the instance as
/// `$file`.
macro_rules! enter_file {
    ($cast:expr, $strand:expr, |$s:ident, $file:ident| $body:expr) => {
        match $cast {
            FileCast::Buffered(cast) => cast.enter($strand, async move |$s, $file| $body).await,
            FileCast::Raw(cast) => cast.enter($strand, async move |$s, $file| $body).await,
        }
    };
}

impl<'v> FileCast<'v, '_> {
    /// See [`FileObject::command_file`].
    pub(crate) async fn command_file<'s>(
        self,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, VfsFile> {
        enter_file!(self, strand, |strand, file| FileObject::command_file(
            file, strand
        )
        .await)
    }
}

#[allow(clippy::too_many_arguments)]
async fn copy_to<'v, 's, S: FileKind<'v>>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    src: Instance<'v, '_, FileObject<S>>,
    dst: FileCast<'v, '_>,
    range: Option<Slot<'v, '_>>,
    size: Option<Slot<'v, '_>>,
    offset: Option<Slot<'v, '_>>,
    clone: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    enter_file!(dst, strand, |strand, dst| copy_data(
        strand, global, src, dst, range, size, offset, clone, out
    )
    .await)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn copy_files<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    src: FileCast<'v, '_>,
    dst: FileCast<'v, '_>,
    range: Option<Slot<'v, '_>>,
    size: Option<Slot<'v, '_>>,
    offset: Option<Slot<'v, '_>>,
    clone: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    enter_file!(src, strand, |strand, src| copy_to(
        strand, global, src, dst, range, size, offset, clone, out
    )
    .await)
}
