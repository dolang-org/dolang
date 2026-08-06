use std::io;

use dolang::runtime::{
    BYTE_STREAM_CHUNK_SIZE, Error, Output, Result, Slot, Strand, Value,
    value::{BinEmbryo, View},
};
use dolang_vfs::OperatingSystem;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt};

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    fs::{read_all, read_into_spare},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoMode {
    Line,
    Chunk,
}

#[derive(Clone, Copy)]
pub(crate) enum ValueEncoding {
    Display,
    Argument,
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
        IoMode::Chunk => {
            let mut chunk = BinEmbryo::new_with_capacity(strand, BYTE_STREAM_CHUNK_SIZE);
            let len = read_into_spare(reader, chunk.spare_capacity_mut()).await?;
            if len == 0 {
                return Ok(false);
            }
            unsafe { chunk.advance(len) };
            chunk.finish(strand, out);
            Ok(true)
        }
    }
}

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
            let content_len = newline.unwrap_or(consumed);
            line.extend(strand, &available[..content_len]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);

        if complete {
            if line.as_slice().ends_with(b"\r") {
                line.truncate(line.len() - 1);
            }
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

pub(crate) fn encode_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    mode: IoMode,
    encoding: ValueEncoding,
    operating_system: OperatingSystem,
) -> Result<'v, 's, Vec<u8>> {
    let (bytes, verbatim) = match value.view(strand) {
        View::Str(value) => (value.pin().as_bytes().to_vec(), false),
        View::Bin(value) => (value.pin().to_vec(), true),
        _ => {
            let value = match encoding {
                ValueEncoding::Display => value.to_string(strand)?,
                ValueEncoding::Argument => value.to_arg(strand)?,
            };
            (value.into_bytes(), false)
        }
    };
    Ok(frame_value(bytes, mode, verbatim, operating_system))
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
    R: AsyncRead + Unpin,
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

/// Writes the bytes of a `Str` or `Bin` value verbatim, reporting the byte count.
///
/// The unframed counterpart to [`encode_value`]. Handle methods such as
/// `shell.stdout.write` sit on a byte edge rather than a value edge, so no
/// [`IoMode`] applies: no line ending is appended and none is translated.
/// Anything that is not a `Str` or `Bin` is a type error rather than being
/// stringified, since there is no framing convention to stringify it into.
pub(crate) async fn write_raw<'v, 'a, 's, W>(
    writer: &mut W,
    data: Slot<'v, 'a>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()>
where
    W: AsyncWriteExt + Unpin + ?Sized,
{
    let written = match data.view(strand) {
        View::Str(value) => {
            let value = value.pin();
            writer
                .write_all(value.as_bytes())
                .await
                .map(|_| value.len())
        }
        View::Bin(value) => {
            let value = value.pin();
            writer.write_all(&value).await.map(|_| value.len())
        }
        _ => return Err(Error::type_error(strand, "expected `Str` or `Bin`")),
    }
    .map_err(|error| error.into_sys(strand))?;
    Output::set(strand, out, written);
    Ok(())
}

fn frame_value(
    mut bytes: Vec<u8>,
    mode: IoMode,
    verbatim: bool,
    operating_system: OperatingSystem,
) -> Vec<u8> {
    if mode == IoMode::Line && !verbatim {
        bytes.extend_from_slice(line_ending(operating_system));
    }
    bytes
}

pub(crate) fn line_ending(operating_system: OperatingSystem) -> &'static [u8] {
    match operating_system {
        OperatingSystem::Windows => b"\r\n",
        OperatingSystem::FreeBsd | OperatingSystem::Linux | OperatingSystem::Macos => b"\n",
    }
}

pub(crate) fn strip_line_ending(value: &str) -> &str {
    value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_uses_target_line_endings_for_non_binary_values() {
        assert_eq!(
            frame_value(
                b"text".to_vec(),
                IoMode::Line,
                false,
                OperatingSystem::Linux
            ),
            b"text\n"
        );
        assert_eq!(
            frame_value(
                b"text".to_vec(),
                IoMode::Line,
                false,
                OperatingSystem::Windows
            ),
            b"text\r\n"
        );
        assert_eq!(
            frame_value(
                b"BIN".to_vec(),
                IoMode::Line,
                true,
                OperatingSystem::Windows
            ),
            b"BIN"
        );
        assert_eq!(
            frame_value(
                b"text".to_vec(),
                IoMode::Chunk,
                false,
                OperatingSystem::Windows
            ),
            b"text"
        );
    }

    #[test]
    fn strips_exactly_one_complete_line_ending() {
        assert_eq!(strip_line_ending("text\r\n"), "text");
        assert_eq!(strip_line_ending("text\n"), "text");
        assert_eq!(strip_line_ending("text\r"), "text\r");
        assert_eq!(strip_line_ending("text\n\n"), "text\n");
        assert_eq!(strip_line_ending("text\r\r\n"), "text\r");
    }
}
