use std::{
    cell::UnsafeCell,
    io::{Read, Write},
    mem::{self, transmute},
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
use tokio::task;
use zip::write::SimpleFileOptions;

use crate::global::Global;

/// Type aliases for the ZIP archive with our file type.
type ZipArchive = zip::ZipArchive<std::fs::File>;
type ZipFile<'a> = zip::read::ZipFile<'a, std::fs::File>;
type ZipWriter = zip::write::ZipWriter<std::fs::File>;

/// Inner archive type that can be either read-only or writable.
enum ArchiveInner {
    Read(ZipArchive),
    WriteAppend(Box<ZipWriter>),
}

/// State of the currently open file within the archive.
enum FileState {
    /// No file currently open
    None,
    /// File open for reading
    Read(Box<ZipFile<'static>>),
    /// File open for writing/appending
    WriteAppend,
}

/// ZIP archive object.
/// Holds the file state while ArchiveAnnex holds the underlying archive data.
pub(crate) struct Archive {
    state: FileState,
}

/// Data associated with Archive objects, stored in the annex.
pub(crate) struct ArchiveAnnex<'v> {
    global: State<'v, Global<'v>>,
    /// The underlying ZIP archive (read, write, or append mode), wrapped in UnsafeCell for interior
    /// mutability. None when closed.
    inner: UnsafeCell<Option<ArchiveInner>>,
}

impl<'v> ArchiveAnnex<'v> {
    fn new(inner: ArchiveInner, global: State<'v, Global<'v>>) -> Self {
        Self {
            global,
            inner: UnsafeCell::new(Some(inner)),
        }
    }

    /// Get a reference to the inner archive
    unsafe fn inner(&self) -> &Option<ArchiveInner> {
        unsafe { &*self.inner.get() }
    }

    /// Get a mutable reference to the inner archive
    #[expect(clippy::mut_from_ref)]
    unsafe fn inner_mut(&self) -> &mut Option<ArchiveInner> {
        unsafe { &mut *self.inner.get() }
    }
}

impl Archive {
    fn new() -> Self {
        Self {
            state: FileState::None,
        }
    }

    fn is_file_open(&self) -> bool {
        !matches!(self.state, FileState::None)
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

                    let borrow = this.borrow(strand)?;
                    // Check if a file is currently open
                    if borrow.is_file_open() {
                        return Err(Error::concurrency(strand));
                    }

                    let inner = unsafe {
                        annex
                            .inner()
                            .as_ref()
                            .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                    };

                    let len = match inner {
                        ArchiveInner::Read(archive) => archive.len(),
                        _ => {
                            return Err(Error::runtime(
                                strand,
                                "cannot list entries in write/append mode",
                            ));
                        }
                    };

                    // Create the iterator with a reference to the archive in slot 0
                    global.types.entry_iter.create_with_annex(
                        strand,
                        EntryIter::new(len),
                        EntryIterAnnex { global },
                        &mut iter,
                    );
                    // Store the archive object in slot 0 of the iterator
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
                    let name_str = name.to_string(strand)?;

                    let mut borrow = this.borrow_mut(strand)?;

                    // Check if a file is already open
                    if borrow.is_file_open() {
                        return Err(Error::concurrency(strand));
                    }

                    let inner = unsafe {
                        annex
                            .inner_mut()
                            .as_mut()
                            .ok_or_else(|| Error::state_error(strand, "archive closed"))?
                    };

                    match inner {
                        ArchiveInner::Read(archive) => {
                            // Get the file from the archive and immediately transmute to 'static
                            // This is necessary because ZipFile holds a reference to ZipArchive
                            let static_file = archive
                                .by_name(&name_str)
                                .map(|zip_file| unsafe {
                                    // Transmute the ZipFile to 'static lifetime
                                    // Safety: The ZipFile holds a reference to the ZipArchive, but we keep
                                    // the Archive object alive via a GC reference in the File's slots.
                                    transmute::<ZipFile<'_>, ZipFile<'static>>(zip_file)
                                })
                                .into_do(strand)?;

                            // Set the file state in the Archive
                            borrow.state = FileState::Read(Box::new(static_file));
                        }
                        ArchiveInner::WriteAppend(writer) => {
                            // Start a new file entry
                            writer
                                .start_file(&name_str, SimpleFileOptions::default())
                                .into_do(strand)?;

                            // Set the file state in the Archive
                            borrow.state = FileState::WriteAppend;
                        }
                    }
                    drop(borrow);

                    global
                        .types
                        .file
                        .create_with_annex(strand, File, global, &mut file);
                    // Store the archive object in slot 0 of the file
                    let file_obj = global.types.file.downcast(&file).unwrap();
                    let mut file_borrow = file_obj.borrow_mut_unwrap();
                    Output::set(strand, Mut::slot_mut::<0>(&mut file_borrow), this);
                    drop(file_borrow);

                    if let Some(block) = block {
                        // Call the block with the file handle as argument
                        let result = call!(strand, block, out, &file).await;

                        // Always close the file, even on error
                        strand
                            .with_interrupt_mask(true, async move |strand| {
                                let _ = method!(strand, &file, close, &mut tmp).await;
                            })
                            .await;

                        result
                    } else {
                        // Just return the handle
                        Output::set(strand, out, file);
                        Ok(())
                    }
                },
            )
            .method("close", async move |this, strand, args, _out| {
                let annex = this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;

                let mut borrow = this.borrow_mut(strand)?;
                // Reset file state to None (invalidates any outstanding File handles)
                borrow.state = FileState::None;
                drop(borrow);

                // For write/append modes, finish the archive before closing
                if let Some(ArchiveInner::WriteAppend(writer)) = unsafe { annex.inner_mut().take() }
                {
                    task::spawn_blocking(move || writer.finish())
                        .await
                        .into_do(strand)?
                        .into_do(strand)?;
                }
                Ok(())
            })
    }
}

/// Iterator over ZIP archive entries (file names).
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
    const SLOTS: usize = 1; // Slot 0: reference to Archive
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

        // First, check bounds and get archive reference using immutable borrow
        let borrow = this.borrow(strand)?;
        let index = borrow.index;
        if index == borrow.len {
            return Ok(false);
        }

        // Get the archive reference from slot 0
        let archive = global
            .types
            .archive
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();
        let archive_borrow = archive.borrow(strand)?;

        // Check if a file is currently open
        if archive_borrow.is_file_open() {
            return Err(Error::concurrency(strand));
        }

        drop(archive_borrow);

        let archive_annex = archive.annex();

        // Get the inner archive
        let inner = unsafe {
            archive_annex
                .inner()
                .as_ref()
                .ok_or_else(|| Error::state_error(strand, "archive closed"))?
        };

        let ArchiveInner::Read(archive) = inner else {
            unreachable!()
        };
        let name = archive.name_for_index(borrow.index).unwrap();
        Output::set(strand, out, name);

        drop(borrow);

        // Now use mutable borrow to increment the index
        let mut borrow = this.borrow_mut(strand)?;
        borrow.index = index + 1;
        Ok(true)
    }
}

/// Marker type for file handles within a ZIP archive.
/// The actual file state is stored in the parent Archive.
pub(crate) struct File;

impl<'v> Object<'v> for File {
    const NAME: &'v str = "File";
    const MODULE: &'v str = "zip";
    const SLOTS: usize = 1; // Slot 0: reference to Archive
    type Annex = State<'v, Global<'v>>;
    type Type = ();
    type TypeAnnex = ();

    fn clear<'a>(this: Instance<'v, 'a, Self>) {
        let global = this.annex();
        // Get the archive reference from slot 0 and reset its file state
        let borrow = this.borrow_unwrap();
        let archive = global
            .types
            .archive
            .downcast(Ref::slot::<0>(&borrow))
            .unwrap();

        let mut archive_borrow = archive.borrow_mut_unwrap();
        // Reset file state to None
        archive_borrow.state = FileState::None;
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .method("read", async move |this, strand, args, out| {
                let global = *this.annex();
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size: usize = size
                    .to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "expected integer"))?
                    .try_into()
                    .map_err(|_| Error::overflow(strand))?;

                // Get the archive reference from slot 0
                let borrow = this.borrow(strand)?;
                let archive = global
                    .types
                    .archive
                    .downcast(Ref::slot::<0>(&borrow))
                    .ok_or_else(|| Error::state_error(strand, "invalid archive reference"))?;
                let mut archive_borrow = archive.borrow_mut(strand)?;

                // Get the file read handle from the archive's file state
                let mut inner = match mem::replace(&mut archive_borrow.state, FileState::None) {
                    FileState::Read(inner) => inner,
                    FileState::None => return Err(Error::state_error(strand, "file closed")),
                    _ => unreachable!(),
                };

                drop(archive_borrow);

                // Hand-off dance with blocking task
                let (inner, res) = task::spawn_blocking(move || {
                    let mut buffer = vec![0u8; size];
                    let data = inner.read(&mut buffer).map(move |n| {
                        buffer.truncate(n);
                        buffer
                    });
                    (inner, data)
                })
                .await
                .into_do(strand)?;

                // Put inner handle back
                let mut archive_borrow = archive.borrow_mut(strand)?;
                archive_borrow.state = FileState::Read(inner);
                drop(archive_borrow);
                let input = res.into_do(strand)?;
                Output::set(strand, out, input.as_slice());
                Ok(())
            })
            .method("write", async move |this, strand, args, _out| {
                let global = *this.annex();
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let data = match data.view(strand) {
                    View::Str(s) => s.to_string().into(),
                    View::Bin(b) => b.to_vec(),
                    _ => return Err(Error::type_error(strand, "expected bytes")),
                };

                // Get the archive reference from slot 0
                let borrow = this.borrow(strand)?;
                let archive = global
                    .types
                    .archive
                    .downcast(Ref::slot::<0>(&borrow))
                    .ok_or_else(|| Error::state_error(strand, "invalid archive reference"))?;

                // Check file state and get the inner archive
                let inner = {
                    let archive_borrow = archive.borrow(strand)?;
                    match &archive_borrow.state {
                        FileState::WriteAppend => (),
                        FileState::None => return Err(Error::state_error(strand, "file closed")),
                        _ => unreachable!(),
                    }

                    let annex = archive.annex();
                    unsafe { (*annex.inner.get()).take().unwrap() }
                };

                // Write the data and get back the writer
                let (inner, result) = match inner {
                    ArchiveInner::WriteAppend(mut writer) => {
                        let (writer, res) = task::spawn_blocking(move || {
                            let res = writer.write_all(&data);
                            (writer, res)
                        })
                        .await
                        .into_do(strand)?;
                        (ArchiveInner::WriteAppend(writer), res.into_do(strand))
                    }
                    ArchiveInner::Read(archive) => (
                        ArchiveInner::Read(archive),
                        Err(Error::state_error(strand, "cannot write in read mode")),
                    ),
                };

                // Put the archive handle back
                let annex = archive.annex();
                unsafe { (*annex.inner.get()) = Some(inner) }
                result
            })
            .method("close", async move |this, strand, args, _out| {
                let global = *this.annex();
                let ([], []) = unpack!(strand, args, 0, 0)?;

                // Get the archive reference from slot 0 and reset its file state
                let borrow = this.borrow(strand)?;
                let archive = global
                    .types
                    .archive
                    .downcast(Ref::slot::<0>(&borrow))
                    .unwrap();

                let mut archive_borrow = archive.borrow_mut(strand)?;
                // Reset file state to None
                archive_borrow.state = FileState::None;
                Ok(())
            })
    }
}

/// Configure the VM with ZIP types and the `open` function.
pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let close = builder.sym("close");
    // Register the zip.open function
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

            // Parse mode parameter (default to "r" for read)
            let mode = mode
                .as_ref()
                .map(|m| {
                    m.as_str(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected `str` for `mode`"))
                        .map(|m| m.to_string())
                })
                .transpose()?
                .unwrap_or_else(|| "r".to_owned());

            // Open the archive based on mode
            let inner = match mode.as_str() {
                "r" => {
                    // Read mode
                    let file = dolang_ext_shell::open(strand, path.as_ref(), "r")
                        .await
                        .into_do(strand)?
                        .into_std()
                        .await;

                    let archive = task::spawn_blocking(move || ZipArchive::new(file))
                        .await
                        .into_do(strand)?
                        .into_do(strand)?;

                    ArchiveInner::Read(archive)
                }
                "w" => {
                    // Write mode (create/truncate)
                    let file = dolang_ext_shell::open(strand, path.as_ref(), "w")
                        .await
                        .into_do(strand)?
                        .into_std()
                        .await;

                    let writer = ZipWriter::new(file);
                    ArchiveInner::WriteAppend(Box::new(writer))
                }
                "a" => {
                    // Append mode
                    let file = dolang_ext_shell::open(strand, path.as_ref(), "r+")
                        .await
                        .into_do(strand)?
                        .into_std()
                        .await;

                    let writer = task::spawn_blocking(move || ZipWriter::new_append(file))
                        .await
                        .into_do(strand)?
                        .into_do(strand)?;

                    ArchiveInner::WriteAppend(Box::new(writer))
                }
                _ => {
                    return Err(Error::value(
                        strand,
                        format!("invalid mode '{}': expected 'r', 'w', or 'a'", mode),
                    ));
                }
            };

            if let Some(block) = block {
                // Block scope mode: create archive, call block with auto-close
                strand
                    .with_slots(async |strand, [mut wrapper, mut tmp]| {
                        global.types.archive.create_with_annex(
                            strand,
                            Archive::new(),
                            ArchiveAnnex::new(inner, global),
                            &mut wrapper,
                        );

                        // Call the block with the archive handle as argument
                        let result = call!(strand, block, out, &wrapper).await;

                        // Always close the archive, even on error
                        strand
                            .with_interrupt_mask(true, async move |strand| {
                                let _ = method!(strand, &wrapper, close, &mut tmp).await;
                            })
                            .await;

                        result
                    })
                    .await
            } else {
                // No block: just return the archive handle
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
