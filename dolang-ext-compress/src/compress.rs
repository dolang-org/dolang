use std::{
    error, fmt,
    future::poll_fn,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use async_compression::Level;
#[cfg(feature = "gzip")]
use async_compression::futures::{bufread::GzipDecoder, write::GzipEncoder};
#[cfg(feature = "xz")]
use async_compression::futures::{bufread::XzDecoder, write::XzEncoder};
#[cfg(feature = "zstd")]
use async_compression::futures::{bufread::ZstdDecoder, write::ZstdEncoder};
use dolang::runtime::io::IoResultExt as _;
use dolang::runtime::{
    Arg, BYTE_STREAM_CHUNK_SIZE, Error, Object, Output, Result, Slot, State, Strand, Sym, Value,
    call,
    error::ErrorKind,
    io::{ByteHandle, ByteIo, IntoByteIo, IoCaps, Read, Shared, Write},
    method,
    object::{Request, TypeBuilder},
    strand::InterruptMask,
    unpack,
    value::{BinEmbryo, TypeObject, View},
    vm::Register,
};
use futures_lite::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::io::{AsyncRead as TokioAsyncRead, ReadBuf};
use tokio_util::compat::{FuturesAsyncReadCompatExt as _, TokioAsyncReadCompatExt as _};

use crate::global::Global;

/// Leading bytes of each format, for `:AUTO:` detection.
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Gzip,
    Zstd,
    Xz,
}

impl Format {
    fn sym<'v>(self, global: State<'v, Global<'v>>) -> Sym<'v, 'v> {
        match self {
            Format::Gzip => global.syms.gzip,
            Format::Zstd => global.syms.zstd,
            Format::Xz => global.syms.xz,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Format::Gzip => ":GZIP:",
            Format::Zstd => ":ZSTD:",
            Format::Xz => ":XZ:",
        }
    }

    fn available(self) -> bool {
        match self {
            Format::Gzip => cfg!(feature = "gzip"),
            Format::Zstd => cfg!(feature = "zstd"),
            Format::Xz => cfg!(feature = "xz"),
        }
    }

    fn detect(magic: &[u8]) -> Option<Self> {
        [
            (GZIP_MAGIC, Format::Gzip),
            (ZSTD_MAGIC, Format::Zstd),
            (XZ_MAGIC, Format::Xz),
        ]
        .into_iter()
        .find_map(|(prefix, format)| magic.starts_with(prefix).then_some(format))
    }

    fn check_available<'v, 's>(self, strand: &mut Strand<'v, 's>) -> Result<'v, 's, Self> {
        if self.available() {
            Ok(self)
        } else {
            Err(Error::value(
                strand,
                format!("compression format {} is not available", self.name()),
            ))
        }
    }

    fn decoder(self, inner: Box<dyn Read>) -> Box<dyn Read> {
        let inner = BufReader::with_capacity(BYTE_STREAM_CHUNK_SIZE, Source(inner));
        match self {
            #[cfg(feature = "gzip")]
            Format::Gzip => {
                let mut decoder = GzipDecoder::new(inner);
                decoder.multiple_members(true);
                Box::new(Decoded(decoder))
            }
            #[cfg(feature = "zstd")]
            Format::Zstd => {
                let mut decoder = ZstdDecoder::new(inner);
                decoder.multiple_members(true);
                Box::new(Decoded(decoder))
            }
            #[cfg(feature = "xz")]
            Format::Xz => {
                let mut decoder = XzDecoder::new(inner);
                decoder.multiple_members(true);
                Box::new(Decoded(decoder))
            }
            #[cfg(not(all(feature = "gzip", feature = "zstd", feature = "xz")))]
            _ => unreachable!("format availability is checked first"),
        }
    }

    fn encoder(self, inner: Box<dyn Write>, level: Level) -> Box<dyn Write> {
        match self {
            #[cfg(feature = "gzip")]
            Format::Gzip => Box::new(GzipEncoder::with_quality(inner, level)),
            #[cfg(feature = "zstd")]
            Format::Zstd => Box::new(ZstdEncoder::with_quality(inner, level)),
            #[cfg(feature = "xz")]
            Format::Xz => Box::new(XzEncoder::with_quality(inner, level)),
            #[cfg(not(all(feature = "gzip", feature = "zstd", feature = "xz")))]
            _ => unreachable!("format availability is checked first"),
        }
    }
}

/// Wraps an error from a decoder's source, so [`Decoded`] can tell it from a codec error.
#[derive(Debug)]
struct SourceError(io::Error);

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl error::Error for SourceError {}

/// A decoder's source, tagging its errors as [`SourceError`]s.
struct Source(Box<dyn Read>);

impl AsyncRead for Source {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0)
            .poll_read(cx, buf)
            .map_err(|err| io::Error::new(err.kind(), SourceError(err)))
    }
}

/// A decoder whose errors are normalized.
///
/// Errors from the source pass through unchanged. The codecs report bad input
/// inconsistently (liblzma reports truncation as `OutOfMemory`, and corruption is
/// often `Other`), so a codec error becomes `UnexpectedEof` for truncation and
/// `InvalidData` otherwise.
struct Decoded<D>(D);

impl<D: AsyncRead + Unpin> AsyncRead for Decoded<D> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf).map_err(|err| {
            if err.get_ref().is_some_and(|inner| inner.is::<SourceError>()) {
                let inner = err.into_inner().expect("checked above");
                return inner.downcast::<SourceError>().expect("checked above").0;
            }
            match err.kind() {
                io::ErrorKind::UnexpectedEof | io::ErrorKind::OutOfMemory => {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "compressed data is truncated")
                }
                _ => io::Error::new(io::ErrorKind::InvalidData, err),
            }
        })
    }
}

/// A requested format; `None` is `:AUTO:`.
fn parse_format<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
    auto: bool,
) -> Result<'v, 's, Option<Format>> {
    let syms = &global.syms;
    match value.as_sym(strand) {
        Some(sym) if sym == syms.gzip => Format::Gzip.check_available(strand).map(Some),
        Some(sym) if sym == syms.zstd => Format::Zstd.check_available(strand).map(Some),
        Some(sym) if sym == syms.xz => Format::Xz.check_available(strand).map(Some),
        Some(sym) if auto && sym == syms.auto => Ok(None),
        _ if auto => Err(Error::value(
            strand,
            "format must be :GZIP:, :ZSTD:, :XZ:, or :AUTO:",
        )),
        _ => Err(Error::value(
            strand,
            "format must be :GZIP:, :ZSTD:, or :XZ:",
        )),
    }
}

fn parse_level<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Level> {
    let Some(value) = value else {
        return Ok(Level::Default);
    };
    let syms = &global.syms;
    if let Some(sym) = value.as_sym(strand) {
        return match sym {
            sym if sym == syms.fastest => Ok(Level::Fastest),
            sym if sym == syms.best => Ok(Level::Best),
            sym if sym == syms.default => Ok(Level::Default),
            _ => Err(Error::value(
                strand,
                "level: expected :FASTEST:, :BEST:, :DEFAULT:, or an Int",
            )),
        };
    }
    let level = value.to_i64(strand).map_err(|_| {
        Error::type_error(
            strand,
            "level: expected :FASTEST:, :BEST:, :DEFAULT:, or an Int",
        )
    })?;
    let level = level.try_into().map_err(|_| Error::overflow(strand))?;
    Ok(Level::Precise(level))
}

/// Gets the byte handle `endpoint` names: a path is opened, and any other value must
/// provide [`ByteIo`] with `need`.
async fn endpoint_handle<'v, 's>(
    strand: &mut Strand<'v, 's>,
    endpoint: &Value<'v>,
    need: IoCaps,
    name: &str,
) -> Result<'v, 's, ByteHandle> {
    let write = need.contains(IoCaps::WRITE);
    if let Some(path) = dolang_ext_shell::as_path(strand, endpoint) {
        let mode = if write { "w" } else { "r" };
        let file = dolang_ext_shell::open(strand, path.as_ref(), mode)
            .await
            .into_do_io(strand)?
            .compat();
        return Ok(if write {
            ByteHandle::write(file)
        } else {
            ByteHandle::read(file)
        });
    }
    dolang::runtime::io::get(strand, endpoint, |strand, caps| {
        if !caps.contains(need) {
            let access = if write { "writable" } else { "readable" };
            return Err(Error::value(strand, format!("{name} must be {access}")));
        }
        Ok(())
    })
    .map_err(|err| {
        if err.kind() == ErrorKind::Unsupported {
            Error::type_error(
                strand,
                format!("expected `Str`, `Path`, or a byte I/O provider for `{name}`"),
            )
        } else {
            err
        }
    })
}

async fn read_into_embryo(
    reader: &mut (impl futures_lite::io::AsyncRead + Unpin),
    embryo: &mut BinEmbryo<'_>,
    limit: usize,
) -> io::Result<usize> {
    let mut reader = reader.compat();
    let spare = embryo.spare_capacity_mut();
    let len = limit.min(spare.len());
    let mut buf = ReadBuf::uninit(&mut spare[..len]);
    poll_fn(|cx| TokioAsyncRead::poll_read(Pin::new(&mut reader), cx, &mut buf)).await?;
    let read = buf.filled().len();
    unsafe { embryo.advance(read) };
    Ok(read)
}

pub(crate) struct StreamAnnex<'v> {
    global: State<'v, Global<'v>>,
    format: Format,
}

/// A decompressing reader, shared with the handles it lends.
pub(crate) struct Decompressor {
    reader: Option<Shared<Box<dyn Read>>>,
}

impl<'v> IntoByteIo<'v> for Decompressor {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(IoCaps::READ)
    }

    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        // Reads fill exactly what was asked for, so there is no read-ahead to hand over.
        let reader = self.reader.as_ref().expect("stream is closed");
        Ok(ByteHandle::read(reader.clone()))
    }
}

impl<'v> Object<'v> for Decompressor {
    const NAME: &'v str = "Decompressor";
    const MODULE: &'v str = "compress";
    type Annex = StreamAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn provide<'a, 's>(
        this: dolang::runtime::Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            if borrow.reader.is_none() {
                return Err(Error::state_error(request, "stream closed"));
            }
            request.provide::<ByteIo>(&mut *borrow)?;
        }
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder.supertype(TypeObject::Io);
        builder
            .get("readable", |this, strand, out| {
                let open = this.borrow(strand)?.reader.is_some();
                Output::set(strand, out, open);
                Ok(())
            })
            .get("format", |this, strand, out| {
                let annex = this.annex();
                Output::set(strand, out, annex.format.sym(annex.global));
                Ok(())
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [size]) = unpack!(strand, args, 0, 1)?;
                let size: Option<usize> = size
                    .map(|size| {
                        size.to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "expected integer"))?
                            .try_into()
                            .map_err(|_| Error::overflow(strand))
                    })
                    .transpose()?;

                let mut data =
                    BinEmbryo::new_with_capacity(strand, size.unwrap_or(BYTE_STREAM_CHUNK_SIZE));
                let mut borrow = this.borrow_mut(strand)?;
                let reader = borrow
                    .reader
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "stream closed"))?;

                loop {
                    let remaining = match size {
                        Some(size) if data.len() >= size => break,
                        Some(size) => size - data.len(),
                        None => BYTE_STREAM_CHUNK_SIZE,
                    };
                    data.reserve(strand, remaining);
                    let n = read_into_embryo(reader, &mut data, remaining)
                        .await
                        .into_do_io(strand)?;
                    if n == 0 {
                        break;
                    }
                }

                drop(borrow);
                data.finish(strand, out);
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let reader = this.borrow_mut(strand)?.reader.take();
                if let Some(mut reader) = reader {
                    // Closes it for the handles lent out too.
                    drop(reader.close().await);
                }
                Ok(())
            })
    }
}

/// A compressing writer, shared with the handles it lends.
///
/// A lent handle's `close` only flushes, so only the compressor's own `close`
/// finishes the stream.
pub(crate) struct Compressor {
    writer: Option<Shared<Box<dyn Write>>>,
}

impl<'v> IntoByteIo<'v> for Compressor {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(IoCaps::WRITE)
    }

    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        let writer = self.writer.as_ref().expect("stream is closed");
        Ok(ByteHandle::write(writer.clone()))
    }
}

impl<'v> Object<'v> for Compressor {
    const NAME: &'v str = "Compressor";
    const MODULE: &'v str = "compress";
    type Annex = StreamAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn provide<'a, 's>(
        this: dolang::runtime::Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            let mut borrow = this.borrow_mut(request)?;
            if borrow.writer.is_none() {
                return Err(Error::state_error(request, "stream closed"));
            }
            request.provide::<ByteIo>(&mut *borrow)?;
        }
        Ok(())
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder.supertype(TypeObject::Io);
        builder
            .get("writable", |this, strand, out| {
                let open = this.borrow(strand)?.writer.is_some();
                Output::set(strand, out, open);
                Ok(())
            })
            .get("format", |this, strand, out| {
                let annex = this.annex();
                Output::set(strand, out, annex.format.sym(annex.global));
                Ok(())
            })
            .method("write", async move |this, strand, args, out| {
                // The encoder copies what it is given, so joining the pieces costs no more.
                let mut data = Vec::new();
                for arg in args {
                    let piece = match arg {
                        Arg::Pos(piece) => piece,
                        Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
                    };
                    match piece.view(strand) {
                        View::Str(s) => data.extend_from_slice(s.pin().as_bytes()),
                        View::Bin(b) => data.extend_from_slice(&b.pin()),
                        _ => return Err(Error::type_error(strand, "expected bytes")),
                    }
                }

                let mut borrow = this.borrow_mut(strand)?;
                let writer = borrow
                    .writer
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "stream closed"))?;
                writer.write_all(&data).await.into_do_io(strand)?;
                drop(borrow);
                Output::set(strand, out, data.len());
                Ok(())
            })
            .method("close", async move |this, strand, args, _out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let writer = this.borrow_mut(strand)?.writer.take();
                let Some(mut writer) = writer else {
                    return Ok(());
                };
                let Some(mut writer) = writer.close().await else {
                    return Ok(());
                };
                // Finishes the stream, then closes the inner handle.
                writer.close().await.into_do_io(strand)
            })
    }
}

/// Splits the trailing optional positionals into the format and block.
fn format_and_block<'v, 'a>(
    strand: &mut Strand<'v, '_>,
    opt1: Option<Slot<'v, 'a>>,
    opt2: Option<Slot<'v, 'a>>,
) -> (Option<Slot<'v, 'a>>, Option<Slot<'v, 'a>>) {
    match (opt1, opt2) {
        (Some(first), None) if first.as_sym(strand).is_none() => (None, Some(first)),
        (first, second) => (first, second),
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    let close = builder.sym("close");
    let level_sym = builder.sym("level");
    builder
        .module("compress")
        .value("Compressor", global.types.compressor)
        .value("Decompressor", global.types.decompressor)
        .function("decompress", async move |strand, args, out| {
            let ([source], [opt1, opt2]) = unpack!(strand, args, 1, 2)?;
            let (format, block) = format_and_block(strand, opt1, opt2);
            let format = match format {
                Some(format) => parse_format(strand, global, &format, true)?,
                None => None,
            };

            let handle = endpoint_handle(strand, &source, IoCaps::READ, "source").await?;
            let inner = handle.into_read().expect("source is readable");
            let (format, inner) = match format {
                Some(format) => (format, inner),
                None => {
                    let mut inner = inner;
                    let mut magic = Vec::with_capacity(XZ_MAGIC.len());
                    while magic.len() < XZ_MAGIC.len() {
                        let mut byte = [0; XZ_MAGIC.len()];
                        let want = XZ_MAGIC.len() - magic.len();
                        let read = inner.read(&mut byte[..want]).await.into_do_io(strand)?;
                        if read == 0 {
                            break;
                        }
                        magic.extend_from_slice(&byte[..read]);
                    }
                    let format = Format::detect(&magic)
                        .ok_or_else(|| Error::value(strand, "unrecognized compression format"))?;
                    let format = format.check_available(strand)?;
                    // Replay the detected bytes ahead of the rest of the stream.
                    let inner = ByteHandle::read(inner)
                        .with_prefix(magic)
                        .into_read()
                        .expect("handle is readable");
                    (format, inner)
                }
            };

            let state = Decompressor {
                reader: Some(Shared::new(format.decoder(inner))),
            };
            let annex = StreamAnnex { global, format };
            let ty = global.types.decompressor;
            if let Some(block) = block {
                strand
                    .with_slots(async |strand, [mut wrapper, mut tmp]| {
                        ty.create_with_annex(strand, state, annex, &mut wrapper);
                        let result = call!(strand, block, out, &wrapper).await;
                        let close_result = strand
                            .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                                method!(strand, &wrapper, close, &mut tmp).await
                            })
                            .await;
                        result.and(close_result)
                    })
                    .await
            } else {
                ty.create_with_annex(strand, state, annex, out);
                Ok(())
            }
        })
        .function("compress", async move |strand, args, out| {
            let ([sink, format], [block, level]) = unpack!(strand, args, 2, 1, level_sym = None)?;
            let format = parse_format(strand, global, &format, false)?
                .expect("`:AUTO:` is rejected for compression");
            let level = parse_level(strand, global, level)?;

            let handle = endpoint_handle(strand, &sink, IoCaps::WRITE, "sink").await?;
            let inner = handle.into_write().expect("sink is writable");
            let state = Compressor {
                writer: Some(Shared::new(format.encoder(inner, level))),
            };
            let annex = StreamAnnex { global, format };
            let ty = global.types.compressor;
            if let Some(block) = block {
                strand
                    .with_slots(async |strand, [mut wrapper, mut tmp]| {
                        ty.create_with_annex(strand, state, annex, &mut wrapper);
                        let result = call!(strand, block, out, &wrapper).await;
                        let close_result = strand
                            .with_interrupt_mask(InterruptMask::all(), async move |strand| {
                                method!(strand, &wrapper, close, &mut tmp).await
                            })
                            .await;
                        result.and(close_result)
                    })
                    .await
            } else {
                ty.create_with_annex(strand, state, annex, out);
                Ok(())
            }
        })
        .commit();
}
