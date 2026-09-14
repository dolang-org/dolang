use std::{
    io::{self, SeekFrom},
    mem, result, str,
};

use bstr::ByteSlice;
use dolang::runtime::{
    AllocExt, BYTE_STREAM_CHUNK_SIZE, Error, Instance, Object, Output, Result, Slot, State, Strand,
    call, method,
    object::{Mut, Ref, TypeBuilder},
    strand::InterruptMask,
    unpack,
    value::{BinEmbryo, PinBin, PinStr, TypeObject, View},
};
use dolang_vfs::{
    file::{
        CopyDest, CopyMode, File as VfsFile, FileLockBehavior, FileLockMode, FileLockRange,
        OpenOptions,
    },
    process::{StdioRecv, StdioSend},
};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use dolang_vfs::path as vfs_path;

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    fs::{
        file_lock::FileLock as FileLockObject, fs_metadata::create_fs_metadata,
        metadata::create_metadata, read_all, read_into_spare, stream, xattr,
    },
    global::{FsGlobal, WindowsSecurityGlobal},
    io_mode::encode_value,
    util,
};

const TEXT_BUFFER_SIZE: usize = 8192;

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

/// Configure OpenOptions based on mode string (supports 'b' suffix for binary mode).
fn configure_options(opts: &mut OpenOptions, mode: &str) {
    // Strip 'b' suffix for binary mode - it doesn't affect file opening
    match mode.strip_suffix('b').unwrap_or(mode) {
        "r" => {
            opts.read(true);
        }
        "w" => {
            opts.write(true).truncate(true).create(true);
        }
        "a" => {
            opts.write(true).append(true).create(true);
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

fn maximal_utf8_prefix(bytes: &[u8]) -> result::Result<&str, ()> {
    match str::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => {
            if e.error_len().is_none() {
                Ok(unsafe { str::from_utf8_unchecked(&bytes[0..e.valid_up_to()]) })
            } else {
                Err(())
            }
        }
    }
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
enum CopySide<'v, 'a> {
    Positional(Ref<'v, 'a, File<'v>>),
    Cursor(Mut<'v, 'a, File<'v>>),
}

impl<'v> CopySide<'v, '_> {
    fn file<'b, 's>(&'b self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, &'b VfsFile> {
        let file = match self {
            Self::Positional(borrow) => borrow.file.as_ref(),
            Self::Cursor(borrow) => borrow.file.as_ref(),
        };
        file.ok_or_else(|| Error::state_error(strand, "file is closed"))
    }
}

/// Implements `fs.copy_data` and `File.copy_data`.
///
/// Both entry points funnel here so the two cannot disagree about addressing.
/// Everything below this point is absolute: ranges and cursors are resolved
/// into `(offset, destination, length)` before anything reaches the VFS, which
/// knows nothing of either.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn copy_data<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    src: Instance<'v, '_, File<'v>>,
    dst: Instance<'v, '_, File<'v>>,
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
        std::ptr::eq(&*src as *const File<'v>, &*dst as *const File<'v>)
    };
    if same_handle && (region.is_none() || offset.is_none()) {
        return Err(Error::state_error(
            strand,
            "cannot copy data between a file handle and itself through its cursor; \
             give both a range: and an offset:",
        ));
    }

    // Each side is borrowed and resolved together, because resolving a cursor
    // requires the exclusive borrow that addressing through it implies.
    // `logical_position` rather than the platform's own: the read-ahead buffer
    // puts that one ahead of the cursor the script can see.
    let (mut src_side, src_offset, len) = match region {
        Some((start, end)) => (
            CopySide::Positional(src.borrow(strand)?),
            start,
            end.map(|end| end - start),
        ),
        None => {
            let mut borrow = src.borrow_mut(strand)?;
            let position = borrow.logical_position(strand).await?;
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
            let position = borrow.logical_position(strand).await?;
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

/// A handle to an open file.
pub(crate) struct File<'v> {
    file: Option<VfsFile>,
    buf: BinEmbryo<'v>,
}

pub(crate) struct FileAnnex<'v> {
    global: State<'v, FsGlobal<'v>>,
    is_binary: bool,
    /// Whether the file was opened for appending, in which case every write
    /// lands at the end and an explicit offset cannot be honored.
    is_append: bool,
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

impl<'v> File<'v> {
    pub(crate) fn create(
        _strand: &Strand<'v, '_>,
        global: State<'v, FsGlobal<'v>>,
        file: VfsFile,
        mode: &str,
    ) -> (Self, FileAnnex<'v>) {
        (
            File {
                file: Some(file),
                buf: BinEmbryo::new(),
            },
            FileAnnex {
                global,
                is_binary: mode.contains('b'),
                is_append: mode.starts_with('a'),
            },
        )
    }

    /// Hands this file to a child process as one of its standard streams,
    /// giving it up here.
    ///
    /// The handoff *steals* the file: the seek position is kept in this process
    /// rather than in the kernel, so two live handles would each believe a
    /// cursor the other moves. Surrendering the handle makes that unobservable
    /// — subsequent use gives the ordinary "file is closed" error — instead of
    /// quietly reading and writing at the wrong places.
    pub(crate) async fn command_send<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Option<StdioSend>> {
        let mut borrow = this.borrow_mut(strand)?;
        if !borrow.buf.is_empty() {
            return Ok(None);
        }
        let mut file = borrow
            .file
            .take()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        // The endpoint carries a position of its own, and the cursor lives on
        // this side rather than in the kernel, so it has to be stated.
        let offset = match file.stream_position().await {
            Ok(offset) => offset,
            Err(error) => {
                borrow.file = Some(file);
                return Err(error).into_sys(strand);
            }
        };
        let stdio = match file.into_stdio_send(offset).await {
            Ok(stdio) => stdio,
            // Nothing was handed over, so the file is still the script's.
            Err(error) => {
                let (file, error) = error.into_parts();
                borrow.file = Some(file);
                return Err(error).into_sys(strand);
            }
        };
        Ok(Some(stdio))
    }

    /// Receives a child process's output into this file, closing it here.
    ///
    /// Steals the file for the same reason as [`File::command_send`].
    pub(crate) async fn command_recv<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &mut Strand<'v, 's>,
    ) -> Result<'v, 's, Option<StdioRecv>> {
        let mut borrow = this.borrow_mut(strand)?;
        if !borrow.buf.is_empty() {
            return Ok(None);
        }
        let mut file = borrow
            .file
            .take()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        // The endpoint carries a position of its own, and the cursor lives on
        // this side rather than in the kernel, so it has to be stated.
        let offset = match file.stream_position().await {
            Ok(offset) => offset,
            Err(error) => {
                borrow.file = Some(file);
                return Err(error).into_sys(strand);
            }
        };
        let stdio = match file.into_stdio_recv(offset).await {
            Ok(stdio) => stdio,
            Err(error) => {
                let (file, error) = error.into_parts();
                borrow.file = Some(file);
                return Err(error).into_sys(strand);
            }
        };
        Ok(Some(stdio))
    }

    /// Reads at an explicit offset, leaving the cursor and its buffer alone.
    async fn read_at<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        offset: u64,
        size: Option<usize>,
        is_binary: bool,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let mut buf = BinEmbryo::new_with_capacity(strand, size.unwrap_or(0));
        read_at_looping(strand, file, &mut buf, offset, size).await?;
        if is_binary {
            buf.finish(strand, out);
            Ok(())
        } else {
            // There is no cursor to carry a split character forward on, so
            // unlike a streaming text read this cannot stash a remainder.
            buf.finish_str(strand, out)
                .map_err(|_| Error::runtime(strand, "invalid UTF-8 data"))
        }
    }

    /// Writes at an explicit offset, leaving the cursor alone.
    async fn write_at<'a, 's>(
        &self,
        data: Slot<'v, 'a>,
        offset: u64,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        // Pinned and written from where it lies rather than copied into a
        // `Bytes` first: the pin holds the address stable across the write, and
        // `data` is a rooted argument slot, so the value cannot go away
        // underneath it. A pin costs nothing today; against a moving collector
        // this would be worth deciding by payload size, since it trades a
        // memcpy for holding the address down across a whole transfer.
        let pinned = match data.view(strand) {
            View::Str(value) => Pinned::Str(value.pin()),
            View::Bin(value) => Pinned::Bin(value.pin()),
            _ => return Err(Error::type_error(strand, "expected `Str` or `Bin`")),
        };
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let written = write_at_looping(file, pinned.as_slice(), offset)
            .await
            .into_sys(strand)?;
        Output::set(strand, out, written);
        Ok(())
    }

    async fn logical_position<'s>(&mut self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, u64> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let pos = file_ref.stream_position().await.into_sys(strand)?;
        pos.checked_sub(self.buf.len() as u64)
            .ok_or_else(|| Error::runtime(strand, "file cursor is before buffered data"))
    }

    async fn seek_to<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        seek_from: SeekFrom,
    ) -> Result<'v, 's, u64> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let pos = file_ref.seek(seek_from).await.into_sys(strand)?;
        self.buf.truncate(0);
        Ok(pos)
    }

    pub(crate) async fn open<'s>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, FsGlobal<'v>>,
        path: vfs_path::Path<'_>,
        opt1: Option<Slot<'v, '_>>,
        opt2: Option<Slot<'v, '_>>,
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

        // Validate mode string (strip 'b' suffix for validation)
        let base_mode = mode.strip_suffix('b').unwrap_or(&mode);
        match base_mode {
            "r" | "w" | "a" | "r+" | "w+" | "a+" => {}
            _ => {
                return Err(Error::value(strand, format!("invalid mode: {}", mode)));
            }
        }

        let file = open(strand, global, path, &mode).await?;

        if let Some(block) = block {
            strand
                .with_slots(async move |strand, [mut handle, mut tmp]| {
                    // Block scope mode: create handle, call block with auto-close
                    let (file, annex) = File::create(strand, global, file, &mode);
                    global
                        .types
                        .file
                        .create_with_annex(strand, file, annex, &mut handle);

                    // Call the block with the handle as argument
                    let result = call!(strand, block, out, &handle).await;

                    // Always close the file, even on error
                    let _ = method!(strand, &handle, global.syms.close, &mut tmp).await;

                    result
                })
                .await
        } else {
            // No block: just return the handle in the slot
            let (file, annex) = File::create(strand, global, file, &mode);
            global
                .types
                .file
                .create_with_annex(strand, file, annex, out);
            Ok(())
        }
    }

    async fn fill_buf<'s>(&mut self, strand: &mut Strand<'v, 's>, n: usize) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        let buf = &mut self.buf;
        if buf.len() >= n {
            return Ok(());
        }
        if n > buf.capacity() {
            buf.reserve(strand, n - buf.len())
        }
        let read = read_into_spare(file_ref, buf.spare_capacity_mut())
            .await
            .into_sys(strand)?;
        unsafe { buf.advance(read) };
        Ok(())
    }

    async fn read_binary<'s>(
        &mut self,
        n: usize,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        while self.buf.len() < n {
            let remaining = n - self.buf.len();
            if self.buf.spare_capacity_mut().len() < remaining {
                self.buf.reserve(strand, remaining);
            }
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
            let spare = self.buf.spare_capacity_mut();
            let read = read_into_spare(file, &mut spare[..remaining])
                .await
                .into_sys(strand)?;
            if read == 0 {
                break;
            }
            unsafe { self.buf.advance(read) };
        }
        let buf = mem::take(&mut self.buf);
        buf.finish(strand, out);
        Ok(())
    }

    async fn read_binary_all<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = mem::take(&mut self.buf);
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        read_all(strand, file, &mut buf).await?;
        buf.finish(strand, out);
        Ok(())
    }

    async fn read_text<'s>(
        &mut self,
        n: usize,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        self.fill_buf(strand, n).await?;
        match maximal_utf8_prefix(self.buf.as_slice()) {
            Ok(s) => {
                let consumed = s.len();
                let rem = self.buf.len() - consumed;
                let mut buf =
                    mem::replace(&mut self.buf, BinEmbryo::new_with_capacity(strand, rem));
                self.buf.extend(strand, &buf.as_slice()[consumed..]);
                buf.truncate(consumed);
                unsafe { buf.finish_str_unchecked(strand, out) };
                Ok(())
            }
            Err(()) => Err(Error::runtime(strand, "invalid UTF-8 data")),
        }
    }

    async fn read_text_all<'s>(
        &mut self,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let mut buf = mem::take(&mut self.buf);
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        read_all(strand, file, &mut buf).await?;
        buf.finish_str(strand, out)
            .map_err(|_| Error::runtime(strand, "invalid UTF-8 data"))
    }

    async fn write<'a, 's>(
        &mut self,
        data: Slot<'v, 'a>,
        strand: &mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let bytes_written = match data.view(strand) {
            View::Str(s) => {
                let s = s.pin();
                file.write_all(s.as_bytes()).await.map(|_| s.len())
            }
            View::Bin(b) => {
                let b = b.pin();
                file.write_all(&b).await.map(|_| b.len())
            }
            _ => return Err(Error::type_error(strand, "expected `Str` or `Bin`")),
        }
        .into_sys(strand)?;

        Output::set(strand, out, bytes_written);
        Ok(())
    }

    async fn metadata<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, FsGlobal<'v>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let metadata = file_ref.metadata().await.into_sys(strand)?;
        create_metadata(strand, global, metadata, out);
        Ok(())
    }

    async fn fs_metadata<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        global: State<'v, FsGlobal<'v>>,
        out: Slot<'v, '_>,
    ) -> Result<'v, 's, ()> {
        let file_ref = self
            .file
            .as_ref()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

        let metadata = file_ref.fs_metadata().await.into_sys(strand)?;
        create_fs_metadata(strand, global, metadata, out);
        Ok(())
    }
}

impl<'v> Object<'v> for File<'v> {
    const NAME: &'v str = "File";
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
        let mut borrow = this.borrow_mut(strand)?;
        let is_binary = this.annex().is_binary;

        if is_binary {
            // Binary mode: read a chunk of data
            let mut buf = mem::take(&mut borrow.buf);
            buf.reserve(strand, BYTE_STREAM_CHUNK_SIZE.saturating_sub(buf.len()));

            let file_ref = borrow
                .file
                .as_mut()
                .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

            match read_into_spare(file_ref, buf.spare_capacity_mut()).await {
                Ok(0) => {
                    borrow.buf = buf;
                    Ok(false)
                }
                Ok(read) => {
                    unsafe { buf.advance(read) };
                    buf.finish(strand, out);
                    Ok(true)
                }
                Err(e) => {
                    borrow.buf = buf;
                    Err(e.into_sys(strand))
                }
            }
        } else {
            // Text mode: read a line using buffered approach
            // Take ownership of the buffer temporarily
            let mut buf = mem::take(&mut borrow.buf);

            loop {
                // Check if we already have a complete line in the buffer
                if let Some((line, _rest)) = buf.as_slice().split_once_str(b"\n") {
                    // The terminator stays with the line: concatenating what a
                    // file yields has to reproduce the file, `\r\n` included.
                    let line_len = line.len();
                    borrow.buf = BinEmbryo::new_with_capacity(strand, buf.len() - (line_len + 1));
                    borrow.buf.extend(strand, &buf.as_slice()[line_len + 1..]);
                    buf.truncate(line_len + 1);
                    buf.finish_str(strand, out)
                        .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                    return Ok(true);
                }

                // Need to read more data
                buf.reserve(strand, TEXT_BUFFER_SIZE);

                let file_ref = borrow
                    .file
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;

                match read_into_spare(file_ref, buf.spare_capacity_mut()).await {
                    Ok(0) => {
                        // EOF reached
                        if buf.is_empty() {
                            borrow.buf = buf;
                            return Ok(false);
                        } else {
                            buf.finish_str(strand, out)
                                .map_err(|_| Error::runtime(strand, "invalid UTF-8"))?;
                            return Ok(true);
                        }
                    }
                    Ok(read) => {
                        unsafe { buf.advance(read) };
                        // Continue loop to check for newline
                        continue;
                    }
                    Err(e) => {
                        borrow.buf = buf;
                        return Err(e.into_sys(strand));
                    }
                }
            }
        }
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
        let file = borrow
            .file
            .as_mut()
            .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
        file.write_all(&bytes).await.into_sys(strand)?;
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
        builder
            .supertype(TypeObject::Iter)
            .supertype(TypeObject::Sink)
            .method("close", async move |this, strand, _args, _out| {
                let mut borrow = this.borrow_mut(strand)?;
                if let Some(file) = borrow.file.take() {
                    file.close().await.into_sys(strand)?
                }
                Ok(())
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
                            let file = borrow
                                .file
                                .as_ref()
                                .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                            let file = borrow
                                .file
                                .as_ref()
                                .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                let size: Option<usize> = size
                    .map(|s| {
                        s.to_i64(strand)
                            .ok()
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| {
                                Error::type_error(strand, "size must be a non-negative integer")
                            })
                    })
                    .transpose()?;
                let offset = read_offset(strand, offset)?;

                let is_binary = this.annex().is_binary;

                // An explicit offset is a disjoint path: it neither consults
                // nor disturbs the cursor or the buffer behind it. So it takes
                // a *shared* borrow, and any number of positional operations
                // can be in flight on one handle at once — taking the
                // exclusive borrow here would serialize them for no reason,
                // and make two concurrent reads a borrow error rather than
                // two reads.
                if let Some(offset) = offset {
                    let borrow = this.borrow(strand)?;
                    return borrow.read_at(strand, offset, size, is_binary, out).await;
                }
                let mut borrow = this.borrow_mut(strand)?;
                match (is_binary, size) {
                    (true, Some(n)) => borrow.read_binary(n, strand, out).await,
                    (true, None) => borrow.read_binary_all(strand, out).await,
                    (false, Some(n)) => borrow.read_text(n, strand, out).await,
                    (false, None) => borrow.read_text_all(strand, out).await,
                }
            })
            .method("write", async move |this, strand, args, out| {
                let ([data], [offset]) = unpack!(strand, args, 1, 0, offset_sym = None)?;
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
                        let borrow = this.borrow(strand)?;
                        borrow.write_at(data, offset, strand, out).await
                    }
                    None => {
                        let mut borrow = this.borrow_mut(strand)?;
                        borrow.write(data, strand, out).await
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
                let dst = global
                    .types
                    .file
                    .cast(&dst)
                    .ok_or_else(|| Error::type_error(strand, "expected fs.File"))?;
                dst.enter(strand, async move |strand, dst| {
                    copy_data(strand, global, this, dst, range, size, offset, clone, out).await
                })
                .await
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
                let pos = borrow.logical_position(strand).await?;
                {
                    let file = borrow
                        .file
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                    file.set_size(size).await.into_sys(strand)?;
                }
                borrow.seek_to(strand, SeekFrom::Start(pos)).await?;
                Ok(())
            })
            // No flush of our own beforehand: `buf` is read-ahead only, and
            // every write path here goes straight to the file, so there is
            // never buffered data on this side for a sync to miss.
            .method("sync", async move |this, strand, args, _out| {
                let ([], [data]) = unpack!(strand, args, 0, 0, data_sym = None)?;
                let data = data
                    .map(|data| crate::util::bool(strand, data, "data"))
                    .transpose()?
                    .unwrap_or(false);
                let borrow = this.borrow(strand)?;
                let file = borrow
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                file.sync(data).await.into_sys(strand)?;
                Ok(())
            })
            .method("metadata", async move |this, strand, _args, out| {
                this.borrow(strand)?
                    .metadata(strand, this.annex().global, out)
                    .await
            })
            .method("fs_metadata", async move |this, strand, _args, out| {
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
                    let file = borrow
                        .file
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                    file.sec_desc(mask).await.into_sys(strand)?
                };
                let windows = strand.force_state::<WindowsSecurityGlobal<'v>>();
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
                    let file = borrow
                        .file
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                let file = borrow
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                file.set_acl(kind, acl.as_ref(), default)
                    .await
                    .into_sys(strand)
            })
            .method("update_sec_desc", async move |this, strand, args, _out| {
                let windows = strand.force_state::<WindowsSecurityGlobal<'v>>();
                let descriptor = crate::security::sec_desc_from_args(
                    strand,
                    windows,
                    args,
                    &crate::security::SpecPath::root("update_sec_desc"),
                )
                .await?;
                let borrow = this.borrow(strand)?;
                let file = borrow
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                    let file = borrow
                        .file
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                    let file = borrow
                        .file
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                    let file = borrow
                        .file
                        .as_ref()
                        .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
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
                let file = borrow
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                file.set_xattr(&name, namespace.as_deref(), &value)
                    .await
                    .into_sys(strand)
            })
            .method("remove_xattr", async move |this, strand, args, _out| {
                let ([name], [namespace]) = unpack!(strand, args, 1, 0, namespace = None)?;
                let global = this.annex().global;
                let (name, namespace) = xattr::parse_name(strand, global, &name, namespace)?;
                let borrow = this.borrow(strand)?;
                let file = borrow
                    .file
                    .as_ref()
                    .ok_or_else(|| Error::state_error(strand, "file is closed"))?;
                file.remove_xattr(&name, namespace.as_deref())
                    .await
                    .into_sys(strand)
            })
            .method("tell", async move |this, strand, _args, out| {
                let mut borrow = this.borrow_mut(strand)?;
                let pos = borrow.logical_position(strand).await?;
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
                        let buffered = i64::try_from(borrow.buf.len()).map_err(|_| {
                            Error::runtime(strand, "file buffer is too large to seek")
                        })?;
                        SeekFrom::Current(offset - buffered)
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
                let pos = borrow.seek_to(strand, seek_from).await?;

                Output::set(strand, out, i128::from(pos));
                Ok(())
            })
    }
}
