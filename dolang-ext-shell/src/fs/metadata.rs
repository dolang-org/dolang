use dolang::runtime::{Object, Output, Result, State, Strand, Sym, object::TypeBuilder};
use dolang_shell_vfs::{FileType, Metadata as VfsMetadata};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_ENCRYPTED,
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM,
    FILE_ATTRIBUTE_TEMPORARY,
};

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

#[cfg(windows)]
fn has_attributes(attributes: u32, flag: u32) -> bool {
    attributes & flag != 0
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
    type Annex = MetadataAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let builder = builder
            .get("len", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.len as i64);
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
                Output::set(strand, out, this.annex().inner.mode as i64);
                Ok(())
            })
            .get("dev", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.dev as i64);
                Ok(())
            })
            .get("ino", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.ino as i64);
                Ok(())
            })
            .get("nlink", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.nlink as i64);
                Ok(())
            })
            .get("uid", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.uid as i64);
                Ok(())
            })
            .get("gid", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.gid as i64);
                Ok(())
            })
            .get("rdev", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.rdev as i64);
                Ok(())
            })
            .get("blksize", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.blksize as i64);
                Ok(())
            })
            .get("blocks", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.blocks as i64);
                Ok(())
            });
        #[cfg(windows)]
        let builder = builder
            .get("attributes", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.attributes as i64);
                Ok(())
            })
            .get("readonly", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_READONLY),
                );
                Ok(())
            })
            .get("hidden", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_HIDDEN),
                );
                Ok(())
            })
            .get("system", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_SYSTEM),
                );
                Ok(())
            })
            .get("archive", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_ARCHIVE),
                );
                Ok(())
            })
            .get("reparse_point", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_REPARSE_POINT),
                );
                Ok(())
            })
            .get("compressed", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_COMPRESSED),
                );
                Ok(())
            })
            .get("encrypted", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_ENCRYPTED),
                );
                Ok(())
            })
            .get("temporary", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_TEMPORARY),
                );
                Ok(())
            })
            .get("offline", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(this.annex().inner.attributes, FILE_ATTRIBUTE_OFFLINE),
                );
                Ok(())
            })
            .get("not_content_indexed", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    has_attributes(
                        this.annex().inner.attributes,
                        FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
                    ),
                );
                Ok(())
            });
        builder
    }
}
