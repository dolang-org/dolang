use std::{
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
    error::ResultExt, object::TypeBuilder, unpack,
};
use dolang_shell_vfs::Vfs;

use super::file::File;

pub(crate) struct Path;

pub(crate) struct PathAnnex<'v> {
    pub(crate) inner: path::PathBuf,
    pub(crate) global: State<'v, Global<'v>>,
}

pub(crate) fn path_from_value<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
) -> Result<'v, 's, PathBuf> {
    if let Some(path) = global.types.path.downcast(value) {
        Ok(path.annex().inner.clone())
    } else if let Some(str) = value.as_str(strand) {
        Ok(strand.access(|x| path::Path::new(str.as_str(x)).to_owned()))
    } else {
        Err(Error::type_error(strand, "expected Path or str"))
    }
}

impl<'v> PathAnnex<'v> {
    pub(crate) fn new(path: path::PathBuf, global: State<'v, Global<'v>>) -> Self {
        // Canonicalize by splitting into components and rejoining
        // This naturally uses platform-native separator
        Self {
            inner: path.components().collect(),
            global,
        }
    }

    /// Returns the path with forward slashes as separator for platform-consistent display
    #[cfg(target_os = "windows")]
    fn forward_slash_display(&self) -> String {
        self.inner.to_string_lossy().replace('\\', "/")
    }

    #[cfg(not(target_os = "windows"))]
    fn forward_slash_display(&self) -> path::Display<'_> {
        self.inner.display()
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
        this.create_with_annex(strand, Path, PathAnnex::new(path, global), out);
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
        write!(w, "{}", this.annex().inner.display()).into_do(strand)
    }

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let all = builder.sym("all");
        let ignore = builder.sym("ignore");
        let max_depth = builder.sym("max_depth");
        let follow = builder.sym("follow");
        let modified = builder.sym("modified");
        let accessed = builder.sym("accessed");
        let created = builder.sym("created");
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
                    borrow.global.types.path.create_with_annex(
                        strand,
                        Path,
                        PathAnnex::new(path.to_owned(), borrow.global),
                        out,
                    );
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
                File::open(
                    strand,
                    this.annex().global,
                    this.annex().inner.clone(),
                    opt1,
                    opt2,
                    out,
                )
                .await
            })
            .method("metadata", async move |this, strand, args, out| {
                let ([], [follow]) = unpack!(strand, args, 0, 1)?;
                let follow = match follow {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => true,
                };
                super::metadata(
                    strand,
                    this.annex().global,
                    &this.annex().inner,
                    follow,
                    out,
                )
                .await
            })
            .method("exists", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                super::exists(strand, this.annex().global, &this.annex().inner, out).await
            })
            .method("read", async move |this, strand, args, out| {
                let ([], [mode]) = unpack!(strand, args, 0, 1)?;
                super::read(strand, this.annex().global, &this.annex().inner, mode, out).await
            })
            .method("write", async move |this, strand, args, out| {
                let ([data], []) = unpack!(strand, args, 1, 0)?;
                super::write(strand, this.annex().global, &this.annex().inner, data, out).await
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
                super::copy(strand, this.annex().global, &this.annex().inner, &to, all).await
            })
            .method("rename", async move |this, strand, args, _out| {
                let ([to], []) = unpack!(strand, args, 1, 0)?;
                let to = path_from_value(strand, this.annex().global, &to)?;
                super::rename(strand, this.annex().global, &this.annex().inner, &to).await
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
                super::move_(strand, this.annex().global, &this.annex().inner, &to, all).await
            })
            .method("entries", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                super::entries(strand, this.annex().global, this.annex().inner.clone(), out).await
            })
            .method("canonical", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                super::path_canonical(strand, this.annex().global, &this.annex().inner, out).await
            })
            .method("read_link", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let annex = this.annex();
                let global = annex.global;
                let local = global.local.get(strand);
                let path = local.cwd().as_ref().join(&annex.inner);
                let vfs = local.vfs();
                let target = vfs.read_link(&path).await.into_sys(strand)?;
                global.types.path.create_with_annex(
                    strand,
                    Path,
                    PathAnnex::new(target, global),
                    out,
                );
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
                super::remove(
                    strand,
                    this.annex().global,
                    &this.annex().inner,
                    all,
                    ignore,
                )
                .await
            })
            .method("create_dir", async move |this, strand, args, _out| {
                let ([], [all]) = unpack!(strand, args, 0, 0, all = None)?;
                let all = match all {
                    Some(v) => v
                        .as_bool(strand)
                        .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                    None => false,
                };
                super::create_dir(strand, this.annex().global, &this.annex().inner, all).await
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
                super::remove_dir(
                    strand,
                    this.annex().global,
                    &this.annex().inner,
                    all,
                    ignore,
                )
                .await
            })
            .method("chmod", async move |this, strand, args, _out| {
                let ([mode], []) = unpack!(strand, args, 1, 0)?;
                let mode = mode
                    .as_i64(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected int"))?
                    as u32;
                super::chmod(strand, this.annex().global, &this.annex().inner, mode).await
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
                super::set_timestamps(
                    strand,
                    this.annex().global,
                    &this.annex().inner,
                    modified,
                    accessed,
                    created,
                )
                .await
            });
        #[cfg(unix)]
        let builder = builder.method("chown", async move |this, strand, args, _out| {
            let global = this.annex().global;
            let (path, user, group, follow) =
                super::parse_chown_common(strand, global, args, Some(this.annex().inner.clone()))?;
            super::chown(strand, global, &path, user, group, follow).await
        });
        builder
            .method("glob", async move |this, strand, args, out| {
                let ([pattern], [max_depth, follow]) =
                    unpack!(strand, args, 1, 0, max_depth = None, follow = None)?;
                super::glob(
                    strand,
                    this.annex().global,
                    Some(&this.annex().inner),
                    pattern,
                    max_depth,
                    follow,
                    out,
                )
                .await
            })
            .method("normal", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                let normalized = normalize_path(&this.annex().inner);
                this.annex().global.types.path.create_with_annex(
                    strand,
                    Path,
                    PathAnnex::new(normalized, this.annex().global),
                    out,
                );
                Ok(())
            })
            .method("absolute", async move |this, strand, args, out| {
                let ([], []) = unpack!(strand, args, 0, 0)?;
                path_absolute(strand, this.annex().global, &this.annex().inner, out)
            })
            .method("relative", async move |this, strand, args, out| {
                let ([], [base]) = unpack!(strand, args, 0, 1)?;
                path_relative(strand, this.annex().global, &this.annex().inner, base, out)
            })
            .method("add_ext", async move |this, strand, args, out| {
                let ([ext], []) = unpack!(strand, args, 1, 0)?;
                let ext = ext
                    .as_str(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected str"))?
                    .to_string();
                let path = this.annex().inner.with_added_extension(ext);
                this.annex().global.types.path.create_with_annex(
                    strand,
                    Path,
                    PathAnnex::new(path, this.annex().global),
                    out,
                );
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
                this.create_with_annex(strand, Path, PathAnnex::new(buf, global), out);
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
            Ok(borrow.inner == other.annex().inner)
        } else {
            Err(Error::not_supported(strand))
        }
    }

    fn hash<'a, 's>(
        this: Instance<'v, 'a, Self>,
        _strand: &'a mut Strand<'v, 's>,
        hasher: &mut impl Hasher,
    ) -> Result<'v, 's, ()> {
        this.annex().inner.hash(hasher);
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
            Ok(borrow.inner < other.annex().inner)
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
            global.types.path.create_with_annex(
                strand,
                Path,
                PathAnnex::new(borrow.inner.join(&other), global),
                out,
            );
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
            global.types.path.create_with_annex(
                strand,
                Path,
                PathAnnex::new(other.join(&borrow.inner), global),
                out,
            );
            Ok(())
        } else {
            Err(Error::not_supported(strand))
        }
    }
}
