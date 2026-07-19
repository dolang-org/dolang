use dolang::runtime::{Object, Output, Result, State, Strand, Sym, object::TypeBuilder};
use dolang_shell_vfs::FsMetadata as VfsFsMetadata;

use crate::{global::Global, util};

pub(crate) struct FsMetadata;

pub(crate) struct FsMetadataAnnex {
    pub(crate) inner: VfsFsMetadata,
}

pub(crate) fn create_fs_metadata<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    metadata: VfsFsMetadata,
    out: impl Output<'v>,
) {
    global.types.fs_metadata.create_with_annex(
        strand,
        FsMetadata,
        FsMetadataAnnex { inner: metadata },
        out,
    );
}

fn option_field<'v, 's, T: dolang::runtime::Input<'v>>(
    strand: &mut Strand<'v, 's>,
    value: Option<T>,
    field: Sym<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    util::option_field(strand, value, field, out)
}

impl<'v> Object<'v> for FsMetadata {
    const NAME: &'v str = "FsMetadata";
    const MODULE: &'v str = "fs";
    type Annex = FsMetadataAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let blocks = builder.sym("blocks");
        let blocks_free = builder.sym("blocks_free");
        let blocks_available = builder.sym("blocks_available");
        let files = builder.sym("files");
        let files_free = builder.sym("files_free");
        let files_available = builder.sym("files_available");
        let fragment_size = builder.sym("fragment_size");
        let linux_attrs = builder.sym("linux_attrs");
        let macos_attrs = builder.sym("macos_attrs");
        let fsid = builder.sym("fsid");
        let name_max = builder.sym("name_max");
        let no_suid = builder.sym("no_suid");
        let no_exec = builder.sym("no_exec");
        let synchronous = builder.sym("synchronous");
        let no_dev = builder.sym("no_dev");
        let no_atime = builder.sym("no_atime");
        let no_dir_atime = builder.sym("no_dir_atime");
        let relatime = builder.sym("relatime");
        let win_flags = builder.sym("win_flags");
        let volume_serial_number = builder.sym("volume_serial_number");
        let component_length_max = builder.sym("component_length_max");

        builder
            .get("capacity", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.capacity);
                Ok(())
            })
            .get("free", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.free);
                Ok(())
            })
            .get("available", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.available);
                Ok(())
            })
            .get("block_size", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.block_size);
                Ok(())
            })
            .get("read_only", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.read_only());
                Ok(())
            })
            .get("blocks", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.blocks),
                    blocks,
                    out,
                )
            })
            .get("blocks_free", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.blocks_free),
                    blocks_free,
                    out,
                )
            })
            .get("blocks_available", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.blocks_available),
                    blocks_available,
                    out,
                )
            })
            .get("files", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.files),
                    files,
                    out,
                )
            })
            .get("files_free", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.files_free),
                    files_free,
                    out,
                )
            })
            .get("files_available", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.files_available),
                    files_available,
                    out,
                )
            })
            .get("fragment_size", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.fragment_size),
                    fragment_size,
                    out,
                )
            })
            .get("linux_attrs", move |this, strand, out| {
                let value = this.annex().inner.unix().and_then(|v| match v.platform {
                    dolang_shell_vfs::UnixFsMetadataPlatform::Linux { flags } => Some(flags),
                    dolang_shell_vfs::UnixFsMetadataPlatform::Macos { .. } => None,
                });
                option_field(strand, value, linux_attrs, out)
            })
            .get("macos_attrs", move |this, strand, out| {
                let value = this.annex().inner.unix().and_then(|v| match v.platform {
                    dolang_shell_vfs::UnixFsMetadataPlatform::Linux { .. } => None,
                    dolang_shell_vfs::UnixFsMetadataPlatform::Macos { flags } => Some(flags),
                });
                option_field(strand, value, macos_attrs, out)
            })
            .get("fsid", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().and_then(|v| v.fsid),
                    fsid,
                    out,
                )
            })
            .get("name_max", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.unix().map(|v| v.name_max),
                    name_max,
                    out,
                )
            })
            .get("no_suid", move |this, strand, out| {
                option_field(strand, this.annex().inner.no_suid(), no_suid, out)
            })
            .get("no_exec", move |this, strand, out| {
                option_field(strand, this.annex().inner.no_exec(), no_exec, out)
            })
            .get("synchronous", move |this, strand, out| {
                option_field(strand, this.annex().inner.synchronous(), synchronous, out)
            })
            .get("no_dev", move |this, strand, out| {
                option_field(strand, this.annex().inner.no_dev(), no_dev, out)
            })
            .get("no_atime", move |this, strand, out| {
                option_field(strand, this.annex().inner.no_atime(), no_atime, out)
            })
            .get("no_dir_atime", move |this, strand, out| {
                option_field(strand, this.annex().inner.no_dir_atime(), no_dir_atime, out)
            })
            .get("relatime", move |this, strand, out| {
                option_field(strand, this.annex().inner.relatime(), relatime, out)
            })
            .get("win_flags", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.windows().map(|v| v.flags),
                    win_flags,
                    out,
                )
            })
            .get("volume_serial_number", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.windows().map(|v| v.volume_serial_number),
                    volume_serial_number,
                    out,
                )
            })
            .get("component_length_max", move |this, strand, out| {
                option_field(
                    strand,
                    this.annex().inner.windows().map(|v| v.component_length_max),
                    component_length_max,
                    out,
                )
            })
    }
}
