//! The representation-agnostic bounded byte-copy step behind
//! [`File::copy_data`], and the file-identity token its overlap check uses.
//!
//! Append copies and relays use the dense step here. Positional local Linux and
//! FreeBSD copies use the platform extent path instead.
//!
//! [`File::copy_data`]: super::File::copy_data

use bytes::{Bytes, BytesMut};

use crate::{
    error::{Error, ErrorKind, Result},
    file::{CopyDataResult, CopyDest, File, FileInner},
};

/// Bytes moved per read/write round of the fallback loop.
///
/// [`STREAM_CHUNK_SIZE`] rather than a size of its own: a client read is
/// already clamped to `MAX_FILE_READ`, which is the same constant, so on the
/// relay route one chunk is exactly one wire round trip with no wasted buffer
/// and no artificially short reads. Locally it stays under the direct
/// backend's `MAX_BLOCKING_IO`, so a chunk is also a single blocking transfer.
pub(crate) const COPY_LIMIT: u64 = 2 * 1024 * 1024;

/// Identifies the file behind a handle, for detecting a copy that would
/// overlap itself.
///
/// Deliberately not derived from [`Metadata`](crate::metadata::Metadata):
/// that carries `dev`/`ino` on Unix but has no file index at all on Windows,
/// where the volume serial lives on the *filesystem* metadata instead. Asking
/// the platform directly costs one call on an already-open handle and works
/// on both.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FileId {
    pub(crate) volume: u64,
    pub(crate) index: u64,
}

/// Returns the identity of the file behind `file`, when it can be determined
/// cheaply.
///
/// `None` means "unknown", not "different": a mixed `direct`/`client` pair has
/// no identity in common to compare, and a platform may decline to report one.
/// Callers must treat an unknown identity as "cannot tell", never as proof
/// that two handles name different files.
pub(crate) async fn identity(file: &File) -> Option<FileId> {
    match &file.inner {
        FileInner::Direct(file) => file.id().await,
        // An opaque handle has no local identity, but two citations of the
        // same gift are trivially the same file. Two *different* gifts may
        // still be one file; that pair reaches the server, where both sides
        // are `direct` and the real check applies.
        FileInner::Client(_) => None,
    }
}

/// Whether `src` and `dst` are the same opaque file on the same session.
pub(crate) fn same_opaque(src: &File, dst: &File) -> bool {
    match (&src.inner, &dst.inner) {
        (FileInner::Client(src), FileInner::Client(dst)) => src.is_same_file(dst),
        _ => false,
    }
}

/// Performs one bounded dense copy step.
pub(crate) async fn copy_chunked(
    src: &File,
    dst: &File,
    src_offset: u64,
    target: CopyDest,
    len: Option<u64>,
) -> Result<CopyDataResult> {
    let want =
        usize::try_from(len.unwrap_or(COPY_LIMIT).min(COPY_LIMIT)).expect("copy limit fits usize");
    if want == 0 {
        return Ok(CopyDataResult {
            count: 0,
            destination_end: None,
        });
    }
    let mut buf = BytesMut::with_capacity(want);
    let read = src
        .read_at_into(&mut buf.spare_capacity_mut()[..want], src_offset)
        .await?;
    if read == 0 {
        return Ok(CopyDataResult {
            count: 0,
            destination_end: None,
        });
    }
    // SAFETY: `read_at_into` initialized the reported prefix.
    unsafe { buf.set_len(read) };
    let mut written = 0usize;
    let mut destination_end = None;
    while written < read {
        let rest = &buf[written..];
        let (count, end) = match target {
            CopyDest::At(base) => (dst.write_at_from(rest, base + written as u64).await?, None),
            CopyDest::Append => {
                let (count, end) = dst.append(Bytes::copy_from_slice(rest)).await?;
                (count, Some(end))
            }
        };
        if count == 0 {
            return Err(Error::new(
                ErrorKind::WriteZero,
                "file copy write made no progress",
            ));
        }
        written += count;
        destination_end = end.or(destination_end);
    }
    Ok(CopyDataResult {
        count: read as u64,
        destination_end,
    })
}
