use super::{FS_BASE, FS_UNIX, FS_WINDOWS};

use dolang::runtime::{Object, Output, State, Strand, object::TypeBuilder};
use dolang_vfs::metadata::FsMetadata as VfsFsMetadata;

use crate::{global::FsGlobal, util};

pub(crate) struct FsMetadata<const FAMILY: u8 = FS_BASE>;

pub(crate) struct FsMetadataAnnex {
    pub(crate) inner: VfsFsMetadata,
}

pub(crate) fn create_fs_metadata<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
    metadata: VfsFsMetadata,
    out: impl Output<'v>,
) {
    let unix = metadata.unix().is_some();
    let annex = FsMetadataAnnex { inner: metadata };
    if unix {
        global
            .types
            .unix_fs_metadata
            .create_with_annex(strand, FsMetadata::<FS_UNIX>, annex, out);
    } else {
        global.types.windows_fs_metadata.create_with_annex(
            strand,
            FsMetadata::<FS_WINDOWS>,
            annex,
            out,
        );
    }
}

impl<'v, const FAMILY: u8> Object<'v> for FsMetadata<FAMILY> {
    const NAME: &'v str = "FsMetadata";
    const MODULE: &'v str = match FAMILY {
        FS_UNIX => "fs.unix",
        FS_WINDOWS => "fs.windows",
        _ => "fs",
    };
    type Annex = FsMetadataAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder = builder
            .get("capacity", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.capacity());
                Ok(())
            })
            .get("free", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.free());
                Ok(())
            })
            .get("available", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.available());
                Ok(())
            })
            .get("block_size", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.block_size());
                Ok(())
            })
            .get("read_only", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.read_only());
                Ok(())
            });
        if FAMILY == FS_UNIX {
            builder = builder
                .get("blocks", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.unix().map(|v| v.blocks()), out)
                })
                .get("blocks_free", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.blocks_free()),
                        out,
                    )
                })
                .get("blocks_available", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.blocks_available()),
                        out,
                    )
                })
                .get("files", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.unix().map(|v| v.files()), out)
                })
                .get("files_free", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.files_free()),
                        out,
                    )
                })
                .get("files_available", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.files_available()),
                        out,
                    )
                })
                .get("fragment_size", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.fragment_size()),
                        out,
                    )
                })
                .get("linux_attrs", move |this, strand, out| {
                    let value = this.annex().inner.unix().and_then(|v| v.linux_flags());
                    util::nullable_field(strand, value, out)
                })
                .get("freebsd_attrs", move |this, strand, out| {
                    let value = this.annex().inner.unix().and_then(|v| v.freebsd_flags());
                    util::nullable_field(strand, value, out)
                })
                .get("macos_attrs", move |this, strand, out| {
                    let value = this.annex().inner.unix().and_then(|v| v.macos_flags());
                    util::nullable_field(strand, value, out)
                })
                .get("fsid", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().and_then(|v| v.fsid()),
                        out,
                    )
                })
                .get("name_max", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.name_max()),
                        out,
                    )
                })
                .get("no_suid", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.no_suid(), out)
                })
                .get("no_exec", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.no_exec(), out)
                })
                .get("synchronous", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.synchronous(), out)
                })
                .get("no_dev", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.no_dev(), out)
                })
                .get("no_atime", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.no_atime(), out)
                })
                .get("no_dir_atime", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.no_dir_atime(), out)
                })
                .get("relatime", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.relatime(), out)
                });
        }
        if FAMILY == FS_WINDOWS {
            builder = builder
                .get("win_flags", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex().inner.windows().map(|v| v.flags()),
                        out,
                    )
                })
                .get("volume_serial_number", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex()
                            .inner
                            .windows()
                            .map(|v| v.volume_serial_number()),
                        out,
                    )
                })
                .get("component_length_max", move |this, strand, out| {
                    util::nullable_field(
                        strand,
                        this.annex()
                            .inner
                            .windows()
                            .map(|v| v.component_length_max()),
                        out,
                    )
                });
        }
        builder
    }
}
