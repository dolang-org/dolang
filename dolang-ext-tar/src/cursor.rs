//! A tar member cursor that owns both its logical reader and the current
//! member frame borrowed from it.

use std::mem::{self, MaybeUninit};

use dolang::runtime::BYTE_STREAM_CHUNK_SIZE;
use dolang_util::alias;
use tar_framing::{FrameError, logical::MemberFrame, logical::TarReader};
use tokio::io::AsyncRead;

/// A logical tar reader and its current member.
///
/// `current` borrows `archive` with its lifetime extended to `'static`. That
/// is sound because:
///
/// 1. `archive` is used only to call `next_frame`, and only while `current` is
///    `None`.
/// 2. No `'static` borrow leaves the cursor: [`Self::frame`] shortens the
///    frame's lifetime to the borrow of the cursor, and the payload is read
///    only through [`Self::read`].
/// 3. `archive` and `current` are each in an [`alias::Box`], so the reader
///    stays in place for the cursor's life, and moving the cursor neither
///    asserts uniqueness over the reader nor retags the references inside the
///    frame. Passing the cursor by value would otherwise protect those
///    references for the call, and dropping the reader during it would
///    violate the protection.
/// 4. `current` is declared before `archive`, so it drops first.
pub(crate) struct Cursor<R: 'static> {
    current: alias::Box<Option<MemberFrame<'static, R>>>,
    chunk: Chunk,
    archive: alias::Box<TarReader<R>>,
}

/// The undelivered part of the last payload chunk read from the current member.
#[derive(Default)]
struct Chunk {
    buf: Vec<u8>,
    offset: usize,
    done: bool,
}

impl<R: AsyncRead + Unpin + 'static> Cursor<R> {
    pub(crate) fn new(archive: TarReader<R>) -> Self {
        Self {
            current: alias::Box::new(None),
            chunk: Chunk::default(),
            archive: alias::Box::new(archive),
        }
    }

    /// The current member, if any.
    pub(crate) fn frame(&self) -> Option<&MemberFrame<'_, R>> {
        (*self.current).as_ref()
    }

    /// Drops the current member.
    pub(crate) fn clear(&mut self) {
        *self.current = None;
        self.chunk = Chunk::default();
    }

    /// Advances to the next member, draining what is left of the current one.
    /// Returns `false` at the end of the archive.
    pub(crate) async fn advance(&mut self) -> Result<bool, FrameError> {
        self.clear();
        let frame = self.archive.next_frame().await?;
        // SAFETY: `current` is `None`, so nothing else borrows `archive`; see
        // the type's invariants.
        *self.current = frame.map(|frame| unsafe {
            mem::transmute::<MemberFrame<'_, R>, MemberFrame<'static, R>>(frame)
        });
        Ok(self.current.is_some())
    }

    /// Reads payload into `buf` until it is full or the member ends; with
    /// `some`, only until it holds any. Returns the number of bytes read,
    /// which is 0 without a current member.
    pub(crate) async fn read(
        &mut self,
        buf: &mut [MaybeUninit<u8>],
        some: bool,
    ) -> Result<usize, FrameError> {
        let Some(frame) = &mut *self.current else {
            return Ok(0);
        };
        let chunk = &mut self.chunk;
        let mut filled = 0;
        while filled < buf.len() {
            if chunk.offset == chunk.buf.len() {
                if chunk.done
                    || !frame
                        .payload
                        .next_chunk(&mut chunk.buf, BYTE_STREAM_CHUNK_SIZE)
                        .await?
                {
                    chunk.done = true;
                    break;
                }
                chunk.offset = 0;
            }
            let count = (buf.len() - filled).min(chunk.buf.len() - chunk.offset);
            let bytes = &chunk.buf[chunk.offset..chunk.offset + count];
            for (dest, &byte) in buf[filled..filled + count].iter_mut().zip(bytes) {
                *dest = MaybeUninit::new(byte);
            }
            chunk.offset += count;
            filled += count;
            if some {
                break;
            }
        }
        Ok(filled)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use tar_framing::{
        UstarKind,
        write::{end_marker_bytes, payload_padding},
    };

    use super::*;
    use crate::header::{self, HeaderSpec};

    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        for (sequence, (path, data)) in members.iter().enumerate() {
            let spec = HeaderSpec {
                path,
                kind: UstarKind::Regular,
                size: data.len() as u64,
                mode: 0o644,
                uid: 0,
                gid: 0,
                mtime: 0,
                uname: None,
                gname: None,
                link: None,
            };
            header::encode(&spec, sequence as u64, &mut buf).unwrap();
            out.extend_from_slice(&buf);
            out.extend_from_slice(data);
            out.extend_from_slice(payload_padding(data.len() as u64));
        }
        out.extend_from_slice(end_marker_bytes());
        out
    }

    fn cursor(members: &[(&str, &[u8])]) -> Cursor<io::Cursor<Vec<u8>>> {
        Cursor::new(TarReader::new(io::Cursor::new(archive(members))))
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    async fn read<R: AsyncRead + Unpin>(cursor: &mut Cursor<R>, len: usize) -> Vec<u8> {
        let mut buf = vec![MaybeUninit::uninit(); len];
        let count = cursor.read(&mut buf, false).await.unwrap();
        buf[..count]
            .iter()
            .map(|byte| unsafe { byte.assume_init() })
            .collect()
    }

    fn path<R: AsyncRead + Unpin>(cursor: &Cursor<R>) -> Vec<u8> {
        cursor.frame().unwrap().effective_path().unwrap().to_vec()
    }

    #[test]
    fn reads_members_across_moves() {
        block_on(async {
            let mut cursor = cursor(&[("a", b"hello"), ("b", b"abc")]);
            assert!(cursor.advance().await.unwrap());
            assert_eq!(path(&cursor), b"a");
            assert_eq!(read(&mut cursor, 2).await, b"he");

            // Moving the cursor keeps the frame borrowing its reader valid.
            let mut cursor = Box::new(cursor);
            assert_eq!(path(&cursor), b"a");
            assert_eq!(read(&mut cursor, 2).await, b"ll");

            // Advancing drains the rest of the member.
            assert!(cursor.advance().await.unwrap());
            assert_eq!(path(&cursor), b"b");
            assert_eq!(read(&mut cursor, 10).await, b"abc");
            assert_eq!(read(&mut cursor, 10).await, b"");
            assert!(!cursor.advance().await.unwrap());
            assert!(cursor.frame().is_none());
        });
    }

    #[test]
    fn drops_with_current_member() {
        block_on(async {
            let mut cursor = cursor(&[("a", b"hello")]);
            assert!(cursor.advance().await.unwrap());
            assert_eq!(read(&mut cursor, 1).await, b"h");
            drop(cursor);
        });
    }
}
