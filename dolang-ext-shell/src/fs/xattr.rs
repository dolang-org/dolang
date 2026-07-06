use std::collections::VecDeque;

use dolang::runtime::{
    Error, Instance, Object, Output, Result, Slot, State, Strand, Value, object::TypeBuilder,
    value::TypeObject,
};
use dolang_shell_vfs::{Vfs, XattrEntry as VfsXattrEntry, XattrNamespace};

use crate::{error::ResultExt as _, global::Global, util};

pub(crate) struct XattrEntry;

pub(crate) struct XattrEntryAnnex {
    pub(crate) inner: VfsXattrEntry,
}

pub(crate) struct XattrIter {
    pub(crate) entries: VecDeque<VfsXattrEntry>,
}

pub(crate) struct XattrIterAnnex<'v> {
    pub(crate) global: State<'v, Global<'v>>,
}

pub(crate) fn create_xattr_entry<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    entry: VfsXattrEntry,
    out: impl Output<'v>,
) {
    global.types.xattr_entry.create_with_annex(
        strand,
        XattrEntry,
        XattrEntryAnnex { inner: entry },
        out,
    );
}

pub(crate) fn create_xattr_iter<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    entries: Vec<VfsXattrEntry>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    global.types.xattr_iter.create_with_annex(
        strand,
        XattrIter {
            entries: entries.into(),
        },
        XattrIterAnnex { global },
        out,
    );
    Ok(())
}

pub(crate) fn parse_name<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
    namespace: Option<Slot<'v, '_>>,
) -> Result<'v, 's, (String, Option<String>)> {
    if let Some(entry) = global.types.xattr_entry.downcast(value) {
        if namespace.is_some() {
            return Err(Error::unexpected_key(strand, global.syms.namespace));
        }
        let entry = &entry.annex().inner;
        return Ok((entry.name.clone(), entry.namespace.clone()));
    }

    let name = util::string(strand, value, "name")?;
    let namespace = namespace
        .map(|namespace| util::string(strand, &namespace, "namespace"))
        .transpose()?;
    Ok((name, namespace))
}

pub(crate) async fn path_list<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    namespace: Option<Slot<'v, '_>>,
    follow: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let follow = follow
        .map(|follow| util::bool(strand, follow, "follow"))
        .transpose()?
        .unwrap_or(true);
    let namespace_buf;
    let namespace = match namespace {
        None => XattrNamespace::Default,
        Some(namespace) => {
            if let Some(sym) = namespace.as_sym(strand) {
                if sym == global.syms.any {
                    XattrNamespace::Any
                } else {
                    return Err(Error::value(strand, "namespace: expected str or :ANY:"));
                }
            } else if let Some(namespace) = namespace.as_str(strand) {
                namespace_buf = namespace.to_string();
                XattrNamespace::Named(&namespace_buf)
            } else {
                return Err(Error::type_error(strand, "namespace: expected str or sym"));
            }
        }
    };
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let entries = local
        .vfs()
        .xattrs(&path, namespace, follow)
        .await
        .into_sys(strand)?;
    create_xattr_iter(strand, global, entries, out)
}

pub(crate) async fn path_get<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    name: &Value<'v>,
    namespace: Option<Slot<'v, '_>>,
    follow: Option<Slot<'v, '_>>,
    out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let (name, namespace) = parse_name(strand, global, name, namespace)?;
    let follow = follow
        .map(|follow| util::bool(strand, follow, "follow"))
        .transpose()?
        .unwrap_or(true);
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let value = local
        .vfs()
        .xattr(&path, &name, namespace.as_deref(), follow)
        .await
        .into_sys(strand)?;
    Output::set(strand, out, value.as_slice());
    Ok(())
}

pub(crate) async fn path_set<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    name: &Value<'v>,
    namespace: Option<Slot<'v, '_>>,
    value: &Value<'v>,
    follow: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ()> {
    let (name, namespace) = parse_name(strand, global, name, namespace)?;
    let value = util::bytes(strand, value, "value")?;
    let follow = follow
        .map(|follow| util::bool(strand, follow, "follow"))
        .transpose()?
        .unwrap_or(true);
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    local
        .vfs()
        .set_xattr(&path, &name, namespace.as_deref(), &value, follow)
        .await
        .into_sys(strand)
}

pub(crate) async fn path_remove<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    name: &Value<'v>,
    namespace: Option<Slot<'v, '_>>,
    follow: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ()> {
    let (name, namespace) = parse_name(strand, global, name, namespace)?;
    let follow = follow
        .map(|follow| util::bool(strand, follow, "follow"))
        .transpose()?
        .unwrap_or(true);
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    local
        .vfs()
        .remove_xattr(&path, &name, namespace.as_deref(), follow)
        .await
        .into_sys(strand)
}

impl<'v> Object<'v> for XattrEntry {
    const NAME: &'v str = "XattrEntry";
    const MODULE: &'v str = "fs";
    type Annex = XattrEntryAnnex;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let namespace = builder.sym("namespace");
        let size = builder.sym("size");
        let flags = builder.sym("flags");

        builder
            .get("name", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.name.as_str());
                Ok(())
            })
            .get("namespace", move |this, strand, out| {
                util::option_field(
                    strand,
                    this.annex().inner.namespace.as_deref(),
                    namespace,
                    out,
                )
            })
            .get("size", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.size, size, out)
            })
            .get("flags", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.flags, flags, out)
            })
    }
}

impl<'v> Object<'v> for XattrIter {
    const NAME: &'v str = "XattrIter";
    const MODULE: &'v str = "fs";
    type Annex = XattrIterAnnex<'v>;
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
                create_xattr_entry(strand, this.annex().global, entry, out);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
