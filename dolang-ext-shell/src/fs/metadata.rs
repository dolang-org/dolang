#[cfg(any(windows, target_os = "macos"))]
use dolang::runtime::object::{Mut, Ref};
use dolang::runtime::{Object, Output, Result, State, Strand, Sym, object::TypeBuilder};
use dolang_shell_vfs::{FileType, Metadata as VfsMetadata};

use crate::{global::Global, time::create_datetime};

const NANOS_PER_SEC_I128: i128 = 1_000_000_000;

pub(crate) struct Metadata;

pub(crate) struct MetadataAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
    pub(crate) inner: VfsMetadata,
}

pub(crate) fn file_type_to_sym<'v>(
    file_type: FileType,
    global: State<'v, Global<'v>>,
) -> Sym<'v, 'v> {
    match file_type {
        FileType::File => global.syms.file,
        FileType::Dir => global.syms.dir,
        FileType::Symlink => global.syms.symlink,
        FileType::Fifo => global.syms.fifo,
        FileType::CharacterDevice => global.syms.char_device,
        FileType::BlockDevice => global.syms.block_device,
        FileType::Socket => global.syms.socket,
        FileType::Unknown => global.syms.unknown,
    }
}

fn timestamp_nanos(secs: i64, nanos: i64) -> i128 {
    i128::from(secs) * NANOS_PER_SEC_I128 + i128::from(nanos)
}

fn write_timestamp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    secs: i64,
    nanos: i64,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    create_datetime(strand, global, timestamp_nanos(secs, nanos), out)
}

pub(crate) fn create_metadata<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    metadata: VfsMetadata,
    out: impl Output<'v>,
) {
    global.types.metadata.create_with_annex(
        strand,
        Metadata,
        MetadataAnnex {
            global,
            inner: metadata,
        },
        out,
    );
}

impl<'v> Object<'v> for Metadata {
    const NAME: &'v str = "Metadata";
    const MODULE: &'v str = "fs";
    const SLOTS: usize = 1;
    type Annex = MetadataAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder
            .get("len", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.len);
                Ok(())
            })
            .get("type", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    file_type_to_sym(this.annex().inner.file_type, this.annex().global),
                );
                Ok(())
            })
            .get("modified", |this, strand, out| {
                let annex = this.annex();
                write_timestamp(
                    strand,
                    annex.global,
                    annex.inner.mtime,
                    annex.inner.mtime_nsec,
                    out,
                )
            })
            .get("accessed", |this, strand, out| {
                let annex = this.annex();
                write_timestamp(
                    strand,
                    annex.global,
                    annex.inner.atime,
                    annex.inner.atime_nsec,
                    out,
                )
            })
            .get("created", |this, strand, out| {
                let annex = this.annex();
                write_timestamp(
                    strand,
                    annex.global,
                    annex.inner.ctime,
                    annex.inner.ctime_nsec,
                    out,
                )
            });
        #[cfg(unix)]
        let builder = builder
            .get("mode", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.mode);
                Ok(())
            })
            .get("dev", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.dev);
                Ok(())
            })
            .get("ino", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.ino);
                Ok(())
            })
            .get("nlink", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.nlink);
                Ok(())
            })
            .get("uid", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.uid);
                Ok(())
            })
            .get("gid", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.gid);
                Ok(())
            })
            .get("rdev", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.rdev);
                Ok(())
            })
            .get("blksize", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.blksize);
                Ok(())
            })
            .get("blocks", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.blocks);
                Ok(())
            });
        #[cfg(windows)]
        let builder = builder.get("win_attrs", |this, strand, out| {
            Output::set(strand, out, this.annex().inner.win_attrs);
            Ok(())
        });
        #[cfg(target_os = "macos")]
        let builder = builder.get("unix_flags", |this, strand, out| {
            Output::set(strand, out, this.annex().inner.unix_flags);
            Ok(())
        });
        #[cfg(any(windows, target_os = "macos"))]
        let builder = builder.get("attrs", |this, strand, mut out| {
            let borrow = this.borrow(strand)?;
            if !Ref::slot::<0>(&borrow).is_nil() {
                Output::set(strand, out, Ref::slot::<0>(&borrow));
                return Ok(());
            }
            drop(borrow);

            let annex = this.annex();
            let attrs = annex.inner.attrs();
            crate::fs::attrs::create_attrs(strand, annex.global, attrs, &mut out);
            let mut borrow = this.borrow_mut(strand)?;
            Output::set(strand, Mut::slot_mut::<0>(&mut borrow), &out);
            Ok(())
        });
        builder
    }
}
