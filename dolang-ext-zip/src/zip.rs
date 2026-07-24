use std::{
    cell::{Cell, UnsafeCell},
    mem::transmute,
};

use async_zip::{
    Compression, StoredZipEntry, ZipEntryBuilder, ZipFile,
    base::{
        read::{WithEntry, ZipEntryReader, seek::ZipFileReader},
        write::{EntrySeekableWriter, ZipFileWriter},
    },
};
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Sym, call,
    error::ResultExt,
    method,
    object::{ArrayLike, ArrayView, Mut, Ref, TypeBuilder},
    unpack,
    value::View,
    vm::Builder,
};
use dolang_ext_shell::FileHandle as _;
use dolang_shell_vfs::AnyFile;
use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::io::BufReader;
use tokio_util::compat::Compat;

use crate::global::Global;

type ZipReader = ZipFileReader<Compat<BufReader<AnyFile>>>;
type ZipReadEntry = ZipEntryReader<'static, Compat<BufReader<AnyFile>>, WithEntry<'static>>;
type ZipWriter = ZipFileWriter<Compat<AnyFile>>;
type ZipWriteEntry = EntrySeekableWriter<'static, Compat<AnyFile>>;

enum ArchiveInner {
    Read(ZipReader),
    Write(Box<ZipWriter>),
}

enum FileInner {
    Read {
        entry: Box<ZipReadEntry>,
        validated: bool,
    },
    Write(Box<ZipWriteEntry>),
}

// Standard POSIX `S_IFMT` file-type bits, stored in the upper bits of a
// ZIP entry's Unix external file attribute alongside its permission bits.
const TYPE_MASK: u16 = 0o170000;
const FILE_TYPE: u16 = 0o100000;
const DIR_TYPE: u16 = 0o040000;
const SYMLINK_TYPE: u16 = 0o120000;
const FIFO_TYPE: u16 = 0o010000;
const CHAR_DEVICE_TYPE: u16 = 0o020000;
const BLOCK_DEVICE_TYPE: u16 = 0o060000;
const SOCKET_TYPE: u16 = 0o140000;

/// Symbols for classifying Unix mode bits into a file-type symbol, used by
/// `Entry`'s `type` getter. Matches the vocabulary `fs.Metadata.type` uses.
#[derive(Clone, Copy)]
struct TypeSyms<'v> {
    file: Sym<'v, 'v>,
    dir: Sym<'v, 'v>,
    symlink: Sym<'v, 'v>,
    fifo: Sym<'v, 'v>,
    char_device: Sym<'v, 'v>,
    block_device: Sym<'v, 'v>,
    socket: Sym<'v, 'v>,
    unknown: Sym<'v, 'v>,
}

fn register_type_syms<'v, 'a, T: Object<'v>>(builder: &mut TypeBuilder<'v, 'a, T>) -> TypeSyms<'v> {
    TypeSyms {
        file: builder.sym("FILE"),
        dir: builder.sym("DIR"),
        symlink: builder.sym("SYMLINK"),
        fifo: builder.sym("FIFO"),
        char_device: builder.sym("CHAR_DEVICE"),
        block_device: builder.sym("BLOCK_DEVICE"),
        socket: builder.sym("SOCKET"),
        unknown: builder.sym("UNKNOWN"),
    }
}

impl<'v> TypeSyms<'v> {
    /// Classifies Unix mode bits (if present) into a file-type symbol.
    fn bits_to_sym(&self, mode: Option<u16>) -> Sym<'v, 'v> {
        match mode.map(|mode| mode & TYPE_MASK) {
            Some(SYMLINK_TYPE) => self.symlink,
            Some(FIFO_TYPE) => self.fifo,
            Some(CHAR_DEVICE_TYPE) => self.char_device,
            Some(BLOCK_DEVICE_TYPE) => self.block_device,
            Some(SOCKET_TYPE) => self.socket,
            Some(DIR_TYPE) => self.dir,
            Some(FILE_TYPE) | Some(0) | None => self.file,
            _ => self.unknown,
        }
    }
}

/// Parses `Archive.open`/`Archive.create_dir`'s `mode:` parameter: Unix
/// permission bits only (not file-type bits). Defaults to 0 when omitted.
fn parse_mode_bits<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mode: Option<Slot<'v, '_>>,
) -> Result<'v, 's, u16> {
    let Some(mode) = mode else {
        return Ok(0);
    };
    let bits = mode
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, "mode: expected non-negative int"))?;
    u16::try_from(bits)
        .ok()
        .filter(|bits| *bits <= 0o7777)
        .ok_or_else(|| Error::value(strand, "mode: expected permission bits in range 0..=0o7777"))
}

/// Opens the entry at `index` for reading. Shared by `Archive::open` (after
/// resolving a name to an index via linear scan) and `Entry::open` (which
/// already knows its index).
async fn open_read_entry_by_index<'v, 's>(
    strand: &mut Strand<'v, 's>,
    archive: &mut ZipReader,
    index: usize,
) -> Result<'v, 's, FileInner> {
    let entry = archive.reader_with_entry(index).await.into_do(strand)?;
    let entry = unsafe {
        // SAFETY: the entry borrows `archive`, which remains in the annex until
        // the entry is closed. The File object roots this Archive in slot 0.
        transmute::<ZipEntryReader<'_, Compat<BufReader<AnyFile>>, WithEntry<'_>>, ZipReadEntry>(
            entry,
        )
    };
    Ok(FileInner::Read {
        entry: Box::new(entry),
        validated: false,
    })
}

pub(crate) struct Archive;

pub(crate) struct ArchiveAnnex<'v> {
    global: State<'v, Global<'v>>,
    file_open: Cell<bool>,
    closed: Cell<bool>,
    /// A snapshot of the central directory, cloned out at open time.
    ///
    /// `None` for write-mode archives. This is deliberately independent of
    /// `inner`'s live `ZipReader`: an open `File`'s reader holds (through an
    /// unsafe transmute) what is really still a live `&mut` borrow of that
    /// `ZipReader`, so any other code path must never take even a shared
    /// reference into it while a `File` is open. Reading entry metadata
    /// through this clone instead avoids that aliasing hazard entirely,
    /// letting metadata getters on `Entry`/`Entries` work regardless of
    /// whether a `File` is currently open.
    entries: Option<ZipFile>,
    /// The entry reader/writer in a child `File` may borrow this value.
    ///
    /// SAFETY: `inner` is never moved or removed while an entry is open. The corresponding
    /// `File` roots its `Archive` through a GC slot, so the borrow remains valid.
    inner: UnsafeCell<Option<ArchiveInner>>,
}

impl<'v> ArchiveAnnex<'v> {
    fn new(inner: ArchiveInner, global: State<'v, Global<'v>>) -> Self {
        let entries = match &inner {
            ArchiveInner::Read(reader) => Some(reader.file().clone()),
            ArchiveInner::Write(_) => None,
        };
        Self {
            global,
            file_open: Cell::new(false),
            closed: Cell::new(false),
            entries,
            inner: UnsafeCell::new(Some(inner)),
        }
    }

    fn is_file_open(&self) -> bool {
        self.file_open.get()
    }

    #[expect(clippy::mut_from_ref)]
    unsafe fn inner_mut(&self) -> &mut Option<ArchiveInner> {
        unsafe { &mut *self.inner.get() }
    }
}

impl Archive {
    fn new() -> Self {
        Self
    }
}

impl<'v> Object<'v> for Archive {
    const NAME: &'v str = "Archive";
    const MODULE: &'v str = "zip";
    type Annex = ArchiveAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let close = builder.sym("close");
        let mode_sym = builder.sym("mode");
        builder
            .get("entries", |this, strand, out| {
                let annex = this.annex();
                if annex.entries.is_none() {
                    return Err(Error::runtime(strand, "cannot list entries in write mode"));
                }
                if annex.closed.get() {
                    return Err(Error::state_error(strand, "archive closed"));
                }

                Output::set(strand, out, ArrayView::<Entries>::new(this));
                Ok(())
            })
            .method_with_slots(
                "open",
                async move |this, strand, args, out, [mut file, mut tmp]| {
                    let annex = this.annex();
                    let global = annex.global;
                    let ([name], [block, mode]) = unpack!(strand, args, 1, 1, mode_sym = None)?;
                    let name = name.to_string(strand)?;

                    if annex.is_file_open() {
                        return Err(Error::concurrency(strand));
                    }

                    let inner = {
                        let inner = unsafe {
                            annex
                                .inner_mut()
                                .as_mut()
                                .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                        };
                        match inner {
                            ArchiveInner::Read(archive) => {
                                if mode.is_some() {
                                    return Err(Error::type_error(
                                        strand,
                                        "mode is only valid when creating entries in write mode",
                                    ));
                                }
                                let mut index = None;
                                for (i, entry) in archive.file().entries().iter().enumerate() {
                                    if entry.filename().as_str().into_do(strand)? == name {
                                        index = Some(i);
                                        break;
                                    }
                                }
                                let index = index.ok_or_else(|| {
                                    Error::runtime(strand, "specified file not found in archive")
                                })?;
                                open_read_entry_by_index(strand, archive, index).await?
                            }
                            ArchiveInner::Write(writer) => {
                                let mode_bits = parse_mode_bits(strand, mode)?;
                                let mut entry =
                                    ZipEntryBuilder::new(name.into(), Compression::Deflate);
                                entry = entry.unix_permissions(FILE_TYPE | mode_bits);
                                let entry =
                                    writer.write_entry_seekable(entry).await.into_do(strand)?;
                                let entry = unsafe {
                                    // SAFETY: the entry borrows `writer`, which remains in the annex until
                                    // the entry is closed. The File object roots this Archive in slot 0.
                                    transmute::<
                                        EntrySeekableWriter<'_, Compat<AnyFile>>,
                                        ZipWriteEntry,
                                    >(entry)
                                };
                                FileInner::Write(Box::new(entry))
                            }
                        }
                    };

                    annex.file_open.set(true);
                    global.types.file.create_with_annex(
                        strand,
                        File::new(inner),
                        global,
                        &mut file,
                    );
                    let file_obj = global.types.file.downcast(&file).unwrap();
                    let mut file_borrow = file_obj.borrow_mut_unwrap();
                    Output::set(strand, Mut::slot_mut::<0>(&mut file_borrow), this);
                    drop(file_borrow);

                    if let Some(block) = block {
                        let result = call!(strand, block, out, &file).await;
                        let close_result = strand
                            .with_interrupt_mask(true, async move |strand| {
                                method!(strand, &file, close, &mut tmp).await
                            })
                            .await;
                        result.and(close_result)
                    } else {
                        Output::set(strand, out, file);
                        Ok(())
                    }
                },
            )
            .method("create_dir", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([name], [mode]) = unpack!(strand, args, 1, 0, mode_sym = None)?;
                let mut name = name.to_string(strand)?;
                if !name.ends_with('/') {
                    name.push('/');
                }

                if annex.is_file_open() {
                    return Err(Error::concurrency(strand));
                }
                let mode_bits = parse_mode_bits(strand, mode)?;

                let inner = unsafe {
                    annex
                        .inner_mut()
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                };
                let ArchiveInner::Write(writer) = inner else {
                    return Err(Error::runtime(
                        strand,
                        "cannot create directory entries in read mode",
                    ));
                };

                let mut entry = ZipEntryBuilder::new(name.into(), Compression::Stored);
                entry = entry.unix_permissions(DIR_TYPE | mode_bits);
                let entry_writer = writer.write_entry_seekable(entry).await.into_do(strand)?;
                EntrySeekableWriter::close(entry_writer)
                    .await
                    .into_do(strand)
            })
            .method("symlink", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([target, name], [mode]) = unpack!(strand, args, 2, 0, mode_sym = None)?;
                let target = target.to_string(strand)?;
                let name = name.to_string(strand)?;

                if annex.is_file_open() {
                    return Err(Error::concurrency(strand));
                }
                let mode_bits = parse_mode_bits(strand, mode)?;

                let inner = unsafe {
                    annex
                        .inner_mut()
                        .as_mut()
                        .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                };
                let ArchiveInner::Write(writer) = inner else {
                    return Err(Error::runtime(
                        strand,
                        "cannot create symlink entries in read mode",
                    ));
                };

                let mut entry = ZipEntryBuilder::new(name.into(), Compression::Stored);
                entry = entry.unix_permissions(SYMLINK_TYPE | mode_bits);
                let mut entry_writer = writer.write_entry_seekable(entry).await.into_do(strand)?;
                entry_writer
                    .write_all(target.as_bytes())
                    .await
                    .into_do(strand)?;
                EntrySeekableWriter::close(entry_writer)
                    .await
                    .into_do(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;

                if annex.is_file_open() {
                    return Err(Error::concurrency(strand));
                }
                let inner = unsafe { annex.inner_mut().take() };
                annex.closed.set(true);
                match inner {
                    Some(ArchiveInner::Read(reader)) => {
                        let file = reader.into_inner().into_inner().into_inner();
                        file.close().await.into_do(strand)?;
                    }
                    Some(ArchiveInner::Write(writer)) => {
                        let file = ZipFileWriter::close(*writer)
                            .await
                            .into_do(strand)?
                            .into_inner();
                        file.close().await.into_do(strand)?;
                    }
                    None => {}
                }
                Ok(())
            })
    }
}

/// A single archive entry's metadata, and a handle to open it for reading.
///
/// Immutable once constructed: all state lives in `EntryAnnex`. Slot 0 roots
/// the owning `Archive`, both to keep it alive via GC and to reach
/// `ArchiveAnnex` for every metadata read/open call.
pub(crate) struct Entry;

pub(crate) struct EntryAnnex<'v> {
    global: State<'v, Global<'v>>,
    index: usize,
}

/// Runs `f` against the archive entry at `this`'s index, after re-validating
/// that the owning archive is still open. Never caches anything from a
/// previous call: entry metadata is re-read fresh every time, since this is
/// the only way to safely notice a concurrent `close()` on the archive.
///
/// Reads from `ArchiveAnnex::entries` (a clone of the central directory taken
/// at open time), NOT from the live `ZipReader` in `inner`. This is required
/// for soundness, not just ergonomics: while a `File` is open, its reader
/// holds (through an unsafe transmute) what is really still a live `&mut`
/// borrow of that `ZipReader`. Reading through `inner` here — even just
/// immutably — would alias that borrow, which is undefined behavior
/// regardless of whether it happens to work in practice. Going through the
/// independent clone sidesteps the hazard entirely, so entry metadata reads
/// are always safe, including reading metadata for the very entry a caller
/// is in the middle of reading via `entry.open do |file| ...`. Only opening
/// a *new* reader (`Entry::open`/`Archive::open`) touches `inner`, and those
/// still need the `file_open` guard.
fn with_entry<'v, 's, R>(
    this: Instance<'v, '_, Entry>,
    strand: &mut Strand<'v, 's>,
    f: impl FnOnce(&mut Strand<'v, 's>, &StoredZipEntry) -> Result<'v, 's, R>,
) -> Result<'v, 's, R> {
    let annex = this.annex();
    let borrow = this.borrow(strand)?;
    let archive = annex
        .global
        .types
        .archive
        .downcast(Ref::slot::<0>(&borrow))
        .expect("Entry always roots its Archive in slot 0");

    let archive_annex = archive.annex();
    if archive_annex.closed.get() {
        return Err(Error::state_error(strand, "archive closed"));
    }
    let entries = archive_annex
        .entries
        .as_ref()
        .expect("Entry can only exist for a read-mode archive");
    let entry = entries
        .entries()
        .get(annex.index)
        .expect("entry count is fixed for the archive's lifetime");
    f(strand, entry)
}

fn compression_sym<'v>(
    compression: Compression,
    stored: Sym<'v, 'v>,
    deflate: Sym<'v, 'v>,
    zstd: Sym<'v, 'v>,
    unknown: Sym<'v, 'v>,
) -> Sym<'v, 'v> {
    match compression {
        Compression::Stored => stored,
        Compression::Deflate => deflate,
        Compression::Zstd => zstd,
        _ => unknown,
    }
}

impl<'v> Object<'v> for Entry {
    const NAME: &'v str = "Entry";
    const MODULE: &'v str = "zip";
    const SLOTS: usize = 1;
    type Annex = EntryAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let close = builder.sym("close");
        let stored = builder.sym("STORED");
        let deflate = builder.sym("DEFLATE");
        let zstd = builder.sym("ZSTD");
        let unknown = builder.sym("UNKNOWN");
        let type_syms = register_type_syms(&mut builder);
        builder
            .get("name", |this, strand, out| {
                let name = with_entry(this, strand, |strand, entry| {
                    entry
                        .filename()
                        .as_str()
                        .into_do(strand)
                        .map(|s| s.to_string())
                })?;
                dolang_ext_shell::unix_path(strand, name, out)
            })
            .get("size", |this, strand, out| {
                let size =
                    with_entry(this, strand, |_strand, entry| Ok(entry.uncompressed_size()))?;
                Output::set(strand, out, size);
                Ok(())
            })
            .get("compressed_size", |this, strand, out| {
                let size = with_entry(this, strand, |_strand, entry| Ok(entry.compressed_size()))?;
                Output::set(strand, out, size);
                Ok(())
            })
            .get("crc32", |this, strand, out| {
                let crc32 = with_entry(this, strand, |_strand, entry| Ok(entry.crc32()))?;
                Output::set(strand, out, crc32);
                Ok(())
            })
            .get("compression", move |this, strand, out| {
                let sym = with_entry(this, strand, |_strand, entry| {
                    Ok(compression_sym(
                        entry.compression(),
                        stored,
                        deflate,
                        zstd,
                        unknown,
                    ))
                })?;
                Output::set(strand, out, sym);
                Ok(())
            })
            .get("mode", |this, strand, out| {
                let mode = with_entry(this, strand, |_strand, entry| Ok(entry.unix_permissions()))?;
                if let Some(mode) = mode {
                    Output::set(strand, out, mode);
                }
                Ok(())
            })
            .get("type", move |this, strand, out| {
                let sym = with_entry(this, strand, |strand, entry| {
                    if entry.dir().into_do(strand)? {
                        return Ok(type_syms.dir);
                    }
                    Ok(type_syms.bits_to_sym(entry.unix_permissions()))
                })?;
                Output::set(strand, out, sym);
                Ok(())
            })
            .get("comment", |this, strand, out| {
                let comment = with_entry(this, strand, |strand, entry| {
                    entry
                        .comment()
                        .as_str()
                        .into_do(strand)
                        .map(|s| s.to_string())
                })?;
                Output::set(strand, out, comment.as_str());
                Ok(())
            })
            .get("last_modified", |this, strand, out| {
                let civil = with_entry(this, strand, |_strand, entry| {
                    Ok(entry.last_modification_date().as_jiff().ok())
                })?;
                let Some(civil) = civil else {
                    return Ok(());
                };
                let Ok(zoned) = civil.to_zoned(jiff::tz::TimeZone::UTC) else {
                    return Ok(());
                };
                let timestamp = zoned.timestamp();
                let Ok(secs) = u64::try_from(timestamp.as_second()) else {
                    return Ok(());
                };
                let system_time = std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::new(secs, timestamp.subsec_nanosecond() as u32);
                dolang_ext_shell::datetime(strand, system_time, out).into_do(strand)
            })
            .method_with_slots(
                "open",
                async move |this, strand, args, out, [mut file, mut tmp]| {
                    let ([], [block]) = unpack!(strand, args, 0, 1)?;
                    let annex = this.annex();
                    let global = annex.global;
                    let index = annex.index;

                    let borrow = this.borrow(strand)?;
                    let archive = global
                        .types
                        .archive
                        .downcast(Ref::slot::<0>(&borrow))
                        .expect("Entry always roots its Archive in slot 0");
                    let archive_annex = archive.annex();

                    if archive_annex.is_file_open() {
                        return Err(Error::concurrency(strand));
                    }
                    let inner = unsafe {
                        archive_annex
                            .inner_mut()
                            .as_mut()
                            .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                    };
                    let ArchiveInner::Read(zip_archive) = inner else {
                        unreachable!("Entry can only exist for a read-mode archive")
                    };
                    let file_inner = open_read_entry_by_index(strand, zip_archive, index).await?;

                    archive_annex.file_open.set(true);
                    global.types.file.create_with_annex(
                        strand,
                        File::new(file_inner),
                        global,
                        &mut file,
                    );
                    let file_obj = global.types.file.downcast(&file).unwrap();
                    let mut file_borrow = file_obj.borrow_mut_unwrap();
                    Output::set(strand, Mut::slot_mut::<0>(&mut file_borrow), archive);
                    drop(file_borrow);

                    if let Some(block) = block {
                        let result = call!(strand, block, out, &file).await;
                        let close_result = strand
                            .with_interrupt_mask(true, async move |strand| {
                                method!(strand, &file, close, &mut tmp).await
                            })
                            .await;
                        result.and(close_result)
                    } else {
                        Output::set(strand, out, file);
                        Ok(())
                    }
                },
            )
    }
}

struct Entries;

impl<'v> ArrayLike<'v> for Entries {
    type Object = Archive;
    const MODULE: &'v str = "zip";
    const NAME: &'v str = "Entries";

    fn len(this: Instance<'v, '_, Archive>, _strand: &mut Strand<'v, '_>) -> usize {
        let annex = this.annex();
        if annex.closed.get() {
            return 0;
        }
        annex
            .entries
            .as_ref()
            .map_or(0, |entries| entries.entries().len())
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, Archive>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        mut out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let annex = this.annex();
        let global = annex.global;
        if annex.entries.is_none() {
            unreachable!("entries getter only valid in read mode")
        }
        if annex.closed.get() {
            return Err(Error::state_error(strand, "archive closed"));
        }

        global
            .types
            .entry
            .create_with_annex(strand, Entry, EntryAnnex { global, index }, &mut out);
        let entry_obj = global.types.entry.downcast(&out).unwrap();
        let mut entry_borrow = entry_obj.borrow_mut_unwrap();
        Output::set(strand, Mut::slot_mut::<0>(&mut entry_borrow), this);
        Ok(())
    }
}

pub(crate) struct File {
    inner: Option<FileInner>,
}

impl File {
    fn new(inner: FileInner) -> Self {
        Self { inner: Some(inner) }
    }
}

impl<'v> Object<'v> for File {
    const NAME: &'v str = "File";
    const MODULE: &'v str = "zip";
    const SLOTS: usize = 1;
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn finalize<'a>(this: Instance<'v, 'a, Self>) {
        let global = this.annex();
        // Drop the entry borrow before allowing another file to borrow the archive.
        let inner = this.borrow_mut_unwrap().inner.take();
        if inner.is_none() {
            return;
        }

        let borrow = this.borrow_unwrap();
        let archive = global
            .types
            .archive
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();
        archive.annex().file_open.set(false);
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("read", async move |this, strand, args, out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size: usize = size
                    .to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "expected integer"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?;

                let mut borrow = this.borrow_mut(strand)?;
                let inner = borrow
                    .inner
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file closed"))?;
                let FileInner::Read { entry, validated } = inner else {
                    return Err(Error::state_error(strand, "cannot read in write mode"));
                };

                let mut data = vec![0; size];
                let read = entry.read(&mut data).await.into_do(strand)?;
                data.truncate(read);
                if read == 0 && !*validated {
                    if entry.compute_hash() != entry.entry().crc32() {
                        return Err(Error::runtime(strand, "CRC32 checksum mismatch"));
                    }
                    *validated = true;
                }

                drop(borrow);
                Output::set(strand, out, data.as_slice());
                Ok(())
            })
            .method("write", async move |this, strand, args, _out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let data = match data.view(strand) {
                    View::Str(s) => s.to_string().into_bytes(),
                    View::Bin(b) => b.to_vec(),
                    _ => return Err(Error::type_error(strand, "expected bytes")),
                };

                let mut borrow = this.borrow_mut(strand)?;
                let inner = borrow
                    .inner
                    .as_mut()
                    .ok_or_else(|| Error::state_error(strand, "file closed"))?;
                let FileInner::Write(entry) = inner else {
                    return Err(Error::state_error(strand, "cannot write in read mode"));
                };
                entry.write_all(&data).await.into_do(strand)
            })
            .method("close", async move |this, strand, args, _out| {
                let global = *this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let Some(inner) = this.borrow_mut(strand)?.inner.take() else {
                    return Ok(());
                };

                let borrow = this.borrow(strand)?;
                let archive = global
                    .types
                    .archive
                    .downcast(Ref::slot::<0>(&borrow))
                    .ok_or_else(|| Error::state_error(strand, "invalid archive reference"))?;

                let result = match inner {
                    FileInner::Write(entry) => {
                        EntrySeekableWriter::close(*entry).await.into_do(strand)
                    }
                    FileInner::Read { .. } => Ok(()),
                };
                archive.annex().file_open.set(false);
                result
            })
    }
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let close = builder.sym("close");
    builder
        .module("zip")
        .function("open", async move |strand, args, out| {
            let ([path], [opt1, opt2]) = unpack!(strand, args, 1, 2)?;
            let path = dolang_ext_shell::as_path(strand, &path)
                .ok_or_else(|| Error::type_error(strand, "expected `str` or `Path` for `path`"))?;

            let (mode, block) = match (&opt1, &opt2) {
                (None, None) => (None, None),
                (Some(first), None) if first.as_str(strand).is_some() => (opt1, None),
                (Some(_), None) => (None, opt1),
                (Some(_), Some(_)) => (opt1, opt2),
                (None, Some(_)) => unreachable!(),
            };
            let mode = mode
                .as_ref()
                .map(|mode| {
                    mode.as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected `str` for `mode`"))
                        .map(|mode| mode.to_string())
                })
                .transpose()?
                .unwrap_or_else(|| "r".to_owned());

            let inner = match mode.as_str() {
                "r" => {
                    let file = dolang_ext_shell::open(strand, path.as_ref(), "r")
                        .await
                        .into_do(strand)?;
                    let reader = ZipReader::with_tokio(BufReader::new(file))
                        .await
                        .into_do(strand)?;
                    ArchiveInner::Read(reader)
                }
                "w" => {
                    let file = dolang_ext_shell::open(strand, path.as_ref(), "w")
                        .await
                        .into_do(strand)?;
                    ArchiveInner::Write(Box::new(ZipWriter::with_tokio(file)))
                }
                _ => {
                    return Err(Error::value(
                        strand,
                        format!("invalid mode '{mode}': expected 'r' or 'w'"),
                    ));
                }
            };

            if let Some(block) = block {
                strand
                    .with_slots(async |strand, [mut wrapper, mut tmp]| {
                        global.types.archive.create_with_annex(
                            strand,
                            Archive::new(),
                            ArchiveAnnex::new(inner, global),
                            &mut wrapper,
                        );
                        let result = call!(strand, block, out, &wrapper).await;
                        let close_result = strand
                            .with_interrupt_mask(true, async move |strand| {
                                method!(strand, &wrapper, close, &mut tmp).await
                            })
                            .await;
                        result.and(close_result)
                    })
                    .await
            } else {
                global.types.archive.create_with_annex(
                    strand,
                    Archive::new(),
                    ArchiveAnnex::new(inner, global),
                    out,
                );
                Ok(())
            }
        })
        .commit();
}
