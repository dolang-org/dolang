use crate::global::Tag;

use std::{
    future::poll_fn,
    io::{self, IoSlice},
    pin::Pin,
};

use dolang::runtime::{
    Arg, Args, BYTE_STREAM_CHUNK_SIZE, Error, Output, Result, Slot, Strand, Value,
    io::Shared,
    value::{BinEmbryo, PinBin, PinStr, View},
};
use dolang_vfs::target::OperatingSystem;
use futures::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _};

use crate::{
    error::ResultExt as _,
    fs::{ReadSpare, poll_read_buffered, read_all, read_into_spare},
};

/// How a byte stream is quantized into values.
///
/// Framing only: it decides *where* a stream is cut, never what the resulting
/// values contain. Both modes are lossless — concatenating the values read from
/// a stream reproduces the stream byte for byte — so the choice is about the
/// shape of the iteration, not about the data. Removing or adding a line
/// terminator is a separate, explicit step (`chomp`/`crimp`).
///
/// A property of the stream object or redirect site that reads it, never of the
/// ambient strand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoMode {
    /// One `Str` per line, terminator included. The final value of a stream
    /// that does not end in a terminator simply has none.
    Line,
    /// Arbitrary `Bin` chunks, at whatever boundaries the reads fall on.
    Chunk,
}

/// Decodes a `:LINE:`/`:CHUNK:` framing argument, defaulting to line framing.
pub(crate) fn parse_mode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mode: Option<&Value<'v>>,
) -> Result<'v, 's, IoMode> {
    let global = strand.state::<Tag>();
    match mode {
        None => Ok(IoMode::Line),
        Some(value) => match value.as_sym(strand) {
            Some(sym) if sym == global.syms.line => Ok(IoMode::Line),
            Some(sym) if sym == global.syms.chunk => Ok(IoMode::Chunk),
            _ => Err(Error::value(strand, "mode must be :LINE: or :CHUNK:")),
        },
    }
}

pub(crate) async fn read_value<'v, R>(
    reader: &mut R,
    mode: IoMode,
    strand: &mut Strand<'v, '_>,
    out: &mut impl Output<'v>,
) -> io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    match mode {
        IoMode::Line => read_line_value(reader, strand, out).await,
        IoMode::Chunk => read_chunk_value(reader, BYTE_STREAM_CHUNK_SIZE, strand, out).await,
    }
}

/// Reads one chunk of at most `size` bytes, at whatever boundary the read falls
/// on, as a `Bin`.
pub(crate) async fn read_chunk_value<'v, R>(
    reader: &mut R,
    size: usize,
    strand: &mut Strand<'v, '_>,
    out: &mut impl Output<'v>,
) -> io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    let mut chunk = BinEmbryo::new_with_capacity(strand, size);
    let spare = chunk.spare_capacity_mut();
    let len = spare.len().min(size);
    let spare = &mut spare[..len];
    let len = poll_fn(|cx| poll_read_buffered(Pin::new(&mut *reader), cx, spare)).await?;
    if len == 0 {
        return Ok(false);
    }
    unsafe { chunk.advance(len) };
    chunk.finish(strand, out);
    Ok(true)
}

/// Reads one line, terminator included, as a `Str`.
///
/// The terminator is content: it is exactly the bytes the stream held, so a
/// `\r\n` file stays `\r\n` and a final line with no terminator yields a value
/// with none. Callers that want the terminator gone ask for it with `chomp`.
async fn read_line_value<'v, R>(
    reader: &mut R,
    strand: &mut Strand<'v, '_>,
    out: &mut impl Output<'v>,
) -> io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = BinEmbryo::new();

    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(false);
                }
                line.finish_str(strand, out).map_err(invalid_utf8)?;
                return Ok(true);
            }

            let newline = available.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            line.extend(strand, &available[..consumed]);
            (consumed, newline.is_some())
        };
        reader.consume_unpin(consumed);

        if complete {
            line.finish_str(strand, out).map_err(invalid_utf8)?;
            return Ok(true);
        }
    }
}

fn invalid_utf8(error: std::str::Utf8Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("stream did not contain valid UTF-8: {error}"),
    )
}

/// Encodes a value as the bytes to write for it, adding nothing.
///
/// The value edge's write half, and the exact inverse of what [`read_value`]
/// produces: a `Str` or `Bin` contributes its own bytes and nothing else, so
/// putting back everything read from a stream reproduces that stream. No line
/// terminator is appended and none is translated — a caller that wants one says
/// so with `crimp`.
///
/// Differs from [`write_pieces`] only in stringifying anything that is not a `Str`
/// or `Bin` rather than rejecting it, so `put 42` writes `42` the way `echo`
/// would.
pub(crate) fn encode_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, Vec<u8>> {
    Ok(match value.view(strand) {
        View::Str(value) => value.pin().as_bytes().to_vec(),
        View::Bin(value) => value.pin().to_vec(),
        _ => value.to_string(strand)?.into_bytes(),
    })
}

/// Reads up to `size` bytes, or to end of stream when `None`, as a `Bin`.
///
/// The unframed counterpart to [`read_value`]. Handle methods such as
/// `shell.stdin.read` sit on a byte edge rather than a value edge, so no
/// [`IoMode`] applies: nothing is quantized into lines and nothing is required
/// to be valid UTF-8. A bounded read is a single read and may yield fewer bytes
/// than requested, as `fs.File.read` does; empty means end of stream.
pub(crate) async fn read_raw<'v, 'a, 's, R>(
    reader: &mut R,
    size: Option<usize>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    R: ReadSpare,
{
    let mut buf = BinEmbryo::new();
    match size {
        Some(size) => {
            buf.reserve(strand, size);
            let read = read_into_spare(reader, buf.spare_capacity_mut())
                .await
                .into_sys(strand)?;
            unsafe { buf.advance(read) };
        }
        None => read_all(strand, reader, &mut buf).await?,
    }
    buf.finish(strand, out);
    Ok(())
}

/// A pinned `Str` or `Bin` payload, held so the bytes underneath it keep their
/// address for as long as a write is borrowing them.
pub(crate) enum Pinned<'v, 'a> {
    Str(PinStr<'v, 'a>),
    Bin(PinBin<'v, 'a>),
}

impl<'v, 'a> Pinned<'v, 'a> {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::Str(value) => value.as_bytes(),
            Self::Bin(value) => value,
        }
    }

    /// Pins a `Str` or `Bin` piece of a write.
    pub(crate) fn new<'s>(
        strand: &mut Strand<'v, 's>,
        value: &'a Value<'v>,
    ) -> Result<'v, 's, Self> {
        match value.view(strand) {
            View::Str(value) => Ok(Self::Str(value.pin())),
            View::Bin(value) => Ok(Self::Bin(value.pin())),
            _ => Err(Error::type_error(strand, "expected `Str` or `Bin`")),
        }
    }
}

/// Writes the bytes of `Str` and `Bin` pieces verbatim, reporting the byte count.
///
/// The unframed counterpart to [`encode_value`]. Handle methods such as
/// `shell.stdout.write` sit on a byte edge rather than a value edge, so no
/// [`IoMode`] applies: no line ending is appended and none is translated.
/// Anything that is not a `Str` or `Bin` is a type error rather than being
/// stringified, since there is no framing convention to stringify it into.
///
/// The pieces are written under one hold of the stream's lock, so other
/// writers cannot come between them.
pub(crate) async fn write_pieces<'v, 'a, 's, W>(
    stream: &Shared<W>,
    args: Args<'v, 'a>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    W: futures::AsyncWrite + Unpin,
{
    let mut pieces = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Arg::Pos(value) => pieces.push(Pinned::new(strand, value.into_ref())?),
            Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
        }
    }
    let mut slices = pieces
        .iter()
        .map(|piece| IoSlice::new(piece.as_slice()))
        .collect::<Vec<_>>();
    let written = slices.iter().map(|slice| slice.len()).sum::<usize>();
    let mut stream = stream.clone();
    let mut stream = stream.lock().await.into_sys(strand)?;
    stream
        .write_all_vectored(&mut slices)
        .await
        .into_sys(strand)?;
    Output::set(strand, out, written);
    Ok(())
}

/// The line terminator native to a platform.
///
/// Used at the *byte* edge — what `echo` puts into the stream, and what
/// `shell.line_ending()` reports so a script can `crimp` with it — never
/// applied implicitly to a value on its way out.
pub(crate) fn line_ending(operating_system: OperatingSystem) -> &'static [u8] {
    match operating_system {
        OperatingSystem::Windows => b"\r\n",
        OperatingSystem::FreeBsd | OperatingSystem::Linux | OperatingSystem::Macos => b"\n",
        _ => b"\n",
    }
}

pub(crate) fn strip_line_ending(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
}
