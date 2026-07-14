use dolang::runtime::object::{Mut, Ref};
use dolang::runtime::{Object, Output, Result, State, Strand, Sym, object::TypeBuilder};
use dolang_shell_vfs::{FileType, Metadata as VfsMetadata, UnixMetadataPlatform};

use crate::{global::Global, time::create_datetime, util};

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

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let mode = builder.sym("mode");
        let dev = builder.sym("dev");
        let ino = builder.sym("ino");
        let nlink = builder.sym("nlink");
        let uid = builder.sym("uid");
        let gid = builder.sym("gid");
        let rdev = builder.sym("rdev");
        let blksize = builder.sym("blksize");
        let blocks = builder.sym("blocks");
        let win_attrs = builder.sym("win_attrs");
        let unix_flags = builder.sym("unix_flags");
        builder
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
            })
            .get("mode", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.unix().map(|unix| unix.mode),
                    mode,
                    out,
                )
            })
            .get("dev", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.unix().map(|v| v.dev), dev, out)
            })
            .get("ino", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.unix().map(|v| v.ino), ino, out)
            })
            .get("nlink", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.nlink),
                    nlink,
                    out,
                )
            })
            .get("uid", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.unix().map(|v| v.uid), uid, out)
            })
            .get("gid", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.unix().map(|v| v.gid), gid, out)
            })
            .get("rdev", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.unix().map(|v| v.rdev), rdev, out)
            })
            .get("blksize", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.blksize),
                    blksize,
                    out,
                )
            })
            .get("blocks", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.blocks),
                    blocks,
                    out,
                )
            })
            .get("win_attrs", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.windows().map(|v| v.attrs),
                    win_attrs,
                    out,
                )
            })
            .get("unix_flags", move |this, strand, out| {
                let value = this.annex().inner.unix().and_then(|v| match &v.platform {
                    UnixMetadataPlatform::Macos { flags } => Some(*flags),
                    _ => None,
                });
                util::option_field(strand, value, unix_flags, out)
            })
            .get("attrs", |this, strand, mut out| {
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
            })
    }
}
