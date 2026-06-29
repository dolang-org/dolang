use std::fmt;
use std::path::PathBuf;

use dolang::runtime::{
    Instance, Object, Output, Result, Slot, State, Strand, error::ResultExt as _,
    object::TypeBuilder, value::TypeObject,
};
use dolang_shell_vfs::{DirEntry as VfsDirEntry, FileType, ReadDir};

use crate::error::ErrorExt as ShellErrorExt;
use crate::global::Global;

use crate::fs::metadata::file_type_to_sym;
use crate::fs::path::{Path, PathAnnex};

pub(crate) struct DirEntry;

pub(crate) struct DirEntryAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) file_type: FileType,
    #[cfg(unix)]
    pub(crate) ino: u64,
}

pub(crate) struct DirEntryIter {
    pub(crate) read_dir: ReadDir,
    pub(crate) path: PathBuf,
}

pub(crate) struct DirEntryIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

pub(crate) fn create_dir_entry<'v>(
    strand: &mut Strand<'v, '_>,
    entry: &VfsDirEntry,
    dir_path: &std::path::Path,
    global: State<'v, Global<'v>>,
    out: Slot<'v, '_>,
) {
    let path = dir_path.join(entry.file_name());
    let name = entry.file_name().to_string_lossy().into_owned();
    global.types.dir_entry.create_with_annex(
        strand,
        DirEntry,
        DirEntryAnnex {
            global,
            path,
            name,
            file_type: entry.file_type(),
            #[cfg(unix)]
            ino: entry.ino(),
        },
        out,
    );
}

impl<'v> Object<'v> for DirEntry {
    const NAME: &'v str = "DirEntry";
    const MODULE: &'v str = "fs";
    type Annex = DirEntryAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<fs.DirEntry {:?}>", this.annex().path).into_do(strand)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder
            .get("path", |this, strand, out| {
                let annex = this.annex();
                annex.global.types.path.create_with_annex(
                    strand,
                    Path,
                    PathAnnex::new(annex.path.clone(), annex.global),
                    out,
                );
                Ok(())
            })
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().name.as_str());
                Ok(())
            })
            .get("type", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    file_type_to_sym(this.annex().file_type, this.annex().global),
                );
                Ok(())
            });
        #[cfg(unix)]
        let builder = builder.get("ino", |this, strand, out| {
            Output::set(strand, out, this.annex().ino);
            Ok(())
        });
        builder
    }
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
                create_dir_entry(strand, &entry, &borrow.path, global, out);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(e.into_sys(strand)),
        }
    }
}
