use std::collections::VecDeque;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value, object::TypeBuilder,
    value::TypeObject,
};
use dolang_vfs::file::StreamEntry as VfsStreamEntry;
use dolang_vfs::path as vfs_path;

use crate::{error::ResultExt as _, global::FsGlobal};

pub(crate) struct StreamEntry;

pub(crate) struct StreamEntryAnnex {
    pub(crate) inner: VfsStreamEntry,
}

pub(crate) struct StreamIter {
    pub(crate) entries: VecDeque<VfsStreamEntry>,
}

pub(crate) struct StreamIterAnnex<'v> {
    pub(crate) global: State<'v, FsGlobal<'v>>,
}

pub(crate) fn create_stream_entry<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
    entry: VfsStreamEntry,
    out: impl Output<'v>,
) {
    global.types.stream_entry.create_with_annex(
        strand,
        StreamEntry,
        StreamEntryAnnex { inner: entry },
        out,
    );
}

pub(crate) fn create_stream_iter<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    entries: Vec<VfsStreamEntry>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    global.types.stream_iter.create_with_annex(
        strand,
        StreamIter {
            entries: entries.into(),
        },
        StreamIterAnnex { global },
        out,
    );
    Ok(())
}

pub(crate) fn stream_path(
    base_path: vfs_path::Path<'_>,
    entry: &VfsStreamEntry,
) -> vfs_path::PathBuf {
    let spec = vfs_path::StreamSpecBuf::from(entry);
    base_path.with_stream(Some(spec.to_spec()))
}

pub(crate) fn path_with_stream<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    path: vfs_path::Path<'_>,
    stream: &Value<'v>,
) -> Result<'v, 's, vfs_path::PathBuf> {
    let stream = global
        .types
        .stream_entry
        .cast(stream)
        .ok_or_else(|| Error::not_supported(strand))?;
    Ok(stream.enter_sync(strand, |_strand, stream| {
        stream_path(path, &stream.annex().inner)
    }))
}

pub(crate) async fn path_list<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    path: vfs_path::Path<'_>,
    resolve: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let follow = super::resolve_sym(strand, global, resolve, true)?;
    let path = super::prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let entries = local
        .vfs()
        .streams(path.to_path(), follow)
        .await
        .into_sys(strand)?;
    create_stream_iter(strand, global, entries, out)
}

impl<'v> Object<'v> for StreamEntry {
    const NAME: &'v str = "StreamEntry";
    const MODULE: &'v str = "fs.windows";
    type Annex = StreamEntryAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.name());
                Ok(())
            })
            .get("type", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.stream_type());
                Ok(())
            })
            .get("size", |this, strand, out| {
                Output::set(strand, out, i128::from(this.annex().inner.size()));
                Ok(())
            })
            .get("alloc_size", |this, strand, out| {
                Output::set(strand, out, i128::from(this.annex().inner.alloc_size()));
                Ok(())
            })
    }
}

impl<'v> Object<'v> for StreamIter {
    const NAME: &'v str = "StreamIter";
    const MODULE: &'v str = "fs";
    type Annex = StreamIterAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.supertype(TypeObject::Iter)
    }

    async fn iter<'a, 's>(
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
        match borrow.entries.pop_front() {
            Some(entry) => {
                create_stream_entry(strand, this.annex().global, entry, out);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
