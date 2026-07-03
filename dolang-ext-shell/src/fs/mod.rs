use dolang::runtime::{
    Arg, Error, Output, Result, Slot, State, Strand, call, unpack,
    value::{BinEmbryo, View},
    vm::Builder,
};
use dolang_shell_vfs::{Attrs, FileType, OpenOptions, Vfs, WellKnownPath};
use std::{
    future::poll_fn,
    io::{self, ErrorKind},
    mem::MaybeUninit,
    path::PathBuf,
    pin::Pin,
    str, time,
};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};

use rand::{RngExt, distr::Alphanumeric};

pub(crate) mod attrs;
pub(crate) mod file;
pub(crate) mod fs_metadata;
pub(crate) mod glob;
pub(crate) mod metadata;
pub(crate) mod path;
pub(crate) mod readdir;
pub(crate) mod stream;
pub(crate) mod xattr;

use crate::{
    error::{ErrorExt as _, ResultExt as _},
    fs::{
        file::File,
        fs_metadata::create_fs_metadata,
        metadata::create_metadata,
        path::{Path, PathAnnex, normalize_path, path_from_value},
        readdir::{DirEntryIter, DirEntryIterAnnex},
    },
    global::Global,
    time::datetime_to_system_time,
    util,
};

use glob::{GlobIter, GlobIterAnnex};

#[cfg(unix)]
use dolang::runtime::Value;

pub(super) async fn read_into_spare(
    reader: &mut (impl AsyncRead + Unpin),
    spare: &mut [MaybeUninit<u8>],
) -> io::Result<usize> {
    let mut buf = ReadBuf::uninit(spare);
    poll_fn(|cx| Pin::new(&mut *reader).poll_read(cx, &mut buf)).await?;
    Ok(buf.filled().len())
}

pub(super) async fn read_all<'v, 's>(
    strand: &mut Strand<'v, 's>,
    reader: &mut (impl AsyncRead + Unpin),
    embryo: &mut BinEmbryo<'v>,
) -> Result<'v, 's, ()> {
    loop {
        if embryo.spare_capacity_mut().is_empty() {
            embryo.reserve(strand, 1);
        }
        let read = read_into_spare(reader, embryo.spare_capacity_mut())
            .await
            .into_sys(strand)?;
        if read == 0 {
            break;
        }
        unsafe { embryo.advance(read) };
    }
    Ok(())
}

async fn metadata<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    follow: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let metadata = if follow {
        vfs.metadata(&path).await
    } else {
        vfs.symlink_metadata(&path).await
    }
    .into_sys(strand)?;
    create_metadata(strand, global, metadata, out);
    Ok(())
}

async fn get_attrs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    follow: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let attrs = vfs.attrs(&path, follow).await.into_sys(strand)?;
    attrs::create_attrs(strand, global, attrs, out);
    Ok(())
}

async fn fs_metadata<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    follow: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let metadata = vfs.fs_metadata(&path, follow).await.into_sys(strand)?;
    create_fs_metadata(strand, global, metadata, out);
    Ok(())
}

fn parse_attr_bool<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: Option<Slot<'v, '_>>,
) -> Result<'v, 's, Option<bool>> {
    value
        .map(|value| util::bool(strand, value, "attr"))
        .transpose()
}

async fn set_attrs<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    attrs: Attrs,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    vfs.set_attrs(&path, attrs).await.into_sys(strand)?;
    Ok(())
}

async fn remove<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    all: bool,
    ignore: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();

    let result = if all {
        match vfs.symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type == FileType::Dir => {
                vfs.remove(&path, true, ignore).await
            }
            Ok(_) => vfs.remove(&path, false, ignore).await.map(|_| ()),
            Err(e) => Err(e),
        }
    } else {
        vfs.remove(&path, false, ignore).await.map(|_| ())
    };

    match result {
        Ok(()) => Ok(()),
        Err(e) if ignore && e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into_sys(strand)),
    }
}

async fn exists<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let res = vfs.metadata(&path).await.map(|_| ());
    Output::set(
        strand,
        out,
        match res {
            Ok(()) => true,
            Err(e) if e.kind() == ErrorKind::NotFound => false,
            Err(e) => return Err(e.into_sys(strand)),
        },
    );
    Ok(())
}

async fn entries<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: PathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let full = local.cwd().as_ref().join(&path);
    let read_dir = local.vfs().read_dir(full).await.into_sys(strand)?;

    global.types.dir_entry_iter.create_with_annex(
        strand,
        DirEntryIter { read_dir },
        DirEntryIterAnnex { global },
        out,
    );
    Ok(())
}

async fn read<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    mode: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let mode = match mode {
        None => "r",
        Some(mode) => match mode.as_str(strand) {
            Some(s) if s.to_string() == "b" => "rb",
            Some(_) => return Err(Error::type_error(strand, "fs.read: mode must be `b`")),
            None => return Err(Error::type_error(strand, "fs.read: mode must be a string")),
        },
    };
    let is_binary = mode == "rb";
    let mut file = file::open(strand, global, path, mode)
        .await
        .into_sys(strand)?;
    let mut embryo = BinEmbryo::new_with_capacity(strand, 8192);
    read_all(strand, &mut file, &mut embryo).await?;
    if is_binary {
        embryo.finish(strand, out);
    } else {
        embryo
            .finish_str(strand, out)
            .map_err(|_| Error::runtime(strand, "invalid UTF-8 data"))?;
    }
    Ok(())
}

async fn write<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    data: Slot<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let mut file = file::open(strand, global, path, "w")
        .await
        .into_sys(strand)?;
    let bytes_written = match data.view(strand) {
        View::Str(s) => {
            let s = s.pin();
            file.write_all(s.as_bytes()).await.map(|_| s.len())
        }
        View::Bin(b) => {
            let b = b.pin();
            file.write_all(&b).await.map(|_| b.len())
        }
        _ => return Err(Error::type_error(strand, "expected `str` or `bin`")),
    }
    .into_sys(strand)?;

    file.flush().await.into_sys(strand)?;
    Output::set(strand, out, bytes_written);
    Ok(())
}

async fn set_len<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    size: u64,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let file = vfs
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .await
        .into_sys(strand)?;
    file.set_len(size).await.into_sys(strand)?;
    Ok(())
}

async fn copy<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: &std::path::Path,
    to: &std::path::Path,
    all: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let from_path = local.cwd().as_ref().join(from);
    let to_path = local.cwd().as_ref().join(to);
    let vfs = local.vfs();
    vfs.copy(&from_path, &to_path, all).await.into_sys(strand)?;
    Ok(())
}

async fn move_<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: &std::path::Path,
    to: &std::path::Path,
    all: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let from_path = local.cwd().as_ref().join(from);
    let to_path = local.cwd().as_ref().join(to);
    let vfs = local.vfs();
    vfs.move_(&from_path, &to_path, all)
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn rename<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: &std::path::Path,
    to: &std::path::Path,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let from_path = local.cwd().as_ref().join(from);
    let to_path = local.cwd().as_ref().join(to);
    let vfs = local.vfs();
    vfs.rename(&from_path, &to_path).await.into_sys(strand)?;
    Ok(())
}

async fn symlink<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(src);
    let dst_path = local.cwd().as_ref().join(dst);
    let vfs = local.vfs();
    vfs.symlink(&src_path, &dst_path).await.into_sys(strand)?;
    Ok(())
}

async fn hard_link<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(src);
    let dst_path = local.cwd().as_ref().join(dst);
    let vfs = local.vfs();
    vfs.hard_link(&src_path, &dst_path).await.into_sys(strand)?;
    Ok(())
}

async fn symlink_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(src);
    let dst_path = local.cwd().as_ref().join(dst);
    let vfs = local.vfs();
    vfs.symlink_dir(&src_path, &dst_path)
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn symlink_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let src_path = local.cwd().as_ref().join(src);
    let dst_path = local.cwd().as_ref().join(dst);
    let vfs = local.vfs();
    vfs.symlink_file(&src_path, &dst_path)
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn create_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    all: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    vfs.create_dir(&path, all).await.into_sys(strand)?;
    Ok(())
}

async fn remove_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    all: bool,
    ignore: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let result = vfs.remove_dir(&path, all, ignore).await;
    match result {
        Ok(()) => Ok(()),
        Err(e) if ignore && e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into_sys(strand)),
    }
}

async fn chmod<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    mode: u32,
) -> Result<'v, 's, ()> {
    #[cfg(unix)]
    {
        let local = global.local.get(strand);
        let path = local.cwd().as_ref().join(path);
        let vfs = local.vfs();
        let perm = dolang_shell_vfs::Permissions::from_mode(mode);
        vfs.set_permissions(&path, perm).await.into_sys(strand)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (global, path, mode);
        Err(Error::runtime(
            strand,
            "chmod is not supported on this platform",
        ))
    }
}

fn parse_timestamp_arg<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: Option<Slot<'v, '_>>,
    name: &str,
) -> Result<'v, 's, Option<time::SystemTime>> {
    let Some(value) = value else {
        return Ok(None);
    };
    datetime_to_system_time(strand, global.types.date_time, &value)
        .map(Some)
        .map_err(|_| Error::type_error(strand, format!("{name}: expected DateTime")))
}

fn system_time_to_unix_timestamp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    time: Option<time::SystemTime>,
) -> Result<'v, 's, Option<(i64, u32)>> {
    let Some(time) = time else {
        return Ok(None);
    };
    match time.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(duration) => {
            let secs = i64::try_from(duration.as_secs()).map_err(|_| Error::overflow(strand))?;
            Ok(Some((secs, duration.subsec_nanos())))
        }
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).map_err(|_| Error::overflow(strand))?;
            if duration.subsec_nanos() == 0 {
                Ok(Some((
                    0i64.checked_sub(secs)
                        .ok_or_else(|| Error::overflow(strand))?,
                    0,
                )))
            } else {
                Ok(Some((
                    0i64.checked_sub(secs)
                        .and_then(|v| v.checked_sub(1))
                        .ok_or_else(|| Error::overflow(strand))?,
                    1_000_000_000u32 - duration.subsec_nanos(),
                )))
            }
        }
    }
}

async fn set_timestamps<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    modified: Option<Slot<'v, '_>>,
    accessed: Option<Slot<'v, '_>>,
    created: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ()> {
    let modified = parse_timestamp_arg(strand, global, modified, "modified")?;
    let accessed = parse_timestamp_arg(strand, global, accessed, "accessed")?;
    let created = parse_timestamp_arg(strand, global, created, "created")?;
    let modified = system_time_to_unix_timestamp(strand, modified)?;
    let accessed = system_time_to_unix_timestamp(strand, accessed)?;
    let created = system_time_to_unix_timestamp(strand, created)?;
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    vfs.set_times(&path, accessed, modified, created)
        .await
        .into_sys(strand)?;
    Ok(())
}

#[cfg(unix)]
fn parse_chown_identity<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    field: &'static str,
) -> Result<'v, 's, dolang_shell_vfs::ChownIdentity> {
    if let Some(value) = value.as_int(strand) {
        let value = u32::try_from(value)
            .map_err(|_| Error::type_error(strand, "expected non-negative int or str"))?;
        Ok(dolang_shell_vfs::ChownIdentity::Id(value))
    } else if let Some(value) = value.as_str(strand) {
        Ok(dolang_shell_vfs::ChownIdentity::Name(value.to_string()))
    } else {
        Err(Error::type_error(
            strand,
            match field {
                "user" => "user: expected int or str",
                "group" => "group: expected int or str",
                _ => "expected int or str",
            },
        ))
    }
}

#[cfg(unix)]
fn parse_chown_common<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    args: dolang::runtime::Args<'v, 'a>,
    path: Option<PathBuf>,
) -> Result<
    'v,
    's,
    (
        PathBuf,
        Option<dolang_shell_vfs::ChownIdentity>,
        Option<dolang_shell_vfs::ChownIdentity>,
        bool,
    ),
> {
    let mut positional_index = usize::from(path.is_some());
    let mut path = path;
    let mut user = None;
    let mut group = None;
    let mut follow = true;

    for arg in args {
        match arg {
            Arg::Pos(slot) => {
                if path.is_none() {
                    path = Some(path_from_value(strand, global, &slot)?);
                } else if user.is_none() {
                    user = Some(parse_chown_identity(strand, &slot, "user")?);
                } else {
                    return Err(Error::unexpected_positional(strand, positional_index));
                }
                positional_index += 1;
            }
            Arg::Key(sym, slot) if sym == global.syms.group => {
                group = Some(parse_chown_identity(strand, &slot, "group")?);
            }
            Arg::Key(sym, slot) if sym == global.syms.follow => {
                follow = slot
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "follow: expected bool"))?;
            }
            Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
        }
    }

    let path = path.ok_or_else(|| Error::missing_positional(strand, 0))?;
    if user.is_none() && group.is_none() {
        return Err(Error::runtime(
            strand,
            "chown requires at least one of user or group",
        ));
    }
    Ok((path, user, group, follow))
}

#[cfg(unix)]
async fn chown<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    user: Option<dolang_shell_vfs::ChownIdentity>,
    group: Option<dolang_shell_vfs::ChownIdentity>,
    follow: bool,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let path = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    vfs.chown(&path, user, group, follow)
        .await
        .into_sys(strand)?;
    Ok(())
}

/// Shared implementation for `fs.absolute` and `Path.absolute`.
pub(crate) fn path_absolute<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        local.cwd().as_ref().join(path)
    };
    let annex = PathAnnex::try_new(strand, absolute, global)?;
    global
        .types
        .path
        .create_with_annex(strand, Path, annex, out);
    Ok(())
}

async fn well_known_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    key: WellKnownPath,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let env = local.env().flatten_delta();
    let path = vfs.well_known_path(key, &env).await.into_sys(strand)?;
    let annex = PathAnnex::try_new(strand, path, global)?;
    global
        .types
        .path
        .create_with_annex(strand, Path, annex, out);
    Ok(())
}

/// Shared implementation for `fs.relative` and `Path.relative`.
pub(crate) fn path_relative<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    base: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let relative = match base {
        Some(b) => path.strip_prefix(&path_from_value(strand, global, &b)?),
        None => path.strip_prefix(global.local.get(strand).cwd().as_ref()),
    };
    let relative = relative
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf());
    let annex = PathAnnex::try_new(strand, relative, global)?;
    global
        .types
        .path
        .create_with_annex(strand, Path, annex, out);
    Ok(())
}

/// Shared implementation for `fs.canonical` and `Path.canonical`.
pub(crate) async fn path_canonical<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: &std::path::Path,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let absolute = local.cwd().as_ref().join(path);
    let vfs = local.vfs();
    let canonical = vfs.canonicalize(&absolute).await.into_sys(strand)?;
    let annex = PathAnnex::try_new(strand, canonical, global)?;
    global
        .types
        .path
        .create_with_annex(strand, Path, annex, out);
    Ok(())
}

async fn glob<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    root: Option<&std::path::Path>,
    pattern: Slot<'v, '_>,
    max_depth: Option<Slot<'v, '_>>,
    follow: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let pattern = pattern
        .as_str(strand)
        .ok_or_else(|| Error::type_error(strand, "pattern: expected str"))?
        .to_string();
    let max_depth = match max_depth {
        Some(v) => Some(v.to_usize(strand)?),
        None => None,
    };
    let follow = match follow {
        Some(v) => v
            .as_bool(strand)
            .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
        None => false,
    };

    let (cwd, vfs) = {
        let local = global.local.get(strand);
        (local.cwd().as_ref().to_owned(), local.vfs())
    };

    let paths = vfs
        .glob(pattern, root.unwrap_or(cwd.as_ref()), follow, max_depth)
        .await
        .into_sys(strand)?;

    global.types.glob_iter.create_with_annex(
        strand,
        GlobIter {
            paths: paths.into(),
        },
        GlobIterAnnex {
            global,
            prefix: root.map(|p| p.to_owned()).unwrap_or_default(),
        },
        out,
    );
    Ok(())
}

async fn create_temp_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    parent: &std::path::Path,
) -> io::Result<PathBuf> {
    let mut rng = rand::rng();
    let vfs = global.local.get(strand).vfs();
    for attempt in 0..1000 {
        let random_suffix: String = (0..16)
            .map(|_| rng.sample(Alphanumeric))
            .map(char::from)
            .collect();
        let temp_path = parent.join(format!("tmp_{}", random_suffix));
        let result = vfs.create_dir(&temp_path, false).await;
        match result {
            Ok(()) => return Ok(temp_path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 999 => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create temporary directory after many attempts",
    ))
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let parent = builder.sym("parent");
    let all = builder.sym("all");
    let ignore = builder.sym("ignore");
    let max_depth = builder.sym("max_depth");
    let follow = builder.sym("follow");
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
    let module = builder
        .module("fs")
        .function("open", async move |strand, args, out| {
            let ([path], [opt1, opt2]) = unpack!(strand, args, 1, 2)?;
            let path = path_from_value(strand, global, &path)?;
            File::open(strand, global, &path, opt1, opt2, out).await
        })
        .function("remove", async move |strand, args, _out| {
            let ([], [all, ignore], paths) =
                unpack!(strand, args, 0, 0, all = None, ignore = None, ...)?;
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
            for path in paths {
                match path {
                    Arg::Pos(path) => {
                        let path = path_from_value(strand, global, &path)?;
                        remove(strand, global, &path, all, ignore).await?;
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            Ok(())
        })
        .function("metadata", async move |strand, args, out| {
            let ([path], [follow]) = unpack!(strand, args, 1, 1)?;
            let path = path_from_value(strand, global, &path)?;
            let follow = match follow {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => true,
            };
            metadata(strand, global, &path, follow, out).await
        })
        .function("fs_metadata", async move |strand, args, out| {
            let ([path], [follow]) = unpack!(strand, args, 1, 0, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            let follow = match follow {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => true,
            };
            fs_metadata(strand, global, &path, follow, out).await
        })
        .function("attrs", async move |strand, args, out| {
            let ([path], [follow]) = unpack!(strand, args, 1, 0, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            let follow = match follow {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => true,
            };
            get_attrs(strand, global, &path, follow, out).await
        })
        .function("xattrs", async move |strand, args, out| {
            let ([path], [namespace, follow]) =
                unpack!(strand, args, 1, 0, namespace = None, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_list(strand, global, &path, namespace, follow, out).await
        })
        .function("streams", async move |strand, args, out| {
            let ([path], [follow]) = unpack!(strand, args, 1, 0, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            stream::path_list(strand, global, &path, follow, out).await
        })
        .function("xattr", async move |strand, args, out| {
            let ([path, name], [namespace, follow]) =
                unpack!(strand, args, 2, 0, namespace = None, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_get(strand, global, &path, &name, namespace, follow, out).await
        })
        .function("set_xattr", async move |strand, args, _out| {
            let ([path, name, value], [namespace, follow]) =
                unpack!(strand, args, 3, 0, namespace = None, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_set(strand, global, &path, &name, namespace, &value, follow).await
        })
        .function("remove_xattr", async move |strand, args, _out| {
            let ([path, name], [namespace, follow]) =
                unpack!(strand, args, 2, 0, namespace = None, follow = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_remove(strand, global, &path, &name, namespace, follow).await
        })
        .function("exists", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            exists(strand, global, &path, out).await
        })
        .function("read", async move |strand, args, out| {
            let ([path], [mode]) = unpack!(strand, args, 1, 1)?;
            let path = path_from_value(strand, global, &path)?;
            read(strand, global, &path, mode, out).await
        })
        .function("write", async move |strand, args, out| {
            let ([path, data], []) = unpack!(strand, args, 2, 0)?;
            let path = path_from_value(strand, global, &path)?;
            write(strand, global, &path, data, out).await
        })
        .function("set_len", async move |strand, args, _out| {
            let ([path, size], []) = unpack!(strand, args, 2, 0)?;
            let path = path_from_value(strand, global, &path)?;
            let size = size
                .to_i64(strand)
                .map_err(|_| Error::type_error(strand, "size must be a non-negative integer"))?;
            let size = u64::try_from(size)
                .map_err(|_| Error::type_error(strand, "size must be a non-negative integer"))?;
            set_len(strand, global, &path, size).await
        })
        .function("set_attrs", async move |strand, args, _out| {
            let (
                [path],
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
                1,
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
            let path = path_from_value(strand, global, &path)?;
            let attrs = Attrs {
                readonly: parse_attr_bool(strand, readonly)?,
                hidden: parse_attr_bool(strand, hidden)?,
                system: parse_attr_bool(strand, system)?,
                archive: parse_attr_bool(strand, archive)?,
                compressed: parse_attr_bool(strand, compressed)?,
                temporary: parse_attr_bool(strand, temporary)?,
                offline: parse_attr_bool(strand, offline)?,
                not_content_indexed: parse_attr_bool(strand, not_content_indexed)?,
                immutable: parse_attr_bool(strand, immutable)?,
                append_only: parse_attr_bool(strand, append_only)?,
                no_dump: parse_attr_bool(strand, no_dump)?,
                no_atime: parse_attr_bool(strand, no_atime)?,
                no_copy_on_write: parse_attr_bool(strand, no_copy_on_write)?,
                dir_sync: parse_attr_bool(strand, dir_sync)?,
                casefold: parse_attr_bool(strand, casefold)?,
                data_journaling: parse_attr_bool(strand, data_journaling)?,
                no_compress: parse_attr_bool(strand, no_compress)?,
                project_inherit: parse_attr_bool(strand, project_inherit)?,
                secure_delete: parse_attr_bool(strand, secure_delete)?,
                sync: parse_attr_bool(strand, sync)?,
                no_tail_merge: parse_attr_bool(strand, no_tail_merge)?,
                top_dir: parse_attr_bool(strand, top_dir)?,
                undelete: parse_attr_bool(strand, undelete)?,
                direct_access: parse_attr_bool(strand, direct_access)?,
                extent_format: parse_attr_bool(strand, extent_format)?,
                opaque: parse_attr_bool(strand, opaque)?,
                ..Attrs::default()
            };
            set_attrs(strand, global, &path, attrs).await
        })
        .function("is_absolute", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            Output::set(strand, out, path.is_absolute());
            Ok(())
        })
        .function("home_dir", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            well_known_path(strand, global, WellKnownPath::HomeDir, out).await
        })
        .function("cache_dir", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            well_known_path(strand, global, WellKnownPath::CacheDir, out).await
        })
        .function("copy", async move |strand, args, out| {
            let ([from, to], [all]) = unpack!(strand, args, 2, 0, all = None)?;
            let from = path_from_value(strand, global, &from)?;
            let to = path_from_value(strand, global, &to)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            let _ = out;
            copy(strand, global, &from, &to, all).await
        })
        .function("rename", async move |strand, args, _out| {
            let ([from, to], []) = unpack!(strand, args, 2, 0)?;
            let from = path_from_value(strand, global, &from)?;
            let to = path_from_value(strand, global, &to)?;
            rename(strand, global, &from, &to).await
        })
        .function("move", async move |strand, args, _out| {
            let ([from, to], [all]) = unpack!(strand, args, 2, 0, all = None)?;
            let from = path_from_value(strand, global, &from)?;
            let to = path_from_value(strand, global, &to)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            move_(strand, global, &from, &to, all).await
        })
        .function("symlink", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            symlink(strand, global, &src, &dst).await
        })
        .function("hard_link", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            hard_link(strand, global, &src, &dst).await
        })
        .function("symlink_dir", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            symlink_dir(strand, global, &src, &dst).await
        })
        .function("symlink_file", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            symlink_file(strand, global, &src, &dst).await
        })
        .function("entries", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            entries(strand, global, path, out).await
        })
        .function("glob", async move |strand, args, out| {
            let ([pattern], [max_depth, follow]) =
                unpack!(strand, args, 1, 0, max_depth = None, follow = None)?;
            glob(strand, global, None, pattern, max_depth, follow, out).await
        })
        .function("create_dir", async move |strand, args, _out| {
            let ([path], [all]) = unpack!(strand, args, 1, 0, all = None)?;
            let path = path_from_value(strand, global, &path)?;
            let all = match all {
                Some(v) => v
                    .as_bool(strand)
                    .ok_or_else(|| Error::type_error(strand, "expected bool"))?,
                None => false,
            };
            create_dir(strand, global, &path, all).await
        })
        .function("remove_dir", async move |strand, args, _out| {
            let ([], [all, ignore], paths) =
                unpack!(strand, args, 0, 0, all = None, ignore = None, ...)?;
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
            for path in paths {
                match path {
                    Arg::Pos(path) => {
                        let path = path_from_value(strand, global, &path)?;
                        remove_dir(strand, global, &path, all, ignore).await?;
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            Ok(())
        })
        .function("chmod", async move |strand, args, _out| {
            let ([path, mode], []) = unpack!(strand, args, 2, 0)?;
            let path = path_from_value(strand, global, &path)?;
            let mode = mode.to_u32(strand)?;
            chmod(strand, global, &path, mode).await
        })
        .function("set_timestamps", async move |strand, args, _out| {
            let ([path], [modified, accessed, created]) = unpack!(
                strand,
                args,
                1,
                0,
                modified = None,
                accessed = None,
                created = None
            )?;
            let path = path_from_value(strand, global, &path)?;
            set_timestamps(strand, global, &path, modified, accessed, created).await
        });
    #[cfg(unix)]
    let module = module.function("chown", async move |strand, args, _out| {
        let (path, user, group, follow) = parse_chown_common(strand, global, args, None)?;
        chown(strand, global, &path, user, group, follow).await
    });
    module
        .function("normal", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            let normalized = normalize_path(&path);
            let annex = PathAnnex::try_new(strand, normalized, global)?;
            global
                .types
                .path
                .create_with_annex(strand, Path, annex, out);
            Ok(())
        })
        .function("absolute", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            path_absolute(strand, global, &path, out)
        })
        .function("relative", async move |strand, args, out| {
            let ([path], [base]) = unpack!(strand, args, 1, 1)?;
            let path = path_from_value(strand, global, &path)?;
            let base_path = match base {
                Some(slot) => path_from_value(strand, global, &slot)?.to_path_buf(),
                None => {
                    let local = global.local.get(strand);
                    local.cwd().as_ref().to_path_buf()
                }
            };
            let relative = path
                .strip_prefix(&base_path)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.to_path_buf());
            let annex = PathAnnex::try_new(strand, relative, global)?;
            global
                .types
                .path
                .create_with_annex(strand, Path, annex, out);
            Ok(())
        })
        .function("canonical", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            path_canonical(strand, global, &path, out).await
        })
        .function_with_slots(
            "with_temp_dir",
            async move |strand, args, out, [mut path]| {
                let ([callable], [parent]) = unpack!(strand, args, 1, 0, parent = None)?;
                let parent = match parent {
                    Some(p) => {
                        let p = path_from_value(strand, global, &p)?;
                        let local = global.local.get(strand);
                        local.cwd().as_ref().join(&p)
                    }
                    None => {
                        let local = global.local.get(strand);
                        if cfg!(windows) {
                            std::env::temp_dir()
                        } else {
                            match local.env().get("TMPDIR") {
                                Some(dir) => local.cwd().as_ref().join(dir.as_ref()),
                                None => PathBuf::from("/tmp"),
                            }
                        }
                    }
                };
                let temp_path = create_temp_dir(strand, global, &parent)
                    .await
                    .into_sys(strand)?;
                let annex = PathAnnex::try_new(strand, temp_path.clone(), global)?;
                global
                    .types
                    .path
                    .create_with_annex(strand, Path, annex, &mut path);
                let result = call!(strand, callable, out, &path).await;
                let _ = strand
                    .with_interrupt_mask(true, async move |strand| {
                        let local = global.local.get(strand);
                        let vfs = local.vfs();
                        vfs.remove(&temp_path, true, false).await
                    })
                    .await;
                result
            },
        )
        .value("Metadata", global.types.metadata)
        .value("FsMetadata", global.types.fs_metadata)
        .value("Attrs", global.types.attrs)
        .value("XattrEntry", global.types.xattr_entry)
        .value("StreamEntry", global.types.stream_entry)
        .value("DirEntry", global.types.dir_entry)
        .value("Path", global.types.path)
        .commit();
}
