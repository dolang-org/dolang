#[cfg(windows)]
use std::path::Prefix;
use std::{
    borrow::Cow,
    collections::VecDeque,
    fmt,
    hash::{Hash, Hasher},
    path::{self, Component, PathBuf},
};

use crate::{
    error::ResultExt as _,
    fs::{path_absolute, path_relative},
    global::Global,
};
use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    error::ResultExt,
    object::{TypeBuilder, Unpack, UnpackItem},
    unpack,
    value::TypeObject,
};
use dolang_shell_vfs::{Attrs, Vfs};

use super::file::File;

pub(crate) struct Path;

pub(crate) struct PathComponentsIter {
    components: VecDeque<String>,
}

pub(crate) struct PathAnnex<'v> {
    pub(crate) inner: path::PathBuf,
    pub(crate) global: State<'v, Global<'v>>,
    #[cfg(windows)]
    stream_name: Option<String>,
    #[cfg(windows)]
    stream_type: Option<String>,
}

pub(crate) fn path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, PathBuf> {
    if let Some(path) = global.types.path.downcast(value) {
        Ok(path.annex().as_path().into_owned())
    } else if let Some(str) = value.as_str(strand) {
        Ok(strand.access(|x| path::Path::new(str.as_str(x)).to_owned()))
    } else {
        Err(Error::type_error(strand, "expected Path or str"))
    }
}

fn create_path<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathBuf,
    out: Slot<'v, 'a>,
) -> Result<'v, 's, ()> {
    let annex = PathAnnex::try_new(strand, path, global)?;
    global
        .types
        .path
        .create_with_annex(strand, Path, annex, out);
    Ok(())
}

fn expect_str<'v, 's>(strand: &mut Strand<'v, 's>, value: &Value<'v>) -> Result<'v, 's, String> {
    value
        .as_str(strand)
        .map(|value| value.to_string())
        .ok_or_else(|| Error::type_error(strand, "expected str"))
}

fn rewrite_path<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    annex: &PathAnnex<'v>,
    path: &path::Path,
    out: Slot<'v, 'a>,
    rewrite: impl FnOnce(&mut PathBuf),
) -> Result<'v, 's, ()> {
    let mut path = path.to_owned();
    rewrite(&mut path);
    let next = annex.with_path(strand, path)?;
    annex
        .global
        .types
        .path
        .create_with_annex(strand, Path, next, out);
    Ok(())
}

fn with_stem_path(path: &path::Path, stem: &str) -> PathBuf {
    let mut path = path.to_owned();
    let ext = path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned());
    match ext {
        Some(ext) => path.set_file_name(format!("{stem}.{ext}")),
        None => path.set_file_name(stem),
    }
    path
}

impl<'v> PathAnnex<'v> {
    pub(crate) fn try_new<'s>(
        strand: &mut Strand<'v, 's>,
        path: path::PathBuf,
        global: State<'v, Global<'v>>,
    ) -> Result<'v, 's, Self> {
        #[cfg(windows)]
        let (path, stream_name, stream_type) = split_windows_ads(strand, path)?;
        #[cfg(not(windows))]
        let _ = strand;

        // Canonicalize by splitting into components and rejoining
        // This naturally uses platform-native separator
        Ok(Self {
            inner: path.components().collect(),
            global,
            #[cfg(windows)]
            stream_name,
            #[cfg(windows)]
            stream_type,
        })
    }

    pub(crate) fn new(path: path::PathBuf, global: State<'v, Global<'v>>) -> Self {
        Self {
            inner: path.components().collect(),
            global,
            #[cfg(windows)]
            stream_name: None,
            #[cfg(windows)]
            stream_type: None,
        }
    }

    pub(crate) fn as_path(&self) -> Cow<'_, path::Path> {
        #[cfg(windows)]
        {
            let Some(stream_name) = &self.stream_name else {
                return Cow::Borrowed(&self.inner);
            };
            let Some(name) = self.inner.file_name() else {
                return Cow::Borrowed(&self.inner);
            };
            let mut name = name.to_string_lossy().into_owned();
            name.push(':');
            name.push_str(stream_name);
            if let Some(stream_type) = &self.stream_type {
                name.push_str(":$");
                name.push_str(stream_type);
            }
            Cow::Owned(self.inner.with_file_name(name))
        }
        #[cfg(not(windows))]
        {
            Cow::Borrowed(&self.inner)
        }
    }

    fn with_path<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        path: path::PathBuf,
    ) -> Result<'v, 's, Self> {
        let annex = Self::try_new(strand, path, self.global)?;
        #[cfg(windows)]
        {
            let mut annex = annex;
            annex.stream_name = self.stream_name.clone();
            annex.stream_type = self.stream_type.clone();
            Ok(annex)
        }
        #[cfg(not(windows))]
        {
            Ok(annex)
        }
    }

    /// Returns the path with forward slashes as separator for platform-consistent display
    #[cfg(target_os = "windows")]
    fn forward_slash_display(&self) -> String {
        self.as_path().to_string_lossy().replace('\\', "/")
    }

    #[cfg(not(target_os = "windows"))]
    fn forward_slash_display(&self) -> path::Display<'_> {
        self.inner.display()
    }

    #[cfg(windows)]
    fn windows_prefix(&self) -> Option<Prefix<'_>> {
        match self.inner.components().next() {
            Some(Component::Prefix(prefix)) => Some(prefix.kind()),
            _ => None,
        }
    }
}

#[cfg(windows)]
fn split_windows_ads<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut path: path::PathBuf,
) -> Result<'v, 's, (path::PathBuf, Option<String>, Option<String>)> {
    let Some(file_name) = path.file_name() else {
        return Ok((path, None, None));
    };
    let file_name = file_name.to_string_lossy().into_owned();
    let parts = file_name.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        [_base] => Ok((path, None, None)),
        [base, stream_name] => {
            path.set_file_name(base);
            Ok((path, Some((*stream_name).to_owned()), None))
        }
        [base, stream_name, stream_type] if stream_type.starts_with('$') => {
            path.set_file_name(base);
            Ok((
                path,
                Some((*stream_name).to_owned()),
                Some(stream_type[1..].to_owned()),
            ))
        }
        [_base, _stream_name, _stream_type] => Err(Error::value(
            strand,
            "explicit alternate data stream type must start with `$`",
        )),
        _ => Err(Error::value(
            strand,
            "path final component has too many alternate data stream parts",
        )),
    }
}

/// Normalize a path by removing `.` and `..` components without filesystem I/O.
pub(crate) fn normalize_path(path: &std::path::Path) -> PathBuf {
    let mut acc = Vec::new();
    let mut parent_count = 0;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if acc.len() > parent_count {
                    acc.pop();
                } else {
                    // Nothing to pop, track the leading ..
                    acc.push(Component::ParentDir);
                    parent_count += 1;
                }
            }
            _ => acc.push(component),
        }
    }
    let mut res = PathBuf::new();
    res.extend(acc);
    res
}

impl<'v> Object<'v> for PathComponentsIter {
    const NAME: &'v str = "PathComponentsIter";
    const MODULE: &'v str = "fs";
    type Annex = ();
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
        match this.borrow_mut(strand)?.components.pop_front() {
            Some(component) => {
                Output::set(strand, out, component.as_str());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn unpack<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        mut unpack: Unpack<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        if let Some(key) = unpack.first_required_key() {
            return Err(Error::missing_key(strand, key));
        }

        let required_pos = unpack.required();
        let optional_pos = unpack.optional();
        let total_pos = required_pos + optional_pos;
        let available = this.borrow(strand)?.components.len();

        if available < required_pos {
            return Err(Error::missing_positional(strand, available));
        }

        if unpack.exhaustive() && available > total_pos {
            return Err(Error::unexpected_positional(strand, total_pos));
        }

        let mut pos_index = 0usize;
        for item in unpack.iter() {
            match item {
                UnpackItem::Pos { mut slot, default } => {
                    if pos_index < available {
                        if !Self::next(this, strand, Slot::reborrow(&mut slot)).await? {
                            unreachable!("checked availability above")
                        }
                    } else {
                        Output::set(strand, slot, default.unwrap());
                    }
                    pos_index += 1;
                }
                UnpackItem::SymKey { slot, default, .. }
                | UnpackItem::ConstKey { slot, default, .. } => {
                    Output::set(strand, slot, default.unwrap());
                }
                UnpackItem::Rest { slot } => Output::set(strand, slot, this),
            }
        }
        Ok(())
    }
}

impl<'v> Object<'v> for Path {
    const NAME: &'v str = "Path";
    const MODULE: &'v str = "fs";
    type Annex = PathAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let ([path], []) = unpack!(strand, args, 1, 0)?;
        let path = path_from_value(strand, global, &path)?.to_owned();
        let annex = PathAnnex::try_new(strand, path, global)?;
        this.create_with_annex(strand, Path, annex, out);
        Ok(())
    }

    fn debug<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "<Path {:?}>", this.annex().forward_slash_display()).into_do(strand)
    }

    /// Display with forward slashes for platform-consistent Do representation
    fn display<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().forward_slash_display()).into_do(strand)
    }

    /// Display with canonical platform path separator for external program arguments
    fn display_arg<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        w: &mut dyn fmt::Write,
    ) -> Result<'v, 's, ()> {
        write!(w, "{}", this.annex().as_path().display()).into_do(strand)
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let all = builder.sym("all");
        let ignore = builder.sym("ignore");
        let max_depth = builder.sym("max_depth");
        let follow = builder.sym("follow");
        let modified = builder.sym("modified");
        let accessed = builder.sym("accessed");
        let created = builder.sym("created");
        let readonly = builder.sym("readonly");
        let hidden = builder.sym("hidden");
        let system = builder.sym("system");
        let archive = builder.sym("archive");
        let compressed = builder.sym("compressed");
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
        let builder = builder
            .get("name", |this, strand, out| {
                let borrow = this.annex();
                if let Some(n) = borrow.inner.file_name() {
                    Output::set(strand, out, n.to_string_lossy().as_ref());
                }
                Ok(())
            })
            .get("stem", |this, strand, out| {
                let borrow = this.annex();
                if let Some(stem) = borrow.inner.file_stem() {
                    Output::set(strand, out, stem.to_string_lossy().as_ref());
                }
                Ok(())
            })
            .get("parent", |this, strand, out| {
                let borrow = this.annex();
                if let Some(path) = borrow.inner.parent() {
                    create_path(strand, borrow.global, path.to_owned(), out)?;
                }
                Ok(())
            })
            .get("ext", |this, strand, out| {
                let borrow = this.annex();
                if let Some(e) = borrow.inner.extension() {
                    Output::set(strand, out, e.to_string_lossy().as_ref());
                }
                Ok(())
            })
            .get("is_absolute", |this, strand, out| {
                let borrow = this.annex();
                Output::set(strand, out, borrow.inner.is_absolute());
                Ok(())
            })
            .method("open", async move |this, strand, args, out| {
                let ([], [opt1, opt2]) = unpack!(strand, args, 0, 2)?;
                let annex = this.annex();
                File::open(strand, annex.global, &annex.as_path(), opt1, opt2, out).await
            })
            .method("metadata", async move |this, strand, args, out| {
                let ([], [follow]) = unpack!(strand, args, 0, 1)?;
                let follow = match follow {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => true,
                };
                let annex = this.annex();
                super::metadata(strand, annex.global, &annex.as_path(), follow, out).await
            })
            .method("attrs", async move |this, strand, args, out| {
                let ([], [follow]) = unpack!(strand, args, 0, 0, follow = None)?;
                let follow = match follow {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => true,
                };
                let annex = this.annex();
                super::get_attrs(strand, annex.global, &annex.as_path(), follow, out).await
            })
            .method("exists", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                super::exists(strand, annex.global, &annex.as_path(), out).await
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [mode]) = unpack!(strand, args, 0, 1)?;
                let annex = this.annex();
                super::read(strand, annex.global, &annex.as_path(), mode, out).await
            })
            .method("write", async move |this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                let annex = this.annex();
                super::write(strand, annex.global, &annex.as_path(), data, out).await
            })
            .method("set_len", async move |this, strand, args, _out| {
                let ([size], []) = unpack!(strand, args, 1, 0)?;
                let size = size.to_i64(strand).map_err(|_| {
                    Error::type_error(strand, "size must be a non-negative integer")
                })?;
                let size = u64::try_from(size).map_err(|_| {
                    Error::type_error(strand, "size must be a non-negative integer")
                })?;
                let annex = this.annex();
                super::set_len(strand, annex.global, &annex.as_path(), size).await
            })
            .method("copy", async move |this, strand, args, _out| {
                let ([to], [all]) = unpack!(strand, args, 1, 0, all = None)?;
                let all = match all {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let to = path_from_value(strand, this.annex().global, &to)?;
                let annex = this.annex();
                super::copy(strand, annex.global, &annex.as_path(), &to, all).await
            })
            .method("rename", async move |this, strand, args, _out| {
                let ([to], []) = unpack!(strand, args, 1, 0)?;
                let to = path_from_value(strand, this.annex().global, &to)?;
                let annex = this.annex();
                super::rename(strand, annex.global, &annex.as_path(), &to).await
            })
            .method("move", async move |this, strand, args, _out| {
                let ([to], [all]) = unpack!(strand, args, 1, 0, all = None)?;
                let all = match all {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let to = path_from_value(strand, this.annex().global, &to)?;
                let annex = this.annex();
                super::move_(strand, annex.global, &annex.as_path(), &to, all).await
            })
            .method("hard_link", async move |this, strand, args, _out| {
                let ([to], []) = unpack!(strand, args, 1, 0)?;
                let to = path_from_value(strand, this.annex().global, &to)?;
                let annex = this.annex();
                super::hard_link(strand, annex.global, &annex.as_path(), &to).await
            })
            .method("entries", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                super::entries(strand, annex.global, annex.as_path().into_owned(), out).await
            })
            .method("canonical", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                super::path_canonical(strand, annex.global, &annex.as_path(), out).await
            })
            .method("read_link", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                let global = annex.global;
                let local = global.local.get(strand);
                let path = local.cwd().join(annex.as_path());
                let vfs = local.vfs();
                let target = vfs.read_link(&path).await.into_sys(strand)?;
                let annex = PathAnnex::try_new(strand, target, global)?;
                global
                    .types
                    .path
                    .create_with_annex(strand, Path, annex, out);
                Ok(())
            })
            .method("remove", async move |this, strand, args, _out| {
                let ([], [all, ignore]) = unpack!(strand, args, 0, 0, all = None, ignore = None)?;
                let all = match all {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let ignore = match ignore {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let annex = this.annex();
                super::remove(strand, annex.global, &annex.as_path(), all, ignore).await
            })
            .method("create_dir", async move |this, strand, args, _out| {
                let ([], [all]) = unpack!(strand, args, 0, 0, all = None)?;
                let all = match all {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let annex = this.annex();
                super::create_dir(strand, annex.global, &annex.as_path(), all).await
            })
            .method("remove_dir", async move |this, strand, args, _out| {
                let ([], [all, ignore]) = unpack!(strand, args, 0, 0, all = None, ignore = None)?;
                let all = match all {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let ignore = match ignore {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                let annex = this.annex();
                super::remove_dir(strand, annex.global, &annex.as_path(), all, ignore).await
            })
            .method("chmod", async move |this, strand, args, _out| {
                let ([mode], []) = unpack!(strand, args, 1, 0)?;
                let mode = mode
                    .to_i64(strand)
                    .map_err(|_| Error::type_error(strand, "expected int"))?
                    as u32;
                let annex = this.annex();
                super::chmod(strand, annex.global, &annex.as_path(), mode).await
            })
            .method("set_attrs", async move |this, strand, args, _out| {
                let (
                    [],
                    [
                        readonly,
                        hidden,
                        system,
                        archive,
                        compressed,
                        temporary,
                        offline,
                        not_content_indexed,
                        immutable,
                        append_only,
                        no_dump,
                        no_atime,
                        no_copy_on_write,
                        dir_sync,
                        casefold,
                        data_journaling,
                        no_compress,
                        project_inherit,
                        secure_delete,
                        sync,
                        no_tail_merge,
                        top_dir,
                        undelete,
                        direct_access,
                        extent_format,
                        opaque,
                    ],
                ) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    readonly = None,
                    hidden = None,
                    system = None,
                    archive = None,
                    compressed = None,
                    temporary = None,
                    offline = None,
                    not_content_indexed = None,
                    immutable = None,
                    append_only = None,
                    no_dump = None,
                    no_atime = None,
                    no_copy_on_write = None,
                    dir_sync = None,
                    casefold = None,
                    data_journaling = None,
                    no_compress = None,
                    project_inherit = None,
                    secure_delete = None,
                    sync = None,
                    no_tail_merge = None,
                    top_dir = None,
                    undelete = None,
                    direct_access = None,
                    extent_format = None,
                    opaque = None
                )?;
                let attrs = Attrs {
                    readonly: super::parse_attr_bool(strand, readonly)?,
                    hidden: super::parse_attr_bool(strand, hidden)?,
                    system: super::parse_attr_bool(strand, system)?,
                    archive: super::parse_attr_bool(strand, archive)?,
                    compressed: super::parse_attr_bool(strand, compressed)?,
                    temporary: super::parse_attr_bool(strand, temporary)?,
                    offline: super::parse_attr_bool(strand, offline)?,
                    not_content_indexed: super::parse_attr_bool(strand, not_content_indexed)?,
                    immutable: super::parse_attr_bool(strand, immutable)?,
                    append_only: super::parse_attr_bool(strand, append_only)?,
                    no_dump: super::parse_attr_bool(strand, no_dump)?,
                    no_atime: super::parse_attr_bool(strand, no_atime)?,
                    no_copy_on_write: super::parse_attr_bool(strand, no_copy_on_write)?,
                    dir_sync: super::parse_attr_bool(strand, dir_sync)?,
                    casefold: super::parse_attr_bool(strand, casefold)?,
                    data_journaling: super::parse_attr_bool(strand, data_journaling)?,
                    no_compress: super::parse_attr_bool(strand, no_compress)?,
                    project_inherit: super::parse_attr_bool(strand, project_inherit)?,
                    secure_delete: super::parse_attr_bool(strand, secure_delete)?,
                    sync: super::parse_attr_bool(strand, sync)?,
                    no_tail_merge: super::parse_attr_bool(strand, no_tail_merge)?,
                    top_dir: super::parse_attr_bool(strand, top_dir)?,
                    undelete: super::parse_attr_bool(strand, undelete)?,
                    direct_access: super::parse_attr_bool(strand, direct_access)?,
                    extent_format: super::parse_attr_bool(strand, extent_format)?,
                    opaque: super::parse_attr_bool(strand, opaque)?,
                    ..Attrs::default()
                };
                let annex = this.annex();
                super::set_attrs(strand, annex.global, &annex.as_path(), attrs).await
            })
            .method("set_timestamps", async move |this, strand, args, _out| {
                let ([], [modified, accessed, created]) = unpack!(
                    strand,
                    args,
                    0,
                    0,
                    modified = None,
                    accessed = None,
                    created = None
                )?;
                let annex = this.annex();
                super::set_timestamps(
                    strand,
                    annex.global,
                    &annex.as_path(),
                    modified,
                    accessed,
                    created,
                )
                .await
            });
        #[cfg(windows)]
        let builder = builder
            .get("stream_name", |this, strand, out| {
                if let Some(stream_name) = &this.annex().stream_name {
                    Output::set(strand, out, stream_name.as_str());
                }
                Ok(())
            })
            .get("stream_type", |this, strand, out| {
                if let Some(stream_type) = &this.annex().stream_type {
                    Output::set(strand, out, stream_type.as_str());
                }
                Ok(())
            })
            .get("disk", |this, strand, out| {
                let annex = this.annex();
                let Some(prefix) = annex.windows_prefix() else {
                    return Ok(());
                };
                let disk = match prefix {
                    Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => Some(char::from(disk)),
                    _ => None,
                };
                if let Some(disk) = disk {
                    let disk = disk.to_string();
                    Output::set(strand, out, disk.as_str());
                }
                Ok(())
            })
            .get("server", |this, strand, out| {
                let annex = this.annex();
                let Some(prefix) = annex.windows_prefix() else {
                    return Ok(());
                };
                let server = match prefix {
                    Prefix::UNC(server, _) | Prefix::VerbatimUNC(server, _) => Some(server),
                    _ => None,
                };
                if let Some(server) = server {
                    Output::set(strand, out, server.to_string_lossy().as_ref());
                }
                Ok(())
            })
            .get("share", |this, strand, out| {
                let annex = this.annex();
                let Some(prefix) = annex.windows_prefix() else {
                    return Ok(());
                };
                let share = match prefix {
                    Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share) => Some(share),
                    _ => None,
                };
                if let Some(share) = share {
                    Output::set(strand, out, share.to_string_lossy().as_ref());
                }
                Ok(())
            })
            .get("device", |this, strand, out| {
                let annex = this.annex();
                let Some(prefix) = annex.windows_prefix() else {
                    return Ok(());
                };
                if let Prefix::DeviceNS(device) = prefix {
                    Output::set(strand, out, device.to_string_lossy().as_ref());
                }
                Ok(())
            })
            .get("verbatim", |this, strand, out| {
                let annex = this.annex();
                let verbatim = annex
                    .windows_prefix()
                    .map(|prefix| prefix.is_verbatim())
                    .unwrap_or(false);
                Output::set(strand, out, verbatim);
                Ok(())
            });
        #[cfg(unix)]
        let builder = builder.method("chown", async move |this, strand, args, _out| {
            let global = this.annex().global;
            let path = this.annex().as_path().into_owned();
            let (path, user, group, follow) =
                super::parse_chown_common(strand, global, args, Some(path))?;
            super::chown(strand, global, &path, user, group, follow).await
        });
        builder
            .method("components", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let components = this
                    .annex()
                    .as_path()
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect();
                this.annex().global.types.path_components_iter.create(
                    strand,
                    PathComponentsIter { components },
                    out,
                );
                Ok(())
            })
            .method("glob", async move |this, strand, args, out| {
                let ([pattern], [max_depth, follow]) =
                    unpack!(strand, args, 1, 0, max_depth = None, follow = None)?;
                let annex = this.annex();
                super::glob(
                    strand,
                    annex.global,
                    Some(&annex.as_path()),
                    pattern,
                    max_depth,
                    follow,
                    out,
                )
                .await
            })
            .method("normalize", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                let normalized = normalize_path(&annex.as_path());
                create_path(strand, annex.global, normalized, out)?;
                Ok(())
            })
            .method("absolute", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                path_absolute(strand, annex.global, &annex.as_path(), out)
            })
            .method("relative", async move |this, strand, args, out| {
                let ([], [base]) = unpack!(strand, args, 0, 1)?;
                let annex = this.annex();
                path_relative(strand, annex.global, &annex.as_path(), base, out)
            })
            .method("add_ext", async move |this, strand, args, out| {
                let ([ext], []) = unpack!(strand, args, 1, 0)?;
                let ext = expect_str(strand, &ext)?;
                let annex = this.annex();
                let annex = annex.with_path(strand, annex.inner.with_added_extension(ext))?;
                this.annex()
                    .global
                    .types
                    .path
                    .create_with_annex(strand, Path, annex, out);
                Ok(())
            })
            .method("without_ext", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                rewrite_path(strand, &annex, &annex.inner, out, |path| {
                    let _ = path.set_extension("");
                })?;
                Ok(())
            })
            .method("with_ext", async move |this, strand, args, out| {
                let ([ext], []) = unpack!(strand, args, 1, 0)?;
                let ext = expect_str(strand, &ext)?;
                let annex = this.annex();
                rewrite_path(strand, &annex, &annex.inner, out, |path| {
                    let _ = path.set_extension(ext);
                })?;
                Ok(())
            })
            .method("with_name", async move |this, strand, args, out| {
                let ([name], []) = unpack!(strand, args, 1, 0)?;
                let name = expect_str(strand, &name)?;
                let annex = this.annex();
                rewrite_path(strand, &annex, &annex.inner, out, |path| {
                    path.set_file_name(name)
                })?;
                Ok(())
            })
            .method("with_stem", async move |this, strand, args, out| {
                let ([stem], []) = unpack!(strand, args, 1, 0)?;
                let stem = expect_str(strand, &stem)?;
                let path = with_stem_path(&this.annex().inner, &stem);
                let annex = this.annex().with_path(strand, path)?;
                this.annex()
                    .global
                    .types
                    .path
                    .create_with_annex(strand, Path, annex, out);
                Ok(())
            })
            .type_method("join", async move |this, strand, args, out| {
                let global = strand.state::<Global<'v>>();
                let mut buf = PathBuf::new();
                for arg in args {
                    match arg {
                        Arg::Pos(slot) => buf.push(&path_from_value(strand, global, &slot)?),
                        Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                    }
                }
                let annex = PathAnnex::try_new(strand, buf, global)?;
                this.create_with_annex(strand, Path, annex, out);
                Ok(())
            })
    }

    fn eq<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.annex();
        let global = borrow.global;
        if let Some(other) = global.types.path.downcast(other) {
            Ok(borrow.as_path() == other.annex().as_path())
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().as_path().hash(hasher);
        Ok(())
    }

    fn lt<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
    ) -> Result<'v, 's, bool> {
        let borrow = this.annex();
        let global = borrow.global;
        if let Some(other) = global.types.path.downcast(other) {
            Ok(borrow.as_path() < other.annex().as_path())
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn div<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.annex();
        let global = borrow.global;
        if let Ok(other) = path_from_value(strand, global, other) {
            let path = borrow.inner.join(&other);
            let annex = PathAnnex::try_new(strand, path, global)?;
            global
                .types
                .path
                .create_with_annex(strand, Path, annex, out);
            Ok(())
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn rdiv<'a, 's>(
        this: Instance<'v, 'a, Self>,
        strand: &'a mut Strand<'v, 's>,
        other: &Value<'v>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let borrow = this.annex();
        let global = borrow.global;
        if let Ok(other) = path_from_value(strand, global, other) {
            let path = other.join(borrow.as_path());
            let annex = PathAnnex::try_new(strand, path, global)?;
            global
                .types
                .path
                .create_with_annex(strand, Path, annex, out);
            Ok(())
        } else {
            Err(Error::not_supported(strand))
        }
    }
}
