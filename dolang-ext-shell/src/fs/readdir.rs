use std::path::PathBuf;

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand, method, object::TypeBuilder,
    value::TypeObject,
};
use dolang_shell_vfs::{DirEntry, FileType, ReadDir};

use crate::error::ErrorExt as ShellErrorExt;
use crate::global::Global;

use crate::fs::path::{Path, PathAnnex};

pub(crate) struct DirEntryIter {
    pub(crate) read_dir: ReadDir,
    pub(crate) path: PathBuf,
}

pub(crate) struct DirEntryIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

impl<'v> Object<'v> for DirEntryIter {
    const NAME: &'v str = "DirEntryIter";
    const MODULE: &'v str = "fs";
    type Annex = DirEntryIterAnnex<'v>;
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
        let mut borrow = this.borrow_mut(strand)?;
        let annex = this.annex();
        let global = annex.global;

        match borrow.read_dir.next_entry().await {
            Ok(Some(entry)) => {
                entry_to_record(&entry, &borrow.path, global, strand, out).await?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(e.into_sys(strand)),
        }
    }
}

async fn entry_to_record<'v, 's>(
    entry: &DirEntry,
    dir_path: &std::path::Path,
    global: State<'v, Global<'v>>,
    strand: &mut Strand<'v, 's>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    strand
        .with_slots(async |strand, [mut std_mod, mut record, mut tmp]| {
            strand.import("std", &mut std_mod).await?;

            let entry_path = dir_path.join(entry.file_name());

            let path_sym = global.syms.path;
            let record_sym = global.syms.record;

            global.types.path.create_with_annex(
                strand,
                Path,
                PathAnnex::new(entry_path, global),
                &mut tmp,
            );

            method!(
                strand, std_mod, record_sym, &mut record,
                path_sym: &mut tmp,
            )
            .await?;

            let ty = match entry.file_type() {
                FileType::File => global.syms.file,
                FileType::Dir => global.syms.dir,
                FileType::Symlink => global.syms.symlink,
                FileType::Fifo => global.syms.fifo,
                FileType::CharacterDevice => global.syms.char_device,
                FileType::BlockDevice => global.syms.block_device,
                FileType::Socket => global.syms.socket,
                FileType::Unknown => global.syms.unknown,
            };
            record.set(strand, global.syms.ty, ty)?;
            #[cfg(unix)]
            record.set(strand, global.syms.ino, entry.ino() as i64)?;

            Output::set(strand, out, record);
            Ok(())
        })
        .await
}
