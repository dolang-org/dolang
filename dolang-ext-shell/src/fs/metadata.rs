use dolang::runtime::{
    Object, Output, Result, State, Strand, Sym,
    object::{Instance, TypeBuilder},
};
use dolang_shell_vfs::{FileType, Metadata as VfsMetadata};

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

fn attr_field<'v, 's>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, Metadata>,
    out: impl Output<'v>,
    sym: Sym<'v, 'v>,
    windows: u32,
    linux: u32,
    macos: u32,
) -> Result<'v, 's, ()> {
    match crate::fs::attrs::flag(&this.annex().inner, windows, linux, macos) {
        crate::fs::attrs::Flag::Inapplicable => Err(dolang::runtime::Error::field(strand, sym)),
        crate::fs::attrs::Flag::Unavailable => Ok(()),
        crate::fs::attrs::Flag::Value(value) => {
            Output::set(strand, out, value);
            Ok(())
        }
    }
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
        let linux_attrs = builder.sym("linux_attrs");
        let macos_attrs = builder.sym("macos_attrs");
        let user = builder.sym("user");
        let group = builder.sym("group");
        let readonly = builder.sym("readonly");
        let hidden = builder.sym("hidden");
        let system = builder.sym("system");
        let archive = builder.sym("archive");
        let reparse_point = builder.sym("reparse_point");
        let compressed = builder.sym("compressed");
        let encrypted = builder.sym("encrypted");
        let temporary = builder.sym("temporary");
        let offline = builder.sym("offline");
        let not_content_indexed = builder.sym("not_content_indexed");
        let immutable = builder.sym("immutable");
        let append_only = builder.sym("append_only");
        let no_dump = builder.sym("no_dump");
        let no_atime = builder.sym("no_atime");
        let no_copy_on_write = builder.sym("no_copy_on_write");
        let dir_sync = builder.sym("dir_sync");
        let casefold = builder.sym("casefold");
        let data_journaling = builder.sym("data_journaling");
        let no_compress = builder.sym("no_compress");
        let project_inherit = builder.sym("project_inherit");
        let secure_delete = builder.sym("secure_delete");
        let sync = builder.sym("sync");
        let no_tail_merge = builder.sym("no_tail_merge");
        let top_dir = builder.sym("top_dir");
        let undelete = builder.sym("undelete");
        let direct_access = builder.sym("direct_access");
        let extent_format = builder.sym("extent_format");
        let opaque = builder.sym("opaque");
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
                util::option_field(strand, this.annex().inner.win_attrs(), win_attrs, out)
            })
            .get("user", move |this, strand, mut out| {
                let annex = this.annex();
                let Some(value) = annex.inner.windows().and_then(|value| value.user.clone()) else {
                    return Err(dolang::runtime::Error::field(strand, user));
                };
                crate::security::create_sid(strand, annex.global, value, &mut out);
                Ok(())
            })
            .get("group", move |this, strand, mut out| {
                let annex = this.annex();
                let Some(value) = annex.inner.windows().and_then(|value| value.group.clone())
                else {
                    return Err(dolang::runtime::Error::field(strand, group));
                };
                crate::security::create_sid(strand, annex.global, value, &mut out);
                Ok(())
            })
            .get("linux_attrs", move |this, strand, out| {
                match &this.annex().inner.family {
                    dolang_shell_vfs::MetadataFamily::Unix(dolang_shell_vfs::UnixMetadata {
                        platform: dolang_shell_vfs::UnixMetadataPlatform::Linux { attrs },
                        ..
                    }) => {
                        if let Some(attrs) = attrs {
                            Output::set(strand, out, *attrs);
                        }
                        Ok(())
                    }
                    _ => Err(dolang::runtime::Error::field(strand, linux_attrs)),
                }
            })
            .get("macos_attrs", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.macos_attrs(), macos_attrs, out)
            })
            .get("readonly", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    readonly,
                    crate::fs::attrs::windows::READONLY,
                    0,
                    0,
                )
            })
            .get("hidden", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    hidden,
                    crate::fs::attrs::windows::HIDDEN,
                    0,
                    crate::fs::attrs::macos::HIDDEN,
                )
            })
            .get("system", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    system,
                    crate::fs::attrs::windows::SYSTEM,
                    0,
                    0,
                )
            })
            .get("archive", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    archive,
                    crate::fs::attrs::windows::ARCHIVE,
                    0,
                    0,
                )
            })
            .get("reparse_point", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    reparse_point,
                    crate::fs::attrs::windows::REPARSE_POINT,
                    0,
                    0,
                )
            })
            .get("compressed", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    compressed,
                    crate::fs::attrs::windows::COMPRESSED,
                    crate::fs::attrs::linux::COMPRESSED,
                    crate::fs::attrs::macos::COMPRESSED,
                )
            })
            .get("encrypted", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    encrypted,
                    crate::fs::attrs::windows::ENCRYPTED,
                    0,
                    0,
                )
            })
            .get("temporary", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    temporary,
                    crate::fs::attrs::windows::TEMPORARY,
                    0,
                    0,
                )
            })
            .get("offline", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    offline,
                    crate::fs::attrs::windows::OFFLINE,
                    0,
                    0,
                )
            })
            .get("not_content_indexed", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    not_content_indexed,
                    crate::fs::attrs::windows::NOT_CONTENT_INDEXED,
                    0,
                    0,
                )
            })
            .get("immutable", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    immutable,
                    0,
                    crate::fs::attrs::linux::IMMUTABLE,
                    crate::fs::attrs::macos::IMMUTABLE,
                )
            })
            .get("append_only", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    append_only,
                    0,
                    crate::fs::attrs::linux::APPEND_ONLY,
                    crate::fs::attrs::macos::APPEND_ONLY,
                )
            })
            .get("no_dump", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    no_dump,
                    0,
                    crate::fs::attrs::linux::NO_DUMP,
                    crate::fs::attrs::macos::NO_DUMP,
                )
            })
            .get("no_atime", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    no_atime,
                    0,
                    crate::fs::attrs::linux::NO_ATIME,
                    0,
                )
            })
            .get("no_copy_on_write", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    no_copy_on_write,
                    0,
                    crate::fs::attrs::linux::NO_COPY_ON_WRITE,
                    0,
                )
            })
            .get("dir_sync", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    dir_sync,
                    0,
                    crate::fs::attrs::linux::DIR_SYNC,
                    0,
                )
            })
            .get("casefold", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    casefold,
                    0,
                    crate::fs::attrs::linux::CASEFOLD,
                    0,
                )
            })
            .get("data_journaling", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    data_journaling,
                    0,
                    crate::fs::attrs::linux::DATA_JOURNALING,
                    0,
                )
            })
            .get("no_compress", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    no_compress,
                    0,
                    crate::fs::attrs::linux::NO_COMPRESS,
                    0,
                )
            })
            .get("project_inherit", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    project_inherit,
                    0,
                    crate::fs::attrs::linux::PROJECT_INHERIT,
                    0,
                )
            })
            .get("secure_delete", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    secure_delete,
                    0,
                    crate::fs::attrs::linux::SECURE_DELETE,
                    0,
                )
            })
            .get("sync", move |this, strand, out| {
                attr_field(strand, this, out, sync, 0, crate::fs::attrs::linux::SYNC, 0)
            })
            .get("no_tail_merge", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    no_tail_merge,
                    0,
                    crate::fs::attrs::linux::NO_TAIL_MERGE,
                    0,
                )
            })
            .get("top_dir", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    top_dir,
                    0,
                    crate::fs::attrs::linux::TOP_DIR,
                    0,
                )
            })
            .get("undelete", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    undelete,
                    0,
                    crate::fs::attrs::linux::UNDELETE,
                    0,
                )
            })
            .get("direct_access", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    direct_access,
                    0,
                    crate::fs::attrs::linux::DIRECT_ACCESS,
                    0,
                )
            })
            .get("extent_format", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    extent_format,
                    0,
                    crate::fs::attrs::linux::EXTENT_FORMAT,
                    0,
                )
            })
            .get("opaque", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    opaque,
                    0,
                    0,
                    crate::fs::attrs::macos::OPAQUE,
                )
            })
    }
}
