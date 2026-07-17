use std::{
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use crate::{
    error::ResultExt as _,
    fs::{path_absolute, path_relative},
    global::Global,
};
use dolang::runtime::object::fmt;

use dolang::runtime::{
    Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    object::{ArrayLike, ArrayView, TypeBuilder},
    unpack,
};
use dolang_shell_vfs::Utf8WindowsPrefix;
use dolang_shell_vfs::{Attrs, Utf8TypedPath, Utf8TypedPathBuf, Vfs};

use super::file::File;

pub(crate) struct Path;
pub(crate) struct UnixPath;
pub(crate) struct WindowsPath;

pub(crate) struct PathAnnex<'v> {
    pub(crate) inner: Utf8TypedPathBuf,
    dispatch: Utf8TypedPathBuf,
    pub(crate) global: State<'v, Global<'v>>,
    stream_name: Option<String>,
    stream_type: Option<String>,
}

fn target_path_type<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, Global<'v>>,
) -> typed_path::PathType {
    global
        .local
        .get(strand)
        .target()
        .operating_system
        .path_type()
}

pub(crate) fn path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    let path = if let Some(path) = global.types.unix_path.downcast(value) {
        Ok(path.annex().inner.clone())
    } else if let Some(path) = global.types.windows_path.downcast(value) {
        Ok(path.annex().typed_path_buf())
    } else if let Some(str) = value.as_str(strand) {
        let target = target_path_type(strand, global);
        Ok(strand.access(|x| match target {
            typed_path::PathType::Unix => Utf8TypedPathBuf::from_unix(str.as_str(x)),
            typed_path::PathType::Windows => Utf8TypedPathBuf::from_windows(str.as_str(x)),
        }))
    } else {
        Err(Error::type_error(strand, "expected Path or str"))
    }?;
    let target = target_path_type(strand, global);
    convert_path_type(strand, path, &target)
}

fn path_path_type(path: Utf8TypedPath<'_>) -> typed_path::PathType {
    match path {
        Utf8TypedPath::Unix(_) => typed_path::PathType::Unix,
        Utf8TypedPath::Windows(_) => typed_path::PathType::Windows,
    }
}

fn same_path_type(a: &typed_path::PathType, b: &typed_path::PathType) -> bool {
    matches!(
        (a, b),
        (typed_path::PathType::Unix, typed_path::PathType::Unix)
            | (typed_path::PathType::Windows, typed_path::PathType::Windows)
    )
}

fn any_path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    if let Some(path) = global.types.unix_path.downcast(value) {
        Ok(path.annex().inner.clone())
    } else if let Some(path) = global.types.windows_path.downcast(value) {
        Ok(path.annex().typed_path_buf())
    } else if let Some(value) = value.as_str(strand) {
        let target = target_path_type(strand, global);
        Ok(strand.access(|x| match target {
            typed_path::PathType::Unix => Utf8TypedPathBuf::from_unix(value.as_str(x)),
            typed_path::PathType::Windows => Utf8TypedPathBuf::from_windows(value.as_str(x)),
        }))
    } else {
        Err(Error::type_error(strand, "expected Path or str"))
    }
}

fn is_path_value<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> bool {
    global.types.unix_path.downcast(value).is_some()
        || global.types.windows_path.downcast(value).is_some()
        || value.as_str(strand).is_some()
}

pub(crate) fn convert_path_type<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: Utf8TypedPathBuf,
    target: &typed_path::PathType,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    if same_path_type(&path_path_type(path.to_path()), target) {
        return Ok(path);
    }
    let invalid_windows_source = match path.to_path() {
        Utf8TypedPath::Windows(path) => {
            path.has_root()
                || path.components().prefix_kind().is_some()
                || path.file_name().is_some_and(|name| name.contains(':'))
        }
        Utf8TypedPath::Unix(path) => path.has_root(),
    };
    if invalid_windows_source || path.is_absolute() {
        return Err(Error::type_error(
            strand,
            "only relative, unrooted paths can be converted between path types",
        ));
    }
    let converted = match target {
        typed_path::PathType::Unix => path.with_unix_encoding_checked(),
        typed_path::PathType::Windows => path.with_windows_encoding_checked(),
    };
    converted.map_err(|_| Error::type_error(strand, "path cannot be converted between path types"))
}

pub(crate) fn safe_concat<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: Utf8TypedPath<'_>,
    right: Utf8TypedPath<'_>,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    let target = path_path_type(left);
    let right = convert_path_type(strand, right.to_path_buf(), &target)?;
    Ok(left.join(right.as_str()))
}

fn concrete_path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
    style: typed_path::PathType,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    if let Some(value) = value.as_str(strand) {
        return Ok(strand.access(|x| match style {
            typed_path::PathType::Unix => Utf8TypedPathBuf::from_unix(value.as_str(x)),
            typed_path::PathType::Windows => Utf8TypedPathBuf::from_windows(value.as_str(x)),
        }));
    }
    let path = any_path_from_value(strand, global, value)?;
    convert_path_type(strand, path, &style)
}

pub(crate) fn create_path_annex<'v, 's>(
    strand: &mut Strand<'v, 's>,
    annex: PathAnnex<'v>,
    out: impl Output<'v>,
) {
    let global = annex.global;
    match annex.inner.to_path() {
        Utf8TypedPath::Unix(_) => global
            .types
            .unix_path
            .create_with_annex(strand, UnixPath, annex, out),
        Utf8TypedPath::Windows(_) => {
            global
                .types
                .windows_path
                .create_with_annex(strand, WindowsPath, annex, out)
        }
    }
}

pub(crate) fn create_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let annex = PathAnnex::try_new(strand, path, global)?;
    create_path_annex(strand, annex, out);
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
    path: Utf8TypedPath<'_>,
    out: Slot<'v, 'a>,
    rewrite: impl FnOnce(&mut Utf8TypedPathBuf),
) -> Result<'v, 's, ()> {
    let mut path = path.to_path_buf();
    rewrite(&mut path);
    let next = annex.with_path(strand, path)?;
    create_path_annex(strand, next, out);
    Ok(())
}

fn with_stem_path(path: Utf8TypedPath<'_>, stem: &str) -> Utf8TypedPathBuf {
    let mut path = path.to_path_buf();
    let ext = path.extension().map(str::to_owned);
    match ext {
        Some(ext) => path.set_file_name(format!("{stem}.{ext}")),
        None => path.set_file_name(stem),
    }
    path
}

impl<'v> PathAnnex<'v> {
    pub(crate) fn try_new<'s>(
        strand: &mut Strand<'v, 's>,
        path: Utf8TypedPathBuf,
        global: State<'v, Global<'v>>,
    ) -> Result<'v, 's, Self> {
        let dispatch = path.clone();
        let (path, stream_name, stream_type) = split_windows_ads(strand, path)?;
        Ok(Self {
            inner: path,
            dispatch,
            global,
            stream_name,
            stream_type,
        })
    }

    pub(crate) fn new(path: Utf8TypedPathBuf, global: State<'v, Global<'v>>) -> Self {
        Self {
            inner: path.clone(),
            dispatch: path,
            global,
            stream_name: None,
            stream_type: None,
        }
    }

    pub(crate) fn as_path(&self) -> Utf8TypedPath<'_> {
        self.dispatch.to_path()
    }

    pub(crate) fn typed_path_buf(&self) -> Utf8TypedPathBuf {
        self.dispatch.clone()
    }

    fn rebuild_dispatch(&mut self) {
        let (Some(stream_name), Some(name)) = (&self.stream_name, self.inner.file_name()) else {
            self.dispatch = self.inner.clone();
            return;
        };
        let mut name = name.to_owned();
        name.push(':');
        name.push_str(stream_name);
        if let Some(stream_type) = &self.stream_type {
            name.push_str(":$");
            name.push_str(stream_type);
        }
        self.dispatch = self.inner.with_file_name(name);
    }

    fn with_path<'s>(
        &self,
        strand: &mut Strand<'v, 's>,
        path: Utf8TypedPathBuf,
    ) -> Result<'v, 's, Self> {
        let annex = Self::try_new(strand, path, self.global)?;
        let mut annex = annex;
        annex.stream_name = self.stream_name.clone();
        annex.stream_type = self.stream_type.clone();
        annex.rebuild_dispatch();
        Ok(annex)
    }

    fn display(&self) -> String {
        self.dispatch.as_str().to_owned()
    }

    fn with_windows_prefix<R>(&self, f: impl FnOnce(Option<Utf8WindowsPrefix<'_>>) -> R) -> R {
        match self.inner.to_path() {
            Utf8TypedPath::Windows(path) => {
                let components = path.components();
                f(components.prefix_kind())
            }
            Utf8TypedPath::Unix(_) => f(None),
        }
    }
}

fn split_windows_ads<'v, 's>(
    strand: &mut Strand<'v, 's>,
    mut path: Utf8TypedPathBuf,
) -> Result<'v, 's, (Utf8TypedPathBuf, Option<String>, Option<String>)> {
    if path.is_unix() {
        return Ok((path, None, None));
    }
    let Some(file_name) = path.file_name() else {
        return Ok((path, None, None));
    };
    let file_name = file_name.to_owned();
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

fn with_added_extension(path: Utf8TypedPath<'_>, ext: &str) -> Utf8TypedPathBuf {
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let mut name = name.to_owned();
    if !ext.is_empty() {
        name.push('.');
        name.push_str(ext);
    }
    path.with_file_name(name)
}

trait ConcretePath<'v>: Object<'v, Annex = PathAnnex<'v>> {}
impl<'v> ConcretePath<'v> for UnixPath {}
impl<'v> ConcretePath<'v> for WindowsPath {}

struct Components<T>(PhantomData<T>);

impl<'v, T: ConcretePath<'v>> ArrayLike<'v> for Components<T> {
    type Object = T;
    const MODULE: &'v str = "fs";
    const NAME: &'v str = "PathComponents";

    fn len(this: Instance<'v, '_, T>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().as_path().components().count()
    }

    fn get<'a, 's>(
        this: Instance<'v, '_, T>,
        strand: &'a mut Strand<'v, 's>,
        index: usize,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let component = this
            .annex()
            .as_path()
            .components()
            .nth(index)
            .expect("array view index was normalized")
            .as_str()
            .to_owned();
        Output::set(strand, out, component.as_str());
        Ok(())
    }
}

impl<'v> Object<'v> for Path {
    const NAME: &'v str = "Path";
    const MODULE: &'v str = "fs";
    type Annex = ();
    type Type = ();
    type TypeAnnex = ();

    async fn new<'a, 's>(
        _this: Type<'v, Self>,
        strand: &'a mut Strand<'v, 's>,
        args: Args<'v, 'a>,
        out: Slot<'v, 'a>,
    ) -> Result<'v, 's, ()> {
        let global = strand.state::<Global<'v>>();
        let ([path], []) = unpack!(strand, args, 1, 0)?;
        let path = any_path_from_value(strand, global, &path)?;
        let target = target_path_type(strand, global);
        let path = convert_path_type(strand, path, &target)?;
        create_path(strand, global, path, out)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.type_method("join", async move |_this, strand, args, out| {
            let global = strand.state::<Global<'v>>();
            let mut target = None;
            let mut buf = None;
            for arg in args {
                match arg {
                    Arg::Pos(slot) => {
                        let path = any_path_from_value(strand, global, &slot)?;
                        let target = target.get_or_insert_with(|| path_path_type(path.to_path()));
                        let path = convert_path_type(strand, path, target)?;
                        let buf = buf.get_or_insert_with(|| match target {
                            typed_path::PathType::Unix => Utf8TypedPathBuf::from_unix(""),
                            typed_path::PathType::Windows => Utf8TypedPathBuf::from_windows(""),
                        });
                        buf.push(path.as_str());
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            let buf = buf.unwrap_or_else(|| match target_path_type(strand, global) {
                typed_path::PathType::Unix => Utf8TypedPathBuf::from_unix(""),
                typed_path::PathType::Windows => Utf8TypedPathBuf::from_windows(""),
            });
            create_path(strand, global, buf, out)
        })
    }
}

macro_rules! impl_concrete_path {
    ($path:ident, $name:literal, $style:expr) => {
        impl<'v> Object<'v> for $path {
            const NAME: &'v str = $name;
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
                let path = concrete_path_from_value(strand, global, &path, $style)?;
                let annex = PathAnnex::try_new(strand, path, global)?;
                this.create_with_annex(strand, $path, annex, out);
                Ok(())
            }

            fn debug<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn dolang::runtime::Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(strand, w, "<Path {:?}>", this.annex().display())
            }

            fn display<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn dolang::runtime::Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(strand, w, "{}", this.annex().display())
            }

            fn display_arg<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn dolang::runtime::Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(strand, w, "{}", this.annex().display())
            }

            fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
                let all = builder.sym("all");
                let ignore = builder.sym("ignore");
                let max_depth = builder.sym("max_depth");
                let follow = builder.sym("follow");
                let owner = builder.sym("owner");
                let group = builder.sym("group");
                let dacl = builder.sym("dacl");
                let sacl = builder.sym("sacl");
                let namespace = builder.sym("namespace");
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
                            Output::set(strand, out, n);
                        }
                        Ok(())
                    })
                    .get("stem", |this, strand, out| {
                        let borrow = this.annex();
                        if let Some(stem) = borrow.inner.file_stem() {
                            Output::set(strand, out, stem);
                        }
                        Ok(())
                    })
                    .get("parent", |this, strand, out| {
                        let borrow = this.annex();
                        if let Some(path) = borrow.inner.parent() {
                            create_path(strand, borrow.global, path.to_path_buf(), out)?;
                        }
                        Ok(())
                    })
                    .get("ext", |this, strand, out| {
                        let borrow = this.annex();
                        if let Some(e) = borrow.inner.extension() {
                            Output::set(strand, out, e);
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
                        File::open(strand, annex.global, annex.as_path(), opt1, opt2, out).await
                    })
                    .method("metadata", async move |this, strand, args, out| {
                        let ([], [follow]) = unpack!(strand, args, 0, 0, follow = None)?;
                        let follow = match follow {
                            Some(v) => v
                                .as_bool(strand)
                                .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                            None => true,
                        };
                        let annex = this.annex();
                        super::metadata(strand, annex.global, annex.as_path(), follow, out).await
                    })
                    .method("fs_metadata", async move |this, strand, args, out| {
                        let ([], [follow]) = unpack!(strand, args, 0, 0, follow = None)?;
                        let follow = match follow {
                            Some(v) => v
                                .as_bool(strand)
                                .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                            None => true,
                        };
                        let annex = this.annex();
                        super::fs_metadata(strand, annex.global, annex.as_path(), follow, out).await
                    })
                    .method("sec_desc", async move |this, strand, args, out| {
                        let ([], [owner, group, dacl, sacl, follow]) = unpack!(
                            strand,
                            args,
                            0,
                            0,
                            owner = None,
                            group = None,
                            dacl = None,
                            sacl = None,
                            follow = None
                        )?;
                        let mask = super::sec_desc_mask(strand, owner, group, dacl, sacl)?;
                        let follow = match follow {
                            Some(value) => value.as_bool(strand).ok_or_else(|| {
                                Error::type_error(strand, "follow: expected bool")
                            })?,
                            None => true,
                        };
                        let annex = this.annex();
                        super::sec_desc(strand, annex.global, annex.as_path(), mask, follow, out)
                            .await
                    })
                    .method("set_sec_desc", async move |this, strand, args, _out| {
                        let ([descriptor], [follow]) = unpack!(strand, args, 1, 0, follow = None)?;
                        let annex = this.annex();
                        let descriptor = crate::security::sec_desc_from_value(
                            strand,
                            annex.global,
                            &descriptor,
                        )?;
                        let follow = match follow {
                            Some(value) => value.as_bool(strand).ok_or_else(|| {
                                Error::type_error(strand, "follow: expected bool")
                            })?,
                            None => true,
                        };
                        super::set_sec_desc(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &descriptor,
                            follow,
                        )
                        .await
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
                        super::get_attrs(strand, annex.global, annex.as_path(), follow, out).await
                    })
                    .method("xattrs", async move |this, strand, args, out| {
                        let ([], [namespace, follow]) =
                            unpack!(strand, args, 0, 0, namespace = None, follow = None)?;
                        let annex = this.annex();
                        super::xattr::path_list(
                            strand,
                            annex.global,
                            annex.as_path(),
                            namespace,
                            follow,
                            out,
                        )
                        .await
                    })
                    .method("streams", async move |this, strand, args, out| {
                        let ([], [follow]) = unpack!(strand, args, 0, 0, follow = None)?;
                        let annex = this.annex();
                        super::stream::path_list(strand, annex.global, annex.as_path(), follow, out)
                            .await
                    })
                    .method("xattr", async move |this, strand, args, out| {
                        let ([name], [namespace, follow]) =
                            unpack!(strand, args, 1, 0, namespace = None, follow = None)?;
                        let annex = this.annex();
                        super::xattr::path_get(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &name,
                            namespace,
                            follow,
                            out,
                        )
                        .await
                    })
                    .method("set_xattr", async move |this, strand, args, _out| {
                        let ([name, value], [namespace, follow]) =
                            unpack!(strand, args, 2, 0, namespace = None, follow = None)?;
                        let annex = this.annex();
                        super::xattr::path_set(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &name,
                            namespace,
                            &value,
                            follow,
                        )
                        .await
                    })
                    .method("remove_xattr", async move |this, strand, args, _out| {
                        let ([name], [namespace, follow]) =
                            unpack!(strand, args, 1, 0, namespace = None, follow = None)?;
                        let annex = this.annex();
                        super::xattr::path_remove(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &name,
                            namespace,
                            follow,
                        )
                        .await
                    })
                    .method("exists", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        super::exists(strand, annex.global, annex.as_path(), out).await
                    })
                    .method("read", async move |this, strand, args, out| {
                        let ([], [mode]) = unpack!(strand, args, 0, 1)?;
                        let annex = this.annex();
                        super::read(strand, annex.global, annex.as_path(), mode, out).await
                    })
                    .method("write", async move |this, strand, args, out| {
                        let ([data], []) = unpack!(strand, args, 1, 0)?;
                        let annex = this.annex();
                        super::write(strand, annex.global, annex.as_path(), data, out).await
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
                        super::set_len(strand, annex.global, annex.as_path(), size).await
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
                        super::copy(strand, annex.global, annex.as_path(), to.to_path(), all).await
                    })
                    .method("rename", async move |this, strand, args, _out| {
                        let ([to], []) = unpack!(strand, args, 1, 0)?;
                        let to = path_from_value(strand, this.annex().global, &to)?;
                        let annex = this.annex();
                        super::rename(strand, annex.global, annex.as_path(), to.to_path()).await
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
                        super::move_(strand, annex.global, annex.as_path(), to.to_path(), all).await
                    })
                    .method("hard_link", async move |this, strand, args, _out| {
                        let ([to], []) = unpack!(strand, args, 1, 0)?;
                        let to = path_from_value(strand, this.annex().global, &to)?;
                        let annex = this.annex();
                        super::hard_link(strand, annex.global, annex.as_path(), to.to_path()).await
                    })
                    .method("entries", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        super::entries(strand, annex.global, annex.as_path().to_path_buf(), out)
                            .await
                    })
                    .method("canonical", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        super::path_canonical(strand, annex.global, annex.as_path(), out).await
                    })
                    .method("read_link", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        let global = annex.global;
                        let path = super::prepend_cwd(strand, global, annex.as_path())?;
                        let local = global.local.get(strand);
                        let vfs = local.vfs();
                        let target = vfs.read_link(path.to_path()).await.into_sys(strand)?;
                        let annex = PathAnnex::try_new(strand, target, global)?;
                        create_path_annex(strand, annex, out);
                        Ok(())
                    })
                    .method("remove", async move |this, strand, args, _out| {
                        let ([], [all, ignore]) =
                            unpack!(strand, args, 0, 0, all = None, ignore = None)?;
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
                        super::remove(strand, annex.global, annex.as_path(), all, ignore).await
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
                        super::create_dir(strand, annex.global, annex.as_path(), all).await
                    })
                    .method("remove_dir", async move |this, strand, args, _out| {
                        let ([], [all, ignore]) =
                            unpack!(strand, args, 0, 0, all = None, ignore = None)?;
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
                        super::remove_dir(strand, annex.global, annex.as_path(), all, ignore).await
                    })
                    .method("chmod", async move |this, strand, args, _out| {
                        let ([mode], []) = unpack!(strand, args, 1, 0)?;
                        let mode = mode
                            .to_i64(strand)
                            .map_err(|_| Error::type_error(strand, "expected int"))?
                            as u32;
                        let annex = this.annex();
                        super::chmod(strand, annex.global, annex.as_path(), mode).await
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
                            not_content_indexed: super::parse_attr_bool(
                                strand,
                                not_content_indexed,
                            )?,
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
                        super::set_attrs(strand, annex.global, annex.as_path(), attrs).await
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
                            annex.as_path(),
                            modified,
                            accessed,
                            created,
                        )
                        .await
                    });
                let builder = if matches!($style, typed_path::PathType::Windows) {
                    builder
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
                            let disk = annex.with_windows_prefix(|prefix| match prefix {
                                Some(Utf8WindowsPrefix::Disk(disk))
                                | Some(Utf8WindowsPrefix::VerbatimDisk(disk)) => Some(disk),
                                _ => None,
                            });
                            if let Some(disk) = disk {
                                let disk = disk.to_string();
                                Output::set(strand, out, disk.as_str());
                            }
                            Ok(())
                        })
                        .get("server", |this, strand, out| {
                            let annex = this.annex();
                            let server = annex.with_windows_prefix(|prefix| match prefix {
                                Some(Utf8WindowsPrefix::UNC(server, _))
                                | Some(Utf8WindowsPrefix::VerbatimUNC(server, _)) => {
                                    Some(server.to_owned())
                                }
                                _ => None,
                            });
                            if let Some(server) = server {
                                Output::set(strand, out, server.as_str());
                            }
                            Ok(())
                        })
                        .get("share", |this, strand, out| {
                            let annex = this.annex();
                            let share = annex.with_windows_prefix(|prefix| match prefix {
                                Some(Utf8WindowsPrefix::UNC(_, share))
                                | Some(Utf8WindowsPrefix::VerbatimUNC(_, share)) => {
                                    Some(share.to_owned())
                                }
                                _ => None,
                            });
                            if let Some(share) = share {
                                Output::set(strand, out, share.as_str());
                            }
                            Ok(())
                        })
                        .get("device", |this, strand, out| {
                            let annex = this.annex();
                            let device = annex.with_windows_prefix(|prefix| match prefix {
                                Some(Utf8WindowsPrefix::DeviceNS(device)) => {
                                    Some(device.to_owned())
                                }
                                _ => None,
                            });
                            if let Some(device) = device {
                                Output::set(strand, out, device.as_str());
                            }
                            Ok(())
                        })
                        .get("verbatim", |this, strand, out| {
                            let annex = this.annex();
                            let verbatim = annex.with_windows_prefix(|prefix| {
                                prefix.is_some_and(|prefix| prefix.is_verbatim())
                            });
                            Output::set(strand, out, verbatim);
                            Ok(())
                        })
                } else {
                    builder
                };
                let builder = builder.method("chown", async move |this, strand, args, _out| {
                    let global = this.annex().global;
                    let path = this.annex().as_path().to_path_buf();
                    let (path, user, group, follow) =
                        super::parse_chown_common(strand, global, args, Some(path))?;
                    super::chown(strand, global, path.to_path(), user, group, follow).await
                });
                builder
                    .get("components", |this, strand, out| {
                        Output::set(strand, out, ArrayView::<Components<$path>>::new(this));
                        Ok(())
                    })
                    .method("glob", async move |this, strand, args, out| {
                        let ([pattern], [max_depth, follow]) =
                            unpack!(strand, args, 1, 0, max_depth = None, follow = None)?;
                        let annex = this.annex();
                        super::glob(
                            strand,
                            annex.global,
                            Some(annex.as_path()),
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
                        let normalized = annex.as_path().normalize();
                        create_path(strand, annex.global, normalized, out)?;
                        Ok(())
                    })
                    .method("absolute", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        path_absolute(strand, annex.global, annex.as_path(), out)
                    })
                    .method("relative", async move |this, strand, args, out| {
                        let ([], [base]) = unpack!(strand, args, 0, 1)?;
                        let annex = this.annex();
                        path_relative(strand, annex.global, annex.as_path(), base, out)
                    })
                    .method("add_ext", async move |this, strand, args, out| {
                        let ([ext], []) = unpack!(strand, args, 1, 0)?;
                        let ext = expect_str(strand, &ext)?;
                        let annex = this.annex();
                        let annex =
                            annex.with_path(strand, with_added_extension(annex.as_path(), &ext))?;
                        create_path_annex(strand, annex, out);
                        Ok(())
                    })
                    .method("without_ext", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, annex.inner.to_path(), out, |path| {
                            let _ = path.set_extension("");
                        })?;
                        Ok(())
                    })
                    .method("with_ext", async move |this, strand, args, out| {
                        let ([ext], []) = unpack!(strand, args, 1, 0)?;
                        let ext = expect_str(strand, &ext)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, annex.inner.to_path(), out, |path| {
                            let _ = path.set_extension(ext);
                        })?;
                        Ok(())
                    })
                    .method("with_name", async move |this, strand, args, out| {
                        let ([name], []) = unpack!(strand, args, 1, 0)?;
                        let name = expect_str(strand, &name)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, annex.inner.to_path(), out, |path| {
                            path.set_file_name(name)
                        })?;
                        Ok(())
                    })
                    .method("with_stem", async move |this, strand, args, out| {
                        let ([stem], []) = unpack!(strand, args, 1, 0)?;
                        let stem = expect_str(strand, &stem)?;
                        let path = with_stem_path(this.annex().inner.to_path(), &stem);
                        let annex = this.annex().with_path(strand, path)?;
                        create_path_annex(strand, annex, out);
                        Ok(())
                    })
                    .type_method("join", async move |this, strand, args, out| {
                        let global = strand.state::<Global<'v>>();
                        let mut buf = match $style {
                            typed_path::PathType::Unix => Utf8TypedPathBuf::from_unix(""),
                            typed_path::PathType::Windows => Utf8TypedPathBuf::from_windows(""),
                        };
                        for arg in args {
                            match arg {
                                Arg::Pos(slot) => {
                                    let path =
                                        concrete_path_from_value(strand, global, &slot, $style)?;
                                    buf.push(path.as_str());
                                }
                                Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                            }
                        }
                        let annex = PathAnnex::try_new(strand, buf, global)?;
                        this.create_with_annex(strand, $path, annex, out);
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
                if let Ok(other) = any_path_from_value(strand, global, other) {
                    Ok(borrow.typed_path_buf() == other)
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
                if let Ok(other) = any_path_from_value(strand, global, other) {
                    Ok(borrow.typed_path_buf() < other)
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
                if is_path_value(strand, global, other) {
                    let other = any_path_from_value(strand, global, other)?;
                    let other = convert_path_type(strand, other, &$style)?;
                    let path = borrow.inner.join(other.as_str());
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
                    Ok(())
                } else if let Ok(path) =
                    super::readdir::path_with_entry(strand, global, borrow.as_path(), other)
                {
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
                    Ok(())
                } else if let Ok(path) =
                    super::stream::path_with_stream(strand, global, borrow.as_path(), other)
                {
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
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
                if is_path_value(strand, global, other) {
                    let other = any_path_from_value(strand, global, other)?;
                    let other = convert_path_type(strand, other, &$style)?;
                    let path = other.join(borrow.as_path().as_str());
                    let annex = PathAnnex::try_new(strand, path, global)?;
                    create_path_annex(strand, annex, out);
                    Ok(())
                } else {
                    Err(Error::not_supported(strand))
                }
            }
        }
    };
}

impl_concrete_path!(UnixPath, "UnixPath", typed_path::PathType::Unix);
impl_concrete_path!(WindowsPath, "WindowsPath", typed_path::PathType::Windows);
