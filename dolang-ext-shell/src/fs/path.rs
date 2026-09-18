use std::{
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use dolang::runtime::value::fmt::Format;

use crate::{
    error::ResultExt as _,
    fs::{path_absolute, path_relative},
    global::{FsGlobal, Global, WindowsSecurityGlobal},
};
use dolang::runtime::object::fmt;

use dolang::runtime::{
    AllocExt, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Type, Value,
    object::{ArrayLike, ArrayView, TypeBuilder},
    unpack,
};
use dolang_vfs::metadata::AttrFlags;
use dolang_vfs::path as vfs_path;

use super::file::File;

pub(crate) struct Path;
pub(crate) struct UnixPath;
pub(crate) struct WindowsPath;

pub(crate) struct PathAnnex<'v> {
    pub(crate) path: vfs_path::PathBuf,
    pub(crate) global: State<'v, FsGlobal<'v>>,
}

fn target_path_type<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
) -> vfs_path::Kind {
    global.local.get(strand).target().os().path_kind()
}

/// Extracts the path held by an `fs.unix.Path` or `fs.windows.Path`.
///
/// No path object exists before the `fs` setup has run, so this does not
/// force it.
pub(crate) fn cast_path<'v>(
    strand: &mut Strand<'v, '_>,
    value: &Value<'v>,
) -> Option<vfs_path::PathBuf> {
    let global = strand.try_state::<FsGlobal<'v>>()?;
    path_object_from_value(strand, global, value)
}

pub(crate) fn path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
) -> Result<'v, 's, vfs_path::PathBuf> {
    if let Some(path) = cast_path(strand, value) {
        Ok(path)
    } else if let Some(str) = value.as_str(strand) {
        let local = strand.state::<Global<'v>>().local;
        let target = local.get(strand).target().os().path_kind();
        Ok(strand.access(|x| match target {
            vfs_path::Kind::Unix => vfs_path::PathBuf::from_unix(str.as_str(x)),
            vfs_path::Kind::Windows => vfs_path::PathBuf::from_windows(str.as_str(x)),
        }))
    } else {
        Err(Error::type_error(strand, "expected Path or Str"))
    }
}

fn any_path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, vfs_path::PathBuf> {
    if let Some(path) = global.types.unix_path.cast(value) {
        Ok(path.enter_sync(strand, |_strand, inst| inst.annex().path_buf()))
    } else if let Some(path) = global.types.windows_path.cast(value) {
        Ok(path.enter_sync(strand, |_strand, inst| inst.annex().path_buf()))
    } else if let Some(value) = value.as_str(strand) {
        let target = target_path_type(strand, global);
        Ok(strand.access(|x| match target {
            vfs_path::Kind::Unix => vfs_path::PathBuf::from_unix(value.as_str(x)),
            vfs_path::Kind::Windows => vfs_path::PathBuf::from_windows(value.as_str(x)),
        }))
    } else {
        Err(Error::type_error(strand, "expected Path or Str"))
    }
}

fn path_object_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    value: &Value<'v>,
) -> Option<vfs_path::PathBuf> {
    if let Some(path) = global.types.unix_path.cast(value) {
        Some(path.enter_sync(strand, |_strand, inst| inst.annex().path_buf()))
    } else {
        global
            .types
            .windows_path
            .cast(value)
            .map(|path| path.enter_sync(strand, |_strand, inst| inst.annex().path_buf()))
    }
}

fn is_path_value<'v>(
    strand: &Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
    value: &Value<'v>,
) -> bool {
    global.types.unix_path.cast(value).is_some()
        || global.types.windows_path.cast(value).is_some()
        || value.as_str(strand).is_some()
}

/// Converts `path` into `target` syntax, reporting failure as a Do type error.
///
/// The rules live in [`vfs_path::Path::to_kind`]; this only restates the
/// failure as the `TypeError` the Do-side `Path` API promises.
pub(crate) fn convert_path_kind<'v, 's>(
    strand: &mut Strand<'v, 's>,
    path: vfs_path::PathBuf,
    target: vfs_path::Kind,
) -> Result<'v, 's, vfs_path::PathBuf> {
    match path.to_kind(target) {
        Ok(path) => Ok(path),
        Err(error) => Err(Error::type_error(strand, error.to_string())),
    }
}

pub(crate) fn safe_concat<'v, 's>(
    strand: &mut Strand<'v, 's>,
    left: vfs_path::Path<'_>,
    right: vfs_path::Path<'_>,
) -> Result<'v, 's, vfs_path::PathBuf> {
    let target = left.kind();
    let right = convert_path_kind(strand, right.to_path_buf(), target)?;
    Ok(left.join(right.as_str()))
}

fn concrete_path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    value: &Value<'v>,
    style: vfs_path::Kind,
) -> Result<'v, 's, vfs_path::PathBuf> {
    if let Some(value) = value.as_str(strand) {
        return Ok(strand.access(|x| match style {
            vfs_path::Kind::Unix => vfs_path::PathBuf::from_unix(value.as_str(x)),
            vfs_path::Kind::Windows => vfs_path::PathBuf::from_windows(value.as_str(x)),
        }));
    }
    let path = any_path_from_value(strand, global, value)?;
    convert_path_kind(strand, path, style)
}

pub(crate) fn create_path_annex<'v, 's>(
    strand: &mut Strand<'v, 's>,
    annex: PathAnnex<'v>,
    out: impl Output<'v>,
) {
    let global = annex.global;
    match annex.path.kind() {
        vfs_path::Kind::Unix => global
            .types
            .unix_path
            .create_with_annex(strand, UnixPath, annex, out),
        vfs_path::Kind::Windows => {
            global
                .types
                .windows_path
                .create_with_annex(strand, WindowsPath, annex, out)
        }
    }
}

pub(crate) fn create_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, FsGlobal<'v>>,
    path: vfs_path::PathBuf,
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
        .ok_or_else(|| Error::type_error(strand, "expected Str"))
}

fn rewrite_path<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    annex: &PathAnnex<'v>,
    out: Slot<'v, 'a>,
    rewrite: impl FnOnce(&mut vfs_path::PathBuf),
) -> Result<'v, 's, ()> {
    let mut path = annex.path_buf();
    rewrite(&mut path);
    let next = PathAnnex::try_new(strand, path, annex.global)?;
    create_path_annex(strand, next, out);
    Ok(())
}

impl<'v> PathAnnex<'v> {
    /// Wraps `path`, rejecting a malformed alternate data stream suffix.
    ///
    /// Validating here is what lets every accessor below stay infallible: a Do
    /// `Path` value never holds a suffix the path layer cannot parse.
    pub(crate) fn try_new<'s>(
        strand: &mut Strand<'v, 's>,
        path: vfs_path::PathBuf,
        global: State<'v, FsGlobal<'v>>,
    ) -> Result<'v, 's, Self> {
        stream_spec(strand, path.to_path())?;
        Ok(Self { path, global })
    }

    pub(crate) fn as_path(&self) -> vfs_path::Path<'_> {
        self.path.to_path()
    }

    pub(crate) fn path_buf(&self) -> vfs_path::PathBuf {
        self.path.clone()
    }

    fn display(&self) -> String {
        self.path.as_str().to_owned()
    }

    fn windows_prefix(&self) -> Option<vfs_path::WindowsPrefix<'_>> {
        self.as_path().windows_prefix()
    }
}

/// Reads a path's alternate data stream specifier.
///
/// The grammar lives in [`vfs_path::Path::stream`]; this only restates a
/// malformed suffix as the `ValueError` the Do-side `Path` API promises.
fn stream_spec<'v, 'a, 's>(
    strand: &mut Strand<'v, 's>,
    path: vfs_path::Path<'a>,
) -> Result<'v, 's, Option<vfs_path::StreamSpec<'a>>> {
    match path.stream() {
        Ok(spec) => Ok(spec),
        Err(error) => Err(Error::value(strand, error.to_string())),
    }
}

trait ConcretePath<'v>: Object<'v, Annex = PathAnnex<'v>> {}
impl<'v> ConcretePath<'v> for UnixPath {}
impl<'v> ConcretePath<'v> for WindowsPath {}

struct Components<T>(PhantomData<T>);

impl<'v, T: ConcretePath<'v>> ArrayLike<'v> for Components<T> {
    type Object = T;
    const MODULE: &'v str = "fs";
    const NAME: &'v str = "PathComponents";

    fn len(&self, this: Instance<'v, '_, T>, _strand: &mut Strand<'v, '_>) -> usize {
        this.annex().as_path().components().count()
    }

    fn get<'a, 's>(
        &self,
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
        let global = strand.state::<FsGlobal<'v>>();
        let ([path], []) = unpack!(strand, args, 1, 0)?;
        let path = any_path_from_value(strand, global, &path)?;
        let target = target_path_type(strand, global);
        let path = convert_path_kind(strand, path, target)?;
        create_path(strand, global, path, out)
    }

    fn build<'a>(builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        builder.type_method("join", async move |_this, strand, args, out| {
            let global = strand.state::<FsGlobal<'v>>();
            let ([], [], paths) = unpack!(strand, args, 0, 0, *)?;
            let mut target = None;
            let mut buf = None;
            for slot in paths {
                let path = any_path_from_value(strand, global, &slot)?;
                let target = *target.get_or_insert_with(|| path.kind());
                let path = convert_path_kind(strand, path, target)?;
                let buf = buf.get_or_insert_with(|| vfs_path::PathBuf::empty(target));
                buf.push(path.as_str());
            }
            let buf = buf.unwrap_or_else(|| match target_path_type(strand, global) {
                vfs_path::Kind::Unix => vfs_path::PathBuf::from_unix(""),
                vfs_path::Kind::Windows => vfs_path::PathBuf::from_windows(""),
            });
            create_path(strand, global, buf, out)
        })
    }
}

macro_rules! impl_concrete_path {
    ($path:ident, $module:literal, $style:expr) => {
        impl<'v> Object<'v> for $path {
            const NAME: &'v str = "Path";
            const MODULE: &'v str = $module;
            type Annex = PathAnnex<'v>;
            type Type = ();
            type TypeAnnex = ();

            async fn new<'a, 's>(
                this: Type<'v, Self>,
                strand: &'a mut Strand<'v, 's>,
                args: Args<'v, 'a>,
                out: Slot<'v, 'a>,
            ) -> Result<'v, 's, ()> {
                let global = strand.state::<FsGlobal<'v>>();
                let ([path], []) = unpack!(strand, args, 1, 0)?;
                let path = concrete_path_from_value(strand, global, &path, $style)?;
                let annex = PathAnnex::try_new(strand, path, global)?;
                this.create_with_annex(strand, $path, annex, out);
                Ok(())
            }

            fn debug<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(strand, w, "<{}.Path {:?}>", $module, this.annex().display())
            }

            fn display<'a, 's>(
                this: Instance<'v, 'a, Self>,
                strand: &'a mut Strand<'v, 's>,
                w: &mut dyn Format<'v>,
            ) -> Result<'v, 's, ()> {
                fmt!(strand, w, "{}", this.annex().display())
            }

            fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
                let all = builder.sym("all");
                let ignore = builder.sym("ignore");
                let max_depth = builder.sym("max_depth");
                let resolve = builder.sym("resolve");
                let replace = builder.sym("replace");
                let mode = builder.sym("mode");
                let data_kw = builder.sym("data");
                let owner = builder.sym("owner");
                let group = builder.sym("group");
                let dacl = builder.sym("dacl");
                let sacl = builder.sym("sacl");
                let default_acl = builder.sym("default");
                let kind_acl = builder.sym("kind");
                let namespace = builder.sym("namespace");
                let modified = builder.sym("modified");
                let accessed = builder.sym("accessed");
                let created = builder.sym("created");
                let readonly = builder.sym("readonly");
                let hidden = builder.sym("hidden");
                let system = builder.sym("system");
                let archive = builder.sym("archive");
                let compressed = builder.sym("compressed");
                let sparse = builder.sym("sparse");
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
                        if let Some(n) = borrow.as_path().file_name() {
                            Output::set(strand, out, n);
                        }
                        Ok(())
                    })
                    .get("stem", |this, strand, out| {
                        let borrow = this.annex();
                        if let Some(stem) = borrow.as_path().file_stem() {
                            Output::set(strand, out, stem);
                        }
                        Ok(())
                    })
                    .get("parent", |this, strand, out| {
                        let borrow = this.annex();
                        if let Some(path) = borrow.as_path().parent() {
                            create_path(strand, borrow.global, path.to_path_buf(), out)?;
                        }
                        Ok(())
                    })
                    .get("ext", |this, strand, out| {
                        let borrow = this.annex();
                        if let Some(e) = borrow.as_path().extension() {
                            Output::set(strand, out, e);
                        }
                        Ok(())
                    })
                    .get("is_absolute", |this, strand, out| {
                        let borrow = this.annex();
                        Output::set(strand, out, borrow.as_path().is_absolute());
                        Ok(())
                    })
                    .method("open", async move |this, strand, args, out| {
                        let ([], [opt1, opt2]) = unpack!(strand, args, 0, 2)?;
                        let annex = this.annex();
                        File::open(strand, annex.global, annex.as_path(), opt1, opt2, out).await
                    })
                    .method("metadata", async move |this, strand, args, out| {
                        let ([], [resolve]) = unpack!(strand, args, 0, 0, resolve = None)?;
                        let annex = this.annex();
                        let follow = super::resolve_sym(strand, annex.global, resolve, true)?;
                        super::metadata(strand, annex.global, annex.as_path(), follow, out).await
                    })
                    .method("fs_metadata", async move |this, strand, args, out| {
                        let ([], [resolve]) = unpack!(strand, args, 0, 0, resolve = None)?;
                        let annex = this.annex();
                        let follow = super::resolve_sym(strand, annex.global, resolve, true)?;
                        super::fs_metadata(strand, annex.global, annex.as_path(), follow, out).await
                    })
                    .method("sec_desc", async move |this, strand, args, out| {
                        let ([], [owner, group, dacl, sacl, resolve]) = unpack!(
                            strand,
                            args,
                            0,
                            0,
                            owner = None,
                            group = None,
                            dacl = None,
                            sacl = None,
                            resolve = None
                        )?;
                        let mask = super::sec_desc_mask(strand, owner, group, dacl, sacl)?;
                        let annex = this.annex();
                        let follow = super::resolve_sym(strand, annex.global, resolve, true)?;
                        super::sec_desc(strand, annex.global, annex.as_path(), mask, follow, out)
                            .await
                    })
                    .method("acl", async move |this, strand, args, out| {
                        let ([], [kind, default, resolve]) = unpack!(
                            strand,
                            args,
                            0,
                            0,
                            kind_acl = None,
                            default_acl = None,
                            resolve = None
                        )?;
                        let annex = this.annex();
                        let kind = crate::security::acl_kind_sym(strand, kind)?;
                        let default = super::acl_default(strand, default.as_deref())?;
                        let follow = super::resolve_sym(strand, annex.global, resolve, true)?;
                        super::acl(
                            strand,
                            annex.global,
                            annex.as_path(),
                            kind,
                            default,
                            follow,
                            out,
                        )
                        .await
                    })
                    .method("set_acl", async move |this, strand, args, _out| {
                        let ([acl_value], [kind, default, resolve]) = unpack!(
                            strand,
                            args,
                            1,
                            0,
                            kind_acl = None,
                            default_acl = None,
                            resolve = None
                        )?;
                        let annex = this.annex();
                        let (kind, acl) = crate::security::resolve_acl_input(
                            strand,
                            &acl_value,
                            kind,
                            &crate::security::SpecPath::root("Path.set_acl.acl"),
                        )
                        .await?;
                        let default = super::acl_default(strand, default.as_deref())?;
                        let follow = super::resolve_sym(strand, annex.global, resolve, true)?;
                        super::set_acl(
                            strand,
                            annex.global,
                            annex.as_path(),
                            kind,
                            acl.as_ref(),
                            default,
                            follow,
                        )
                        .await
                    })
                    .method("update_sec_desc", async move |this, strand, args, _out| {
                        let ([], [resolve], rest) =
                            unpack!(strand, args, 0, 0, resolve = None, ...)?;
                        let annex = this.annex();
                        let windows = strand.force_state::<WindowsSecurityGlobal<'v>>();
                        let descriptor = crate::security::sec_desc_from_args(
                            strand,
                            windows,
                            rest,
                            &crate::security::SpecPath::root("update_sec_desc"),
                        )
                        .await?;
                        let follow = super::resolve_sym(strand, annex.global, resolve, true)?;
                        super::update_sec_desc(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &descriptor,
                            follow,
                        )
                        .await
                    })
                    .method("xattrs", async move |this, strand, args, out| {
                        let ([], [namespace, resolve]) =
                            unpack!(strand, args, 0, 0, namespace = None, resolve = None)?;
                        let annex = this.annex();
                        super::xattr::path_list(
                            strand,
                            annex.global,
                            annex.as_path(),
                            namespace,
                            resolve,
                            out,
                        )
                        .await
                    })
                    .method("streams", async move |this, strand, args, out| {
                        let ([], [resolve]) = unpack!(strand, args, 0, 0, resolve = None)?;
                        let annex = this.annex();
                        super::stream::path_list(
                            strand,
                            annex.global,
                            annex.as_path(),
                            resolve,
                            out,
                        )
                        .await
                    })
                    .method("xattr", async move |this, strand, args, out| {
                        let ([name], [namespace, resolve]) =
                            unpack!(strand, args, 1, 0, namespace = None, resolve = None)?;
                        let annex = this.annex();
                        super::xattr::path_get(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &name,
                            namespace,
                            resolve,
                            out,
                        )
                        .await
                    })
                    .method("set_xattr", async move |this, strand, args, _out| {
                        let ([name, value], [namespace, resolve]) =
                            unpack!(strand, args, 2, 0, namespace = None, resolve = None)?;
                        let annex = this.annex();
                        super::xattr::path_set(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &name,
                            namespace,
                            &value,
                            resolve,
                        )
                        .await
                    })
                    .method("remove_xattr", async move |this, strand, args, _out| {
                        let ([name], [namespace, resolve]) =
                            unpack!(strand, args, 1, 0, namespace = None, resolve = None)?;
                        let annex = this.annex();
                        super::xattr::path_remove(
                            strand,
                            annex.global,
                            annex.as_path(),
                            &name,
                            namespace,
                            resolve,
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
                    .method("append", async move |this, strand, args, out| {
                        let ([data], []) = unpack!(strand, args, 1, 0)?;
                        let annex = this.annex();
                        super::append(strand, annex.global, annex.as_path(), data, out).await
                    })
                    .method("set_size", async move |this, strand, args, _out| {
                        let ([size], []) = unpack!(strand, args, 1, 0)?;
                        let size = size.to_i64(strand).map_err(|_| {
                            Error::type_error(strand, "size must be a non-negative integer")
                        })?;
                        let size = u64::try_from(size).map_err(|_| {
                            Error::type_error(strand, "size must be a non-negative integer")
                        })?;
                        let annex = this.annex();
                        super::set_size(strand, annex.global, annex.as_path(), size).await
                    })
                    .method("sync", async move |this, strand, args, _out| {
                        let ([], [data]) = unpack!(strand, args, 0, 0, data_kw = None)?;
                        let data = data
                            .map(|data| crate::util::bool(strand, data, "data"))
                            .transpose()?
                            .unwrap_or(false);
                        let annex = this.annex();
                        super::sync_file(strand, annex.global, annex.as_path(), data).await
                    })
                    .method("copy", async move |this, strand, args, _out| {
                        let ([to], [all]) = unpack!(strand, args, 1, 0, all = None)?;
                        let all = match all {
                            Some(v) => v
                                .as_bool(strand)
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
                            None => false,
                        };
                        let to = path_from_value(strand, &to)?;
                        let annex = this.annex();
                        super::copy(strand, annex.global, annex.as_path(), to.to_path(), all).await
                    })
                    .method("rename", async move |this, strand, args, _out| {
                        let ([to], [replace]) = unpack!(strand, args, 1, 0, replace = None)?;
                        let to = path_from_value(strand, &to)?;
                        let replace = replace
                            .map(|value| crate::util::bool(strand, value, "replace"))
                            .transpose()?
                            .unwrap_or(true);
                        let annex = this.annex();
                        super::rename(strand, annex.global, annex.as_path(), to.to_path(), replace)
                            .await
                    })
                    .method("move", async move |this, strand, args, _out| {
                        let ([to], [all]) = unpack!(strand, args, 1, 0, all = None)?;
                        let all = match all {
                            Some(v) => v
                                .as_bool(strand)
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
                            None => false,
                        };
                        let to = path_from_value(strand, &to)?;
                        let annex = this.annex();
                        super::move_(strand, annex.global, annex.as_path(), to.to_path(), all).await
                    })
                    .method("hard_link", async move |this, strand, args, _out| {
                        let ([to], []) = unpack!(strand, args, 1, 0)?;
                        let to = path_from_value(strand, &to)?;
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
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
                            None => false,
                        };
                        let ignore = match ignore {
                            Some(v) => v
                                .as_bool(strand)
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
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
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
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
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
                            None => false,
                        };
                        let ignore = match ignore {
                            Some(v) => v
                                .as_bool(strand)
                                .ok_or_else(|| Error::type_error(strand, "expected Bool"))?,
                            None => false,
                        };
                        let annex = this.annex();
                        super::remove_dir(strand, annex.global, annex.as_path(), all, ignore).await
                    })
                    .method("update_metadata", async move |this, strand, args, _out| {
                        let (
                            [],
                            [
                                mode,
                                owner,
                                group,
                                modified,
                                accessed,
                                created,
                                resolve,
                                readonly,
                                hidden,
                                system,
                                archive,
                                compressed,
                                sparse,
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
                            mode = None,
                            owner = None,
                            group = None,
                            modified = None,
                            accessed = None,
                            created = None,
                            resolve = None,
                            readonly = None,
                            hidden = None,
                            system = None,
                            archive = None,
                            compressed = None,
                            sparse = None,
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
                        let attrs = super::attrs_patch(
                            strand,
                            [
                                (AttrFlags::READONLY, readonly),
                                (AttrFlags::HIDDEN, hidden),
                                (AttrFlags::SYSTEM, system),
                                (AttrFlags::ARCHIVE, archive),
                                (AttrFlags::COMPRESSED, compressed),
                                (AttrFlags::SPARSE, sparse),
                                (AttrFlags::TEMPORARY, temporary),
                                (AttrFlags::OFFLINE, offline),
                                (AttrFlags::NOT_CONTENT_INDEXED, not_content_indexed),
                                (AttrFlags::IMMUTABLE, immutable),
                                (AttrFlags::APPEND_ONLY, append_only),
                                (AttrFlags::NO_DUMP, no_dump),
                                (AttrFlags::NO_ATIME, no_atime),
                                (AttrFlags::NO_COPY_ON_WRITE, no_copy_on_write),
                                (AttrFlags::DIR_SYNC, dir_sync),
                                (AttrFlags::CASEFOLD, casefold),
                                (AttrFlags::DATA_JOURNALING, data_journaling),
                                (AttrFlags::NO_COMPRESS, no_compress),
                                (AttrFlags::PROJECT_INHERIT, project_inherit),
                                (AttrFlags::SECURE_DELETE, secure_delete),
                                (AttrFlags::SYNC, sync),
                                (AttrFlags::NO_TAIL_MERGE, no_tail_merge),
                                (AttrFlags::TOP_DIR, top_dir),
                                (AttrFlags::UNDELETE, undelete),
                                (AttrFlags::DIRECT_ACCESS, direct_access),
                                (AttrFlags::EXTENT_FORMAT, extent_format),
                                (AttrFlags::OPAQUE, opaque),
                            ],
                        )?;
                        let global = this.annex().global;
                        let patch = super::metadata_patch(
                            strand,
                            global,
                            [mode, owner, group],
                            [modified, accessed, created],
                            resolve,
                            attrs,
                        )?;
                        let annex = this.annex();
                        super::update_metadata(
                            strand,
                            annex.global,
                            vec![annex.as_path().to_path_buf()],
                            patch,
                        )
                        .await
                    });
                let builder = if matches!($style, vfs_path::Kind::Windows) {
                    builder
                        .get("stream_name", |this, strand, out| {
                            let annex = this.annex();
                            if let Some(spec) = stream_spec(strand, annex.as_path())? {
                                Output::set(strand, out, spec.name());
                            }
                            Ok(())
                        })
                        .get("stream_type", |this, strand, out| {
                            let annex = this.annex();
                            let stream_type = stream_spec(strand, annex.as_path())?
                                .and_then(|spec| spec.stream_type());
                            if let Some(stream_type) = stream_type {
                                Output::set(strand, out, stream_type);
                            }
                            Ok(())
                        })
                        .get("disk", |this, strand, out| {
                            let annex = this.annex();
                            let disk = match annex.windows_prefix() {
                                Some(vfs_path::WindowsPrefix::Disk(disk))
                                | Some(vfs_path::WindowsPrefix::VerbatimDisk(disk)) => Some(disk),
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
                            let server = match annex.windows_prefix() {
                                Some(vfs_path::WindowsPrefix::UNC(server, _))
                                | Some(vfs_path::WindowsPrefix::VerbatimUNC(server, _)) => {
                                    Some(server.to_owned())
                                }
                                _ => None,
                            };
                            if let Some(server) = server {
                                Output::set(strand, out, server.as_str());
                            }
                            Ok(())
                        })
                        .get("share", |this, strand, out| {
                            let annex = this.annex();
                            let share = match annex.windows_prefix() {
                                Some(vfs_path::WindowsPrefix::UNC(_, share))
                                | Some(vfs_path::WindowsPrefix::VerbatimUNC(_, share)) => {
                                    Some(share.to_owned())
                                }
                                _ => None,
                            };
                            if let Some(share) = share {
                                Output::set(strand, out, share.as_str());
                            }
                            Ok(())
                        })
                        .get("device", |this, strand, out| {
                            let annex = this.annex();
                            let device = match annex.windows_prefix() {
                                Some(vfs_path::WindowsPrefix::DeviceNS(device)) => {
                                    Some(device.to_owned())
                                }
                                _ => None,
                            };
                            if let Some(device) = device {
                                Output::set(strand, out, device.as_str());
                            }
                            Ok(())
                        })
                        .get("is_verbatim", |this, strand, out| {
                            let annex = this.annex();
                            let verbatim = annex
                                .windows_prefix()
                                .is_some_and(|prefix| prefix.is_verbatim());
                            Output::set(strand, out, verbatim);
                            Ok(())
                        })
                } else {
                    builder
                };
                builder
                    .get("components", |this, strand, out| {
                        Output::set(
                            strand,
                            out,
                            ArrayView::new(this, Components(PhantomData::<$path>)),
                        );
                        Ok(())
                    })
                    .method("glob", async move |this, strand, args, out| {
                        let ([pattern], [max_depth, resolve]) =
                            unpack!(strand, args, 1, 0, max_depth = None, resolve = None)?;
                        let annex = this.annex();
                        super::glob(
                            strand,
                            annex.global,
                            Some(annex.as_path()),
                            pattern,
                            max_depth,
                            resolve,
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
                        rewrite_path(strand, &annex, out, |path| {
                            let Some(name) = path.file_name() else {
                                return;
                            };
                            let mut name = name.to_owned();
                            if !ext.is_empty() {
                                name.push('.');
                                name.push_str(&ext);
                            }
                            path.set_file_name(name);
                        })
                    })
                    .method("without_ext", async move |this, strand, args, out| {
                        let ([], []) = unpack!(strand, args, 0, 0)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, out, |path| {
                            let _ = path.set_extension("");
                        })
                    })
                    .method("with_ext", async move |this, strand, args, out| {
                        let ([ext], []) = unpack!(strand, args, 1, 0)?;
                        let ext = expect_str(strand, &ext)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, out, |path| {
                            let _ = path.set_extension(ext);
                        })
                    })
                    .method("with_name", async move |this, strand, args, out| {
                        let ([name], []) = unpack!(strand, args, 1, 0)?;
                        let name = expect_str(strand, &name)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, out, |path| path.set_file_name(name))
                    })
                    .method("with_stem", async move |this, strand, args, out| {
                        let ([stem], []) = unpack!(strand, args, 1, 0)?;
                        let stem = expect_str(strand, &stem)?;
                        let annex = this.annex();
                        rewrite_path(strand, &annex, out, |path| {
                            let name = match path.extension() {
                                Some(ext) => format!("{stem}.{ext}"),
                                None => stem.clone(),
                            };
                            path.set_file_name(name);
                        })
                    })
                    .type_method("join", async move |this, strand, args, out| {
                        let global = strand.state::<FsGlobal<'v>>();
                        let mut buf = match $style {
                            vfs_path::Kind::Unix => vfs_path::PathBuf::from_unix(""),
                            vfs_path::Kind::Windows => vfs_path::PathBuf::from_windows(""),
                        };
                        let ([], [], paths) = unpack!(strand, args, 0, 0, *)?;
                        for slot in paths {
                            let path = concrete_path_from_value(strand, global, &slot, $style)?;
                            buf.push(path.as_str());
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
                if let Some(other) = path_object_from_value(strand, global, other) {
                    Ok(borrow.path_buf() == other)
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
                if let Some(other) = path_object_from_value(strand, global, other) {
                    Ok(borrow.path_buf() < other)
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
                    let other = convert_path_kind(strand, other, $style)?;
                    let path = borrow.as_path().without_stream().join(other.as_str());
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
                    let other = convert_path_kind(strand, other, $style)?;
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

impl_concrete_path!(UnixPath, "fs.unix", vfs_path::Kind::Unix);
impl_concrete_path!(WindowsPath, "fs.windows", vfs_path::Kind::Windows);
