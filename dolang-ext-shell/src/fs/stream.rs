use std::collections::VecDeque;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value, object::TypeBuilder,
    value::TypeObject,
};
use dolang_shell_vfs::{StreamEntry as VfsStreamEntry, Vfs};

use crate::{error::ResultExt as _, global::Global};

pub(crate) struct StreamEntry;

pub(crate) struct StreamEntryAnnex {
    pub(crate) inner: VfsStreamEntry,
}

pub(crate) struct StreamIter {
    pub(crate) entries: VecDeque<VfsStreamEntry>,
}

pub(crate) struct StreamIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

pub(crate) fn create_stream_entry<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
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
    global: State<'v, Global<'v>>,
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
    base_path: &std::path::Path,
    entry: &VfsStreamEntry,
) -> std::path::PathBuf {
    let mut path = base_path.to_owned();
    let mut name = path
        .file_name()
        .expect("stream base path must have a file name")
        .to_os_string();
    name.push(":");
    name.push(entry.name.as_str());
    name.push(":$");
    name.push(entry.r#type.as_str());
    path.set_file_name(name);
    path
}

pub(crate) fn path_with_stream<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    stream: &Value<'v>,
) -> Result<'v, 's, std::path::PathBuf> {
    let stream = global
        .types
        .stream_entry
        .downcast(stream)
        .ok_or_else(|| Error::not_supported(strand))?;
    Ok(stream_path(path, &stream.annex().inner))
}

pub(crate) async fn path_list<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    follow: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let follow = follow
        .map(|follow| {
            follow
                .as_bool(strand)
                .ok_or_else(|| Error::type_error(strand, "follow: expected bool"))
        })
        .transpose()?
        .unwrap_or(true);
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let entries = local.vfs().streams(&path, follow).await.into_sys(strand)?;
    create_stream_iter(strand, global, entries, out)
}

pub(crate) async fn file_list<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    file: &tokio::fs::File,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let entries = global
        .local
        .get(strand)
        .vfs()
        .file_streams(file)
        .await
        .into_sys(strand)?;
    create_stream_iter(strand, global, entries, out)
}

impl<'v> Object<'v> for StreamEntry {
    const NAME: &'v str = "StreamEntry";
    const MODULE: &'v str = "fs";
    type Annex = StreamEntryAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.name.as_str());
                Ok(())
            })
            .get("type", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.r#type.as_str());
                Ok(())
            })
            .get("size", |this, strand, out| {
                Output::set(strand, out, i128::from(this.annex().inner.size));
                Ok(())
            })
            .get("alloc_size", |this, strand, out| {
                Output::set(strand, out, i128::from(this.annex().inner.alloc_size));
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
        match borrow.entries.pop_front() {
            Some(entry) => {
                create_stream_entry(strand, this.annex().global, entry, out);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
