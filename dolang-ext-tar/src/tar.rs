use std::{
    io::{self, IoSlice},
    ops::Deref,
    path::Path,
    pin::Pin,
    str,
    task::{Context, Poll, ready},
    time::{Duration, SystemTime},
};

use async_compression::tokio::{
    bufread::{GzipDecoder, ZstdDecoder},
    write::{GzipEncoder, ZstdEncoder},
};
use dolang::runtime::io::{IoErrorExt as _, IoResultExt as _};
use dolang::runtime::{
    AllocExt, Arg, BYTE_STREAM_CHUNK_SIZE, Error, Instance, Object, Result, Slot, State, Strand,
    Value, call,
    error::{ErrorKind, ResultExt},
    io::{ByteHandle, ByteIo, IntoByteIo, IoCaps, Shared},
    object::{Mut, Ref, Request, TypeBuilder},
    unpack,
    value::{BinEmbryo, Nil, PinBin, PinStr, TypeObject, View},
    vm::Register,
    with_instance, with_slots,
};
use tar_framing::{
    FrameError, FrameErrorInner, PaxKeyword, PaxRecord, PaxValue, StreamPolicy, TrailingDataPolicy,
    UstarKind,
    logical::{MemberExtensions, MemberFrame, TarReader as FrameReader},
    write::{end_marker_bytes, payload_padding},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::compat::{
    Compat, FuturesAsyncReadCompatExt as _, FuturesAsyncWriteCompatExt as _,
    TokioAsyncWriteCompatExt as _,
};

use crate::cursor::Cursor;
use crate::global::Global;
use crate::header::{self, HeaderSpec};

const GZIP_MAGIC: &[u8] = b"\x1f\x8b";
const ZSTD_MAGIC: &[u8] = b"\x28\xb5\x2f\xfd";

type DynReader = Pin<Box<dyn AsyncRead + Send>>;
type DynWriter = Pin<Box<dyn AsyncWrite + Send>>;
type NativeFrame<'a> = MemberFrame<'a, DynReader>;

/// Maps a framing error to a Do error: a failed read keeps its I/O class, and
/// anything else is malformed data.
fn frame_error<'v, 's>(strand: &mut Strand<'v, 's>, err: FrameError) -> Error<'v, 's> {
    match err.inner {
        FrameErrorInner::Io { source } => source.into_do_decode(strand),
        _ => Error::data(strand, err.to_string()),
    }
}

/// The effective value of a numeric field: a pax record overrides the header
/// fallback, and a deleted one leaves it absent.
fn effective_number(frame: &NativeFrame<'_>, keyword: PaxKeyword, fallback: Option<u64>) -> u64 {
    if let MemberExtensions::Pax(state) = &frame.extensions
        && let Some(PaxRecord::Uid(value) | PaxRecord::Gid(value) | PaxRecord::Mtime(value)) =
            state.effective_record(&keyword)
    {
        return match value {
            PaxValue::Value(value) => *value,
            PaxValue::Deleted => 0,
        };
    }
    fallback.unwrap_or(0)
}

/// The effective owner or group name, like [`effective_number`]; empty is absent.
fn effective_name<'a>(
    frame: &'a NativeFrame<'_>,
    keyword: PaxKeyword,
    fallback: &'a [u8],
) -> Option<&'a [u8]> {
    let name = match &frame.extensions {
        MemberExtensions::Pax(state) => match state.effective_record(&keyword) {
            Some(PaxRecord::Uname(value) | PaxRecord::Gname(value)) => match value {
                PaxValue::Value(value) => value.as_bytes(),
                PaxValue::Deleted => &[],
            },
            _ => fallback,
        },
        MemberExtensions::Gnu { .. } => fallback,
    };
    (!name.is_empty()).then_some(name)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Zstd,
}

pub(crate) struct TarReader {
    cursor: Cursor<DynReader>,
    generation: u64,
    compression: Compression,
    closed: bool,
}

pub(crate) struct TarEntry {
    generation: u64,
}

pub(crate) struct TarWriter {
    /// The archive output, taken while an entry is being written.
    sink: Option<DynWriter>,
    /// Numbers pax extended headers.
    sequence: u64,
    generation: u64,
    active: bool,
    poisoned: bool,
    compression: Compression,
}

pub(crate) struct TarEntryWriter {
    /// Taken when the entry's scope ends, which closes it for lent handles too.
    stream: Option<Shared<Compat<EntrySink>>>,
    generation: u64,
}

/// An entry's content stream into the archive output, refusing to go past the
/// declared size.
struct EntrySink {
    writer: DynWriter,
    written: u64,
    size: u64,
    /// Whether a write to the output failed, leaving the archive unusable.
    failed: bool,
}

impl EntrySink {
    /// Fails if `len` more bytes would go past the declared size.
    fn check(&self, len: usize) -> io::Result<()> {
        let len = u64::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        if self.written.saturating_add(len) > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tar entry exceeds declared size",
            ));
        }
        Ok(())
    }

    fn count(&mut self, written: io::Result<usize>) -> io::Result<usize> {
        let written = written.inspect_err(|_| self.failed = true)?;
        self.written += written as u64;
        Ok(written)
    }
}

impl AsyncWrite for EntrySink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check(buf.len())?;
        let written = ready!(Pin::new(&mut this.writer).poll_write(cx, buf));
        Poll::Ready(this.count(written))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check(bufs.iter().map(|buf| buf.len()).sum())?;
        let written = ready!(Pin::new(&mut this.writer).poll_write_vectored(cx, bufs));
        Poll::Ready(this.count(written))
    }

    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// Lends a handle sharing an entry's content stream.
struct EntryIo<'a> {
    stream: &'a Shared<Compat<EntrySink>>,
}

impl<'v> IntoByteIo<'v> for EntryIo<'_> {
    fn caps<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, IoCaps> {
        Ok(IoCaps::WRITE)
    }

    fn get<'s>(&mut self, _strand: &mut Strand<'v, 's>) -> Result<'v, 's, ByteHandle> {
        Ok(ByteHandle::write(self.stream.clone()))
    }
}

fn compression_sym<'v>(
    global: State<'v, Global<'v>>,
    compression: Compression,
) -> dolang::runtime::Sym<'v, 'v> {
    match compression {
        Compression::None => global.syms.none,
        Compression::Gzip => global.syms.gzip,
        Compression::Zstd => global.syms.zstd,
    }
}

fn entry_type_sym<'v>(
    global: State<'v, Global<'v>>,
    kind: UstarKind,
) -> dolang::runtime::Sym<'v, 'v> {
    match kind {
        UstarKind::Regular => global.syms.file,
        UstarKind::HardLink => global.syms.hardlink,
        UstarKind::SymbolicLink => global.syms.symlink,
        UstarKind::CharacterDevice => global.syms.char_device,
        UstarKind::BlockDevice => global.syms.block_device,
        UstarKind::Directory => global.syms.dir,
        UstarKind::Fifo => global.syms.fifo,
        UstarKind::Contiguous => global.syms.contiguous,
    }
}

fn nonnegative_u64<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    name: &str,
) -> Result<'v, 's, u64> {
    let value = value
        .to_int(strand)
        .map_err(|_| Error::type_error(strand, format!("{name} must be a non-negative integer")))?;
    u64::try_from(value)
        .map_err(|_| Error::value(strand, format!("{name} must be a non-negative integer")))
}

fn optional_u64<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    default: u64,
    name: &str,
) -> Result<'v, 's, u64> {
    value
        .map(|value| nonnegative_u64(strand, value, name))
        .unwrap_or(Ok(default))
}

fn optional_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
    name: &str,
) -> Result<'v, 's, Option<String>> {
    value
        .map(|value| {
            value
                .as_str(strand)
                .map(|value| value.to_string())
                .ok_or_else(|| Error::type_error(strand, format!("{name} must be a string")))
        })
        .transpose()
}

fn unix_path_string<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Slot<'v, '_>,
    name: &str,
) -> Result<'v, 's, String> {
    if let Some(path) = dolang_ext_shell::as_unix_path(strand, &value) {
        Ok(path.as_str().to_owned())
    } else if let Some(path) = value.as_str(strand) {
        Ok(path.to_string())
    } else {
        Err(Error::type_error(
            strand,
            format!("{name} must be a Str or fs.unix.Path"),
        ))
    }
}

/// Converts an entry path like [`unix_path_string`], rejecting paths that are
/// unsafe to extract before the writer is touched, so a bad argument doesn't
/// poison the writer.
fn entry_path<'v, 's>(strand: &mut Strand<'v, 's>, value: Slot<'v, '_>) -> Result<'v, 's, String> {
    let path = unix_path_string(strand, value, "path")?;
    if path.starts_with('/') {
        return Err(Error::value(strand, "tar entry path must be relative"));
    }
    if path.contains('\0') {
        return Err(Error::value(strand, "tar entry path must not contain NUL"));
    }
    let mut components = path.split('/').filter(|c| !c.is_empty() && *c != ".");
    if components.clone().any(|c| c == "..") {
        return Err(Error::value(strand, "tar entry path must not contain `..`"));
    }
    if components.next().is_none() {
        return Err(Error::value(strand, "tar entry path must not be empty"));
    }
    Ok(path)
}

/// Converts a link target like [`unix_path_string`], rejecting an empty one.
fn link_target<'v, 's>(strand: &mut Strand<'v, 's>, value: Slot<'v, '_>) -> Result<'v, 's, String> {
    let target = unix_path_string(strand, value, "target")?;
    if target.is_empty() {
        return Err(Error::value(strand, "tar link target must not be empty"));
    }
    Ok(target)
}

/// Metadata common to every tar entry, shared by `entry`, `create_dir`,
/// `symlink`, and `hard_link`.
struct CommonEntryOptions {
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    owner_name: Option<String>,
    group_name: Option<String>,
}

#[expect(clippy::too_many_arguments)]
fn parse_common_entry_options<'v, 's>(
    strand: &mut Strand<'v, 's>,
    default_mode: u64,
    mode: Option<Slot<'v, '_>>,
    uid: Option<Slot<'v, '_>>,
    gid: Option<Slot<'v, '_>>,
    mtime: Option<Slot<'v, '_>>,
    owner_name: Option<Slot<'v, '_>>,
    group_name: Option<Slot<'v, '_>>,
) -> Result<'v, 's, CommonEntryOptions> {
    let mode = u32::try_from(optional_u64(strand, mode, default_mode, "mode")?)
        .map_err(|_| Error::overflow(strand))?;
    let uid = optional_u64(strand, uid, 0, "uid")?;
    let gid = optional_u64(strand, gid, 0, "gid")?;
    let mtime = match mtime {
        Some(value) => dolang_ext_time::as_datetime(strand, &value)
            .ok_or_else(|| Error::type_error(strand, "mtime must be a DateTime"))?
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| Error::value(strand, "mtime is before the Unix epoch"))?
            .as_secs(),
        None => 0,
    };
    let owner_name = optional_string(strand, owner_name, "owner_name")?;
    let group_name = optional_string(strand, group_name, "group_name")?;
    Ok(CommonEntryOptions {
        mode,
        uid,
        gid,
        mtime,
        owner_name,
        group_name,
    })
}

/// Encodes an entry header into a buffer before the writer is touched, so a
/// value that can't be encoded doesn't poison the writer.
fn build_header<'v, 's>(
    strand: &mut Strand<'v, 's>,
    sequence: u64,
    path: &str,
    kind: UstarKind,
    size: u64,
    opts: &CommonEntryOptions,
    link: Option<&str>,
) -> Result<'v, 's, Vec<u8>> {
    let spec = HeaderSpec {
        path,
        kind,
        size,
        mode: opts.mode,
        uid: opts.uid,
        gid: opts.gid,
        mtime: opts.mtime,
        uname: opts.owner_name.as_deref(),
        gname: opts.group_name.as_deref(),
        link,
    };
    let mut buf = Vec::new();
    header::encode(&spec, sequence, &mut buf).into_do_value(strand)?;
    Ok(buf)
}

/// Takes the archive output for an entry, failing if the writer can't start one.
fn take_sink<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, TarWriter>,
) -> Result<'v, 's, (DynWriter, u64)> {
    let mut parent = this.borrow_mut(strand)?;
    if parent.poisoned {
        return Err(Error::state_error(strand, "tar writer is poisoned"));
    }
    if parent.active {
        return Err(Error::concurrency(strand));
    }
    let sink = parent
        .sink
        .take()
        .ok_or_else(|| Error::state_error(strand, "tar writer is closed"))?;
    parent.active = true;
    parent.generation = parent.generation.wrapping_add(1);
    parent.sequence += 1;
    Ok((sink, parent.generation))
}

/// Returns the archive output after an entry, poisoning the writer if the entry failed.
fn restore_sink<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, TarWriter>,
    sink: DynWriter,
    ok: bool,
) -> Result<'v, 's, ()> {
    let mut parent = this.borrow_mut(strand)?;
    parent.active = false;
    parent.sink = Some(sink);
    if !ok {
        parent.poisoned = true;
    }
    Ok(())
}

/// The sequence number the next entry's pax header will get.
fn next_sequence<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, TarWriter>,
) -> Result<'v, 's, u64> {
    Ok(this.borrow(strand)?.sequence)
}

/// Appends a zero-content entry (directory or link) directly, without
/// exposing a write handle: none carries a byte stream, so a block would
/// have nothing meaningful to write to.
async fn append_metadata_only_entry<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, TarWriter>,
    path: &str,
    kind: UstarKind,
    opts: &CommonEntryOptions,
    link: Option<&str>,
) -> Result<'v, 's, ()> {
    let sequence = next_sequence(strand, this)?;
    let header = build_header(strand, sequence, path, kind, 0, opts, link)?;
    let (mut sink, _) = take_sink(strand, this)?;
    let result = sink.write_all(&header).await;
    restore_sink(strand, this, sink, result.is_ok())?;
    result.into_do_io(strand)
}

/// Validates that the reader-backed entry `$this` is still active, binds
/// `$entry` to a borrow of the underlying reader state, and evaluates
/// `$body` (which sees `$entry` and a reborrow of `$strand`) inside that
/// scope. `$body` must evaluate to a `Result<'v, 's, ()>`.
macro_rules! active_entry {
    ($this:expr, $strand:ident, $entry:ident, $body:block) => {{
        let child = $this.borrow($strand)?;
        with_instance!(
            $strand,
            $this
                .annex()
                .types
                .reader
                .cast(Ref::slot::<0>(&child))
                .ok_or_else(|| Error::state_error($strand, "tar entry is invalid"))?,
            |reader_inst| {
                let $entry = reader_inst.borrow($strand)?;
                if $entry.generation != child.generation {
                    return Err(Error::state_error($strand, "tar entry is no longer active"));
                }
                $body
            }
        )
    }};
}

macro_rules! validate_entry_writer {
    ($this:expr, $strand:ident) => {{
        let child = $this.borrow($strand)?;
        with_instance!(
            $strand,
            $this
                .annex()
                .types
                .writer
                .cast(Ref::slot::<0>(&child))
                .ok_or_else(|| Error::state_error($strand, "tar entry writer is invalid"))?,
            |writer_inst| {
                let parent = writer_inst.borrow($strand)?;
                if !parent.active || parent.generation != child.generation || child.stream.is_none()
                {
                    return Err(Error::state_error($strand, "tar entry writer is closed"));
                }
                Ok(())
            }
        )?
    }};
}

impl<'v> Object<'v> for TarReader {
    const NAME: &'v str = "Reader";
    const MODULE: &'v str = "tar";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .supertype(TypeObject::Iter)
            .get("compression", |this, strand, out| {
                let compression = this.borrow(strand)?.compression;
                strand.store(out, compression_sym(*this.annex(), compression));
                Ok(())
            })
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let global = *this.annex();
        let mut borrow = this.borrow_mut(strand)?;
        if borrow.closed {
            return Err(Error::state_error(strand, "tar reader is closed"));
        }
        borrow.generation = borrow.generation.wrapping_add(1);
        match borrow.cursor.advance().await {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(err) => return Err(frame_error(strand, err)),
        }
        let generation = borrow.generation;
        drop(borrow);

        global.types.entry.create_with_annex(
            strand,
            TarEntry { generation },
            global,
            Slot::reborrow(&mut out),
        );
        with_instance!(strand, global.types.entry.cast(&out).unwrap(), |entry| {
            let mut entry_borrow = entry.borrow_mut_unwrap();
            strand.store(Mut::slot_mut::<0>(&mut entry_borrow), this);
        });
        Ok(true)
    }
}

impl<'v> Object<'v> for TarEntry {
    const NAME: &'v str = "Entry";
    const MODULE: &'v str = "tar";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    // Native consumers are served through `read_some` by `Io`'s bridge, so a
    // read after the reader advances fails as `read` does.
    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder
            .supertype(TypeObject::Iter)
            .supertype(TypeObject::Io)
            .get("readable", |this, strand, out| {
                let child = this.borrow(strand)?;
                let generation = child.generation;
                let active = with_instance!(
                    strand,
                    this.annex()
                        .types
                        .reader
                        .cast(Ref::slot::<0>(&child))
                        .ok_or_else(|| Error::state_error(strand, "tar entry is invalid"))?,
                    |reader| {
                        let reader = reader.borrow(strand)?;
                        Ok(!reader.closed && reader.generation == generation)
                    }
                )?;
                strand.store(out, active);
                Ok(())
            })
            .get("path", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let frame = borrow.cursor.frame().unwrap();
                    let path = frame
                        .effective_path()
                        .map_err(|err| frame_error(strand, err))?;
                    let path = str::from_utf8(path).into_do_data(strand)?;
                    dolang_ext_shell::unix_path(strand, path, out)
                })
            })
            .get("type", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let kind = borrow.cursor.frame().unwrap().header.kind;
                    strand.store(out, entry_type_sym(*this.annex(), kind));
                    Ok(())
                })
            })
            .get("size", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    strand.store(out, borrow.cursor.frame().unwrap().header.effective_size);
                    Ok(())
                })
            })
            .get("mode", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let mode = borrow.cursor.frame().unwrap().header.mode;
                    strand.store(out, mode.unwrap_or(0));
                    Ok(())
                })
            })
            .get("uid", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let frame = borrow.cursor.frame().unwrap();
                    strand.store(
                        out,
                        effective_number(frame, PaxKeyword::Uid, frame.header.uid),
                    );
                    Ok(())
                })
            })
            .get("gid", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let frame = borrow.cursor.frame().unwrap();
                    strand.store(
                        out,
                        effective_number(frame, PaxKeyword::Gid, frame.header.gid),
                    );
                    Ok(())
                })
            })
            .get("mtime", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let frame = borrow.cursor.frame().unwrap();
                    let seconds = effective_number(frame, PaxKeyword::Mtime, frame.header.mtime);
                    let time = SystemTime::UNIX_EPOCH
                        .checked_add(Duration::from_secs(seconds))
                        .ok_or_else(|| Error::overflow(strand))?;
                    dolang_ext_time::datetime(strand, time, out).into_do_decode(strand)
                })
            })
            .get("owner_name", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let frame = borrow.cursor.frame().unwrap();
                    match effective_name(frame, PaxKeyword::Uname, frame.header.uname) {
                        Some(value) => {
                            let value = str::from_utf8(value).into_do_data(strand)?;
                            strand.store(out, value)
                        }
                        None => strand.store(out, Nil),
                    }
                    Ok(())
                })
            })
            .get("group_name", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let frame = borrow.cursor.frame().unwrap();
                    match effective_name(frame, PaxKeyword::Gname, frame.header.gname) {
                        Some(value) => {
                            let value = str::from_utf8(value).into_do_data(strand)?;
                            strand.store(out, value)
                        }
                        None => strand.store(out, Nil),
                    }
                    Ok(())
                })
            })
            .get("link_name", |this, strand, out| {
                active_entry!(this, strand, borrow, {
                    let link = borrow
                        .cursor
                        .frame()
                        .unwrap()
                        .effective_link_path()
                        .map_err(|err| frame_error(strand, err))?;
                    if link.is_empty() {
                        strand.store(out, Nil);
                    } else {
                        let link = str::from_utf8(link).into_do_data(strand)?;
                        dolang_ext_shell::unix_path(strand, link, out)?;
                    }
                    Ok(())
                })
            });
        // `read` reads to `size`, and `read_some` until it has any.
        for (name, some) in [("read", false), ("read_some", true)] {
            builder = builder.method(name, async move |this, strand, args, out| {
                unpack!(strand, args => size);
                let size = usize::try_from(nonnegative_u64(strand, size, "size")?)
                    .map_err(|_| Error::overflow(strand))?;
                let child = this.borrow(strand)?;
                let generation = child.generation;
                with_instance!(
                    strand,
                    this.annex()
                        .types
                        .reader
                        .cast(Ref::slot::<0>(&child))
                        .ok_or_else(|| Error::state_error(strand, "tar entry is invalid"))?,
                    |reader| {
                        let mut borrow = reader.borrow_mut(strand)?;
                        if borrow.generation != generation {
                            return Err(Error::state_error(
                                strand,
                                "tar entry is no longer active",
                            ));
                        }
                        let mut data = BinEmbryo::new_with_capacity(strand, size);
                        let spare = &mut data.spare_capacity_mut()[..size];
                        let read = borrow
                            .cursor
                            .read(spare, some)
                            .await
                            .map_err(|err| frame_error(strand, err))?;
                        // SAFETY: `read` initialized that many spare bytes.
                        unsafe { data.advance(read) };
                        data.finish(strand, out);
                        Ok(())
                    }
                )
            });
        }
        builder
    }

    async fn iter<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        strand.store(out, this);
        Ok(())
    }

    async fn next<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, bool> {
        let child = this.borrow(strand)?;
        let generation = child.generation;
        with_instance!(
            strand,
            this.annex()
                .types
                .reader
                .cast(Ref::slot::<0>(&child))
                .ok_or_else(|| Error::state_error(strand, "tar entry is invalid"))?,
            |reader| {
                let mut borrow = reader.borrow_mut(strand)?;
                if borrow.generation != generation {
                    return Err(Error::state_error(strand, "tar entry is no longer active"));
                }
                let mut data = BinEmbryo::new_with_capacity(strand, BYTE_STREAM_CHUNK_SIZE);
                let spare = &mut data.spare_capacity_mut()[..BYTE_STREAM_CHUNK_SIZE];
                let read = borrow
                    .cursor
                    .read(spare, true)
                    .await
                    .map_err(|err| frame_error(strand, err))?;
                if read == 0 {
                    return Ok(false);
                }
                // SAFETY: `read` initialized that many spare bytes.
                unsafe { data.advance(read) };
                data.finish(strand, out);
                Ok(true)
            }
        )
    }
}

impl<'v> Object<'v> for TarEntryWriter {
    const NAME: &'v str = "EntryWriter";
    const MODULE: &'v str = "tar";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn provide<'a, 's>(
        this: Instance<'v, 'a, Self>,
        request: &mut Request<'v, 's, '_>,
    ) -> Result<'v, 's, ()> {
        if request.wants::<ByteIo>() {
            validate_entry_writer!(this, request);
            let borrow = this.borrow(request)?;
            let Some(stream) = &borrow.stream else {
                return Err(Error::state_error(request, "tar entry writer is closed"));
            };
            request.provide::<ByteIo>(&mut EntryIo { stream })?;
        }
        Ok(())
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder
            .supertype(TypeObject::Io)
            // The stream is taken when the entry's scope ends.
            .get("writable", |this, strand, out| {
                let writable = this.borrow(strand)?.stream.is_some();
                strand.store(out, writable);
                Ok(())
            });
        // A write takes all of it, so `write_some` is `write`.
        for name in ["write", "write_some"] {
            builder = builder.method(name, async move |this, strand, args, out| {
                let mut pieces = Vec::with_capacity(args.len());
                for arg in args {
                    let piece = match arg {
                        Arg::Pos(piece) => piece.into_ref(),
                        Arg::Key(key, _) => return Err(Error::unexpected_key(strand, key)),
                    };
                    pieces.push(match piece.view(strand) {
                        View::Str(value) => Piece::Str(value.pin()),
                        View::Bin(value) => Piece::Bin(value.pin()),
                        _ => return Err(Error::type_error(strand, "expected Str or Bin")),
                    });
                }
                let mut slices = pieces
                    .iter()
                    .map(|piece| IoSlice::new(piece))
                    .collect::<Vec<_>>();
                let written = slices.iter().map(|slice| slice.len()).sum::<usize>();

                validate_entry_writer!(this, strand);
                let stream = this.borrow(strand)?.stream.clone();
                let Some(mut stream) = stream else {
                    return Err(Error::state_error(strand, "tar entry writer is closed"));
                };
                let Ok(mut sink) = stream.lock().await else {
                    return Err(Error::state_error(strand, "tar entry writer is closed"));
                };
                let sink = sink.get_mut();
                if sink.check(written).is_err() {
                    return Err(Error::value(strand, "tar entry exceeds declared size"));
                }
                let mut slices = &mut slices[..];
                IoSlice::advance_slices(&mut slices, 0);
                while !slices.is_empty() {
                    let n = sink.write_vectored(slices).await.into_do_io(strand)?;
                    if n == 0 {
                        return Err(io::Error::from(io::ErrorKind::WriteZero)).into_do_io(strand);
                    }
                    IoSlice::advance_slices(&mut slices, n);
                }
                strand.store(out, written);
                Ok(())
            });
        }
        builder
    }
}

/// A pinned piece of a write.
enum Piece<'v> {
    Str(PinStr<'v>),
    Bin(PinBin<'v>),
}

impl Deref for Piece<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Str(value) => value.as_bytes(),
            Self::Bin(value) => value,
        }
    }
}

impl<'v> Object<'v> for TarWriter {
    const NAME: &'v str = "Writer";
    const MODULE: &'v str = "tar";
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let size = builder.sym("size");
        let mode = builder.sym("mode");
        let uid = builder.sym("uid");
        let gid = builder.sym("gid");
        let mtime = builder.sym("mtime");
        let owner_name = builder.sym("owner_name");
        let group_name = builder.sym("group_name");
        builder
            .get("compression", |this, strand, out| {
                let compression = this.borrow(strand)?.compression;
                strand.store(out, compression_sym(*this.annex(), compression));
                Ok(())
            })
            .method_with_slots(
                "entry",
                async move |this, strand, args, out, [mut handle]| {
                    let global = *this.annex();
                    unpack!(strand, args =>
                        path,
                        block,
                        ?:size,
                        ?:mode,
                        ?:uid,
                        ?:gid,
                        ?:mtime,
                        ?:owner_name,
                        ?:group_name
                    );
                    let path = entry_path(strand, path)?;
                    let size = size.ok_or_else(|| Error::value(strand, "size is required"))?;
                    let size = nonnegative_u64(strand, size, "size")?;
                    let opts = parse_common_entry_options(
                        strand, 0o644, mode, uid, gid, mtime, owner_name, group_name,
                    )?;

                    let sequence = next_sequence(strand, this)?;
                    let header = build_header(
                        strand,
                        sequence,
                        &path,
                        UstarKind::Regular,
                        size,
                        &opts,
                        None,
                    )?;
                    let (mut sink, generation) = take_sink(strand, this)?;
                    if let Err(err) = sink.write_all(&header).await {
                        restore_sink(strand, this, sink, false)?;
                        return Err(err.into_do_io(strand));
                    }

                    global.types.entry_writer.create_with_annex(
                        strand,
                        TarEntryWriter {
                            stream: Some(Shared::new(
                                EntrySink {
                                    writer: sink,
                                    written: 0,
                                    size,
                                    failed: false,
                                }
                                .compat_write(),
                            )),
                            generation,
                        },
                        global,
                        &mut handle,
                    );
                    let handle_cast = global.types.entry_writer.cast(&handle).unwrap();
                    with_instance!(strand, handle_cast, |handle_obj| {
                        let mut handle_borrow = handle_obj.borrow_mut_unwrap();
                        strand.store(Mut::slot_mut::<0>(&mut handle_borrow), this);
                    });

                    let block_result = call!(strand, block, &handle, out);
                    let stream = with_instance!(strand, handle_cast, |handle_obj| {
                        Ok(handle_obj.borrow_mut(strand)?.stream.take())
                    })?;
                    // Closing waits for a write in progress on a lent handle.
                    let sink = match stream {
                        Some(mut stream) => stream.close().await.map(Compat::into_inner),
                        None => None,
                    };
                    let Some(EntrySink {
                        writer: mut sink,
                        written,
                        failed,
                        ..
                    }) = sink
                    else {
                        // The output can't be recovered, so the archive can't be finished.
                        let mut parent = this.borrow_mut(strand)?;
                        parent.active = false;
                        parent.poisoned = true;
                        drop(parent);
                        block_result?;
                        return Err(Error::state_error(strand, "tar entry writer was lost"));
                    };
                    // A short entry can't be completed, so only a full one is padded.
                    let pad_result = if written == size && !failed {
                        sink.write_all(payload_padding(size)).await
                    } else {
                        Ok(())
                    };
                    let ok = written == size && !failed && pad_result.is_ok();
                    restore_sink(strand, this, sink, ok)?;

                    block_result?;
                    pad_result.into_do_io(strand)?;
                    if written != size {
                        return Err(Error::value(
                            strand,
                            format!("tar entry declared {size} bytes but received {written}"),
                        ));
                    }
                    Ok(())
                },
            )
            .method("create_dir", async move |this, strand, args, _out| {
                unpack!(strand, args =>
                    path,
                    ?:mode,
                    ?:uid,
                    ?:gid,
                    ?:mtime,
                    ?:owner_name,
                    ?:group_name
                );
                let path = entry_path(strand, path)?;
                let opts = parse_common_entry_options(
                    strand, 0o755, mode, uid, gid, mtime, owner_name, group_name,
                )?;
                append_metadata_only_entry(strand, this, &path, UstarKind::Directory, &opts, None)
                    .await
            })
            .method("symlink", async move |this, strand, args, _out| {
                unpack!(strand, args =>
                    target,
                    path,
                    ?:mode,
                    ?:uid,
                    ?:gid,
                    ?:mtime,
                    ?:owner_name,
                    ?:group_name
                );
                let target = link_target(strand, target)?;
                let path = entry_path(strand, path)?;
                let opts = parse_common_entry_options(
                    strand, 0o777, mode, uid, gid, mtime, owner_name, group_name,
                )?;
                append_metadata_only_entry(
                    strand,
                    this,
                    &path,
                    UstarKind::SymbolicLink,
                    &opts,
                    Some(&target),
                )
                .await
            })
            .method("hard_link", async move |this, strand, args, _out| {
                unpack!(strand, args =>
                    target,
                    path,
                    ?:mode,
                    ?:uid,
                    ?:gid,
                    ?:mtime,
                    ?:owner_name,
                    ?:group_name
                );
                let target = link_target(strand, target)?;
                let path = entry_path(strand, path)?;
                let opts = parse_common_entry_options(
                    strand, 0o644, mode, uid, gid, mtime, owner_name, group_name,
                )?;
                append_metadata_only_entry(
                    strand,
                    this,
                    &path,
                    UstarKind::HardLink,
                    &opts,
                    Some(&target),
                )
                .await
            })
    }
}

/// Gets the byte handle `endpoint` names: a path is opened, and any other value must
/// provide `ByteIo` with `need`.
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
            .into_do_io(strand)?;
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

async fn open_reader<'v, 's>(
    strand: &mut Strand<'v, 's>,
    handle: ByteHandle,
) -> Result<'v, 's, (FrameReader<DynReader>, Compression)> {
    let mut source = handle.into_read().expect("source is readable").compat();
    let mut magic = Vec::with_capacity(ZSTD_MAGIC.len());
    while magic.len() < ZSTD_MAGIC.len() {
        let mut bytes = [0; ZSTD_MAGIC.len()];
        let want = ZSTD_MAGIC.len() - magic.len();
        let read = source.read(&mut bytes[..want]).await.into_do_io(strand)?;
        if read == 0 {
            break;
        }
        magic.extend_from_slice(&bytes[..read]);
    }
    let compression = if magic.starts_with(GZIP_MAGIC) {
        Compression::Gzip
    } else if magic.starts_with(ZSTD_MAGIC) {
        Compression::Zstd
    } else {
        Compression::None
    };
    // Replay the detected bytes ahead of the rest of the source.
    let source = ByteHandle::read(source.into_inner())
        .with_prefix(magic)
        .into_read()
        .expect("handle is readable")
        .compat();
    let buffered = BufReader::with_capacity(BYTE_STREAM_CHUNK_SIZE, source);
    let reader: DynReader = match compression {
        Compression::None => Box::pin(buffered),
        Compression::Gzip => Box::pin(GzipDecoder::new(buffered)),
        Compression::Zstd => Box::pin(ZstdDecoder::new(buffered)),
    };
    // An archive may be followed by other data in the source, so reading stops at
    // the end marker.
    let policy = StreamPolicy::default().trailing_data_policy(TrailingDataPolicy::Ignore);
    Ok((FrameReader::new(reader).with_policy(policy), compression))
}

fn parse_compression<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<Slot<'v, '_>>,
    path: Option<&Path>,
) -> Result<'v, 's, Compression> {
    let Some(value) = value else {
        let extension = path
            .and_then(Path::extension)
            .and_then(|extension| extension.to_str());
        return Ok(match extension {
            Some(extension)
                if extension.eq_ignore_ascii_case("gz")
                    || extension.eq_ignore_ascii_case("tgz") =>
            {
                Compression::Gzip
            }
            Some(extension)
                if extension.eq_ignore_ascii_case("zst")
                    || extension.eq_ignore_ascii_case("tzst") =>
            {
                Compression::Zstd
            }
            _ => Compression::None,
        });
    };
    match value.as_sym(strand) {
        Some(sym) if sym == global.syms.none => Ok(Compression::None),
        Some(sym) if sym == global.syms.gzip => Ok(Compression::Gzip),
        Some(sym) if sym == global.syms.zstd => Ok(Compression::Zstd),
        _ => Err(Error::value(
            strand,
            "compression must be :NONE:, :GZIP:, or :ZSTD:",
        )),
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, global: State<'v, Global<'v>>) {
    let compression_sym = builder.sym("compression");
    builder
        .module("tar")
        .value("Reader", global.types.reader)
        .value("Entry", global.types.entry)
        .value("Writer", global.types.writer)
        .value("EntryWriter", global.types.entry_writer)
        .function("read", async move |strand, args, out| {
            unpack!(strand, args => source, block);
            let handle = endpoint_handle(strand, &source, IoCaps::READ, "source").await?;
            let (archive, compression) = open_reader(strand, handle).await?;
            with_slots!(strand, |mut reader| {
                global.types.reader.create_with_annex(
                    strand,
                    TarReader {
                        cursor: Cursor::new(archive),
                        generation: 0,
                        compression,
                        closed: false,
                    },
                    global,
                    &mut reader,
                );
                let result = call!(strand, block, &reader, out);
                with_instance!(
                    strand,
                    global.types.reader.cast(&reader).unwrap(),
                    |reader_obj| {
                        let mut borrow = reader_obj.borrow_mut(strand)?;
                        borrow.cursor.clear();
                        borrow.closed = true;
                        borrow.generation = borrow.generation.wrapping_add(1);
                        Ok(())
                    }
                )?;
                result
            })
        })
        .function("write", async move |strand, args, out| {
            unpack!(strand, args => sink, block, ?compression_sym: compression);
            let path = dolang_ext_shell::as_path(strand, &sink);
            let compression = parse_compression(strand, global, compression, path.as_deref())?;
            let handle = endpoint_handle(strand, &sink, IoCaps::WRITE, "sink").await?;
            let sink = handle
                .into_write()
                .expect("sink is writable")
                .compat_write();
            let output: DynWriter = match compression {
                Compression::None => Box::pin(sink),
                Compression::Gzip => Box::pin(GzipEncoder::new(sink)),
                Compression::Zstd => Box::pin(ZstdEncoder::new(sink)),
            };

            with_slots!(strand, |mut writer| {
                global.types.writer.create_with_annex(
                    strand,
                    TarWriter {
                        sink: Some(output),
                        sequence: 0,
                        generation: 0,
                        active: false,
                        poisoned: false,
                        compression,
                    },
                    global,
                    &mut writer,
                );
                let result = call!(strand, block, &writer, out);
                let (sink, poisoned, active) = with_instance!(
                    strand,
                    global.types.writer.cast(&writer).unwrap(),
                    |writer_obj| {
                        let mut borrow = writer_obj.borrow_mut(strand)?;
                        Ok((borrow.sink.take(), borrow.poisoned, borrow.active))
                    }
                )?;
                let cleanup = if active || poisoned {
                    Err(Error::state_error(
                        strand,
                        "tar writer did not finish cleanly",
                    ))
                } else {
                    async {
                        let mut sink =
                            sink.ok_or_else(|| Error::state_error(strand, "tar writer is closed"))?;
                        sink.write_all(end_marker_bytes())
                            .await
                            .into_do_io(strand)?;
                        sink.shutdown().await.into_do_io(strand)?;
                        Ok(())
                    }
                    .await
                };
                result.and(cleanup)
            })
        })
        .commit();
}
