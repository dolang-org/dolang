use std::{
    cell::{Cell, UnsafeCell},
    mem::transmute,
};

use async_zip::{
    Compression, ZipEntryBuilder,
    base::{
        read::{WithEntry, ZipEntryReader, seek::ZipFileReader},
        write::{EntrySeekableWriter, ZipFileWriter},
    },
};
use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, call,
    error::ResultExt,
    method,
    object::{Mut, Ref, TypeBuilder},
    unpack,
    value::{TypeObject, View},
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

pub(crate) struct Archive;

pub(crate) struct ArchiveAnnex<'v> {
    global: State<'v, Global<'v>>,
    file_open: Cell<bool>,
    /// The entry reader/writer in a child `File` may borrow this value.
    ///
    /// SAFETY: `inner` is never moved or removed while an entry is open. The corresponding
    /// `File` roots its `Archive` through a GC slot, so the borrow remains valid.
    inner: UnsafeCell<Option<ArchiveInner>>,
}

impl<'v> ArchiveAnnex<'v> {
    fn new(inner: ArchiveInner, global: State<'v, Global<'v>>) -> Self {
        Self {
            global,
            file_open: Cell::new(false),
            inner: UnsafeCell::new(Some(inner)),
        }
    }

    fn is_file_open(&self) -> bool {
        self.file_open.get()
    }

    unsafe fn inner(&self) -> &Option<ArchiveInner> {
        unsafe { &*self.inner.get() }
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
        builder
            .method_with_slots(
                "entries",
                async move |this, strand, args, out, [mut iter, _tmp]| {
                    let annex = this.annex();
                    let global = annex.global;
                    let ([], []) = unpack!(strand, args, 0, 0)?;

                    if annex.is_file_open() {
                        return Err(Error::concurrency(strand));
                    }

                    let inner = unsafe {
                        annex
                            .inner()
                            .as_ref()
                            .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                    };
                    let len = match inner {
                        ArchiveInner::Read(archive) => archive.file().entries().len(),
                        ArchiveInner::Write(_) => {
                            return Err(Error::runtime(
                                strand,
                                "cannot list entries in write mode",
                            ));
                        }
                    };
                    global.types.entry_iter.create_with_annex(
                        strand,
                        EntryIter::new(len),
                        EntryIterAnnex { global },
                        &mut iter,
                    );
                    let iter_obj = global.types.entry_iter.downcast(&iter).unwrap();
                    let mut iter_borrow = iter_obj.borrow_mut_unwrap();
                    Output::set(strand, Mut::slot_mut::<0>(&mut iter_borrow), this);
                    drop(iter_borrow);

                    Output::set(strand, out, iter);
                    Ok(())
                },
            )
            .method_with_slots(
                "open",
                async move |this, strand, args, out, [mut file, mut tmp]| {
                    let annex = this.annex();
                    let global = annex.global;
                    let ([name], [block]) = unpack!(strand, args, 1, 1)?;
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
                                let entry =
                                    archive.reader_with_entry(index).await.into_do(strand)?;
                                let entry = unsafe {
                                    // SAFETY: the entry borrows `archive`, which remains in the annex until
                                    // the entry is closed. The File object roots this Archive in slot 0.
                                    transmute::<
                                        ZipEntryReader<
                                            '_,
                                            Compat<BufReader<AnyFile>>,
                                            WithEntry<'_>,
                                        >,
                                        ZipReadEntry,
                                    >(entry)
                                };
                                FileInner::Read {
                                    entry: Box::new(entry),
                                    validated: false,
                                }
                            }
                            ArchiveInner::Write(writer) => {
                                let entry = ZipEntryBuilder::new(name.into(), Compression::Deflate);
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
            .method("close", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;

                if annex.is_file_open() {
                    return Err(Error::concurrency(strand));
                }
                let inner = unsafe { annex.inner_mut().take() };
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

pub(crate) struct EntryIter {
    index: usize,
    len: usize,
}

impl EntryIter {
    fn new(len: usize) -> Self {
        Self { index: 0, len }
    }
}

pub(crate) struct EntryIterAnnex<'v> {
    global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for EntryIter {
    const NAME: &'v str = "EntryIter";
    const MODULE: &'v str = "zip";
    const SLOTS: usize = 1;
    type Annex = EntryIterAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn input<'a, 's>(
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
        let annex = this.annex();
        let global = annex.global;
        let borrow = this.borrow(strand)?;
        let index = borrow.index;
        if index == borrow.len {
            return Ok(false);
        }

        let archive = global
            .types
            .archive
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();
        if archive.annex().is_file_open() {
            return Err(Error::concurrency(strand));
        }

        let archive_annex = archive.annex();
        let inner = unsafe {
            archive_annex
                .inner()
                .as_ref()
                .ok_or_else(|| Error::state_error(strand, "archive closed"))?
        };
        let ArchiveInner::Read(archive) = inner else {
            unreachable!()
        };
        let name = archive.file().entries()[index]
            .filename()
            .as_str()
            .into_do(strand)?;
        Output::set(strand, out, name);
        drop(borrow);

        this.borrow_mut(strand)?.index = index + 1;
        Ok(true)
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
