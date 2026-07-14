use std::fmt;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value, error::ResultExt as _,
    object::TypeBuilder, value::TypeObject,
};
use dolang_shell_vfs::{
    DirEntry as VfsDirEntry, FileType, ReadDir, Utf8TypedPath, Utf8TypedPathBuf,
};

use crate::error::ErrorExt as ShellErrorExt;
use crate::global::Global;

use crate::fs::metadata::file_type_to_sym;

pub(crate) struct DirEntry;

pub(crate) struct DirEntryAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) name: String,
    pub(crate) file_type: FileType,
    #[cfg(unix)]
    pub(crate) ino: u64,
}

pub(crate) struct DirEntryIter {
    pub(crate) read_dir: ReadDir,
}

pub(crate) struct DirEntryIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

pub(crate) fn create_dir_entry<'v, 's>(
    strand: &mut Strand<'v, 's>,
    entry: &VfsDirEntry,
    global: State<'v, Global<'v>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let name = entry.file_name().to_string_lossy().into_owned();
    global.types.dir_entry.create_with_annex(
        strand,
        DirEntry,
        DirEntryAnnex {
            global,
            name,
            file_type: entry.file_type(),
            #[cfg(unix)]
            ino: entry.ino(),
        },
        out,
    );
    Ok(())
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
        write!(w, "<fs.DirEntry {:?}>", this.annex().name).into_do(strand)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder
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
                create_dir_entry(strand, &entry, global, out)?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(e.into_sys(strand)),
        }
    }
}

pub(crate) fn path_with_entry<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    entry: &Value<'v>,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    let entry = global
        .types
        .dir_entry
        .downcast(entry)
        .ok_or_else(|| Error::not_supported(strand))?;
    Ok(path.join(entry.annex().name.as_str()))
}
