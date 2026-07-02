use dolang::runtime::{Object, Output, State, Strand, object::TypeBuilder};
use dolang_shell_vfs::Attrs as VfsAttrs;

use crate::{global::Global, util};

pub(crate) struct Attrs;

pub(crate) struct AttrsAnnex {
    pub(crate) inner: VfsAttrs,
}

pub(crate) fn create_attrs<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    attrs: VfsAttrs,
    out: impl Output<'v>,
) {
    global
        .types
        .attrs
        .create_with_annex(strand, Attrs, AttrsAnnex { inner: attrs }, out);
}

impl<'v> Object<'v> for Attrs {
    const NAME: &'v str = "Attrs";
    const MODULE: &'v str = "fs";
    type Annex = AttrsAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let win_attrs = builder.sym("win_attrs");
        let unix_flags = builder.sym("unix_flags");
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
            .get("win_attrs", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.win_attrs, win_attrs, out)
            })
            .get("unix_flags", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.unix_flags, unix_flags, out)
            })
            .get("readonly", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.readonly, readonly, out)
            })
            .get("hidden", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.hidden, hidden, out)
            })
            .get("system", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.system, system, out)
            })
            .get("archive", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.archive, archive, out)
            })
            .get("reparse_point", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.reparse_point, reparse_point, out)
            })
            .get("compressed", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.compressed, compressed, out)
            })
            .get("encrypted", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.encrypted, encrypted, out)
            })
            .get("temporary", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.temporary, temporary, out)
            })
            .get("offline", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.offline, offline, out)
            })
            .get("not_content_indexed", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.not_content_indexed,
                    not_content_indexed,
                    out,
                )
            })
            .get("immutable", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.immutable, immutable, out)
            })
            .get("append_only", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.append_only, append_only, out)
            })
            .get("no_dump", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.no_dump, no_dump, out)
            })
            .get("no_atime", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.no_atime, no_atime, out)
            })
            .get("no_copy_on_write", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.no_copy_on_write,
                    no_copy_on_write,
                    out,
                )
            })
            .get("dir_sync", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.dir_sync, dir_sync, out)
            })
            .get("casefold", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.casefold, casefold, out)
            })
            .get("data_journaling", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.data_journaling,
                    data_journaling,
                    out,
                )
            })
            .get("no_compress", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.no_compress, no_compress, out)
            })
            .get("project_inherit", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.project_inherit,
                    project_inherit,
                    out,
                )
            })
            .get("secure_delete", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.secure_delete, secure_delete, out)
            })
            .get("sync", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.sync, sync, out)
            })
            .get("no_tail_merge", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.no_tail_merge, no_tail_merge, out)
            })
            .get("top_dir", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.top_dir, top_dir, out)
            })
            .get("undelete", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.undelete, undelete, out)
            })
            .get("direct_access", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.direct_access, direct_access, out)
            })
            .get("extent_format", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.extent_format, extent_format, out)
            })
            .get("opaque", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.opaque, opaque, out)
            })
    }
}
