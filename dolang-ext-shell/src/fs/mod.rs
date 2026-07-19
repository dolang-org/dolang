use dolang::runtime::{
    Arg, Error, Output, Result, Slot, State, Strand, call, unpack,
    value::{BinEmbryo, View},
    vm::Builder,
};
use dolang_shell_vfs::{
    AttrFlags, AttrsPatch, DACL_SECURITY_INFORMATION, FileHandle, FileType,
    GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, OpenOptions, SACL_SECURITY_INFORMATION,
    SecDesc, Utf8TypedPath, Utf8TypedPathBuf, Vfs, WellKnownPath,
};
use std::{
    future::poll_fn,
    io::{self, ErrorKind},
    mem::MaybeUninit,
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
        path::{PathAnnex, convert_path_type, create_path_annex, path_from_value, safe_concat},
        readdir::{DirEntryIter, DirEntryIterAnnex},
    },
    global::Global,
    security,
    time::datetime_to_system_time,
    util,
};

fn sec_desc_mask<'v, 's>(
    strand: &mut Strand<'v, 's>,
    owner: Option<Slot<'v, '_>>,
    group: Option<Slot<'v, '_>>,
    dacl: Option<Slot<'v, '_>>,
    sacl: Option<Slot<'v, '_>>,
) -> Result<'v, 's, u32> {
    fn selected<'v, 's>(
        strand: &mut Strand<'v, 's>,
        value: Option<Slot<'v, '_>>,
        default: bool,
    ) -> Result<'v, 's, bool> {
        value
            .map(|value| util::bool(strand, value, "security descriptor component"))
            .transpose()
            .map(|value| value.unwrap_or(default))
    }
    let mut mask = 0;
    if selected(strand, owner, true)? {
        mask |= OWNER_SECURITY_INFORMATION;
    }
    if selected(strand, group, true)? {
        mask |= GROUP_SECURITY_INFORMATION;
    }
    if selected(strand, dacl, true)? {
        mask |= DACL_SECURITY_INFORMATION;
    }
    if selected(strand, sacl, false)? {
        mask |= SACL_SECURITY_INFORMATION;
    }
    Ok(mask)
}

async fn sec_desc<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    mask: u32,
    follow: bool,
    mut out: Slot<'v, '_>,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let descriptor = global
        .local
        .get(strand)
        .vfs()
        .sec_desc(path.to_path(), mask, follow)
        .await
        .into_sys(strand)?;
    security::create_sec_desc(strand, global, descriptor, &mut out);
    Ok(())
}

async fn set_sec_desc<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    descriptor: &SecDesc,
    follow: bool,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    global
        .local
        .get(strand)
        .vfs()
        .set_sec_desc(path.to_path(), descriptor, follow)
        .await
        .into_sys(strand)
}

use glob::{GlobIter, GlobIterAnnex};

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

pub(crate) fn prepend_cwd<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
) -> Result<'v, 's, Utf8TypedPathBuf> {
    let cwd = global.local.get(strand).cwd().clone();
    safe_concat(strand, cwd.to_path(), path)
}

async fn metadata<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    follow: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let metadata = if follow {
        vfs.metadata(path.to_path()).await
    } else {
        vfs.symlink_metadata(path.to_path()).await
    }
    .into_sys(strand)?;
    create_metadata(strand, global, metadata, out);
    Ok(())
}

async fn fs_metadata<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    follow: bool,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let metadata = vfs
        .fs_metadata(path.to_path(), follow)
        .await
        .into_sys(strand)?;
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

fn attrs_patch<'v, 's, 'a>(
    strand: &mut Strand<'v, 's>,
    values: impl IntoIterator<Item = (AttrFlags, Option<Slot<'v, 'a>>)>,
) -> Result<'v, 's, AttrsPatch>
where
    'v: 'a,
{
    let mut patch = AttrsPatch::default();
    for (flag, value) in values {
        patch.update(flag, parse_attr_bool(strand, value)?);
    }
    Ok(patch)
}

pub(crate) fn resolve_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    slot: Option<Slot<'v, '_>>,
    default: bool,
) -> Result<'v, 's, bool> {
    let slot = match slot {
        Some(slot) => slot,
        None => return Ok(default),
    };
    let sym = slot
        .as_sym(strand)
        .ok_or_else(|| Error::type_error(strand, "resolve: expected :TARGET: or :LINK:"))?;
    if sym == global.syms.target {
        Ok(true)
    } else if sym == global.syms.link {
        Ok(false)
    } else {
        Err(Error::value(strand, "resolve: expected :TARGET: or :LINK:"))
    }
}

fn metadata_patch<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    mode: Option<Slot<'v, '_>>,
    user: Option<Slot<'v, '_>>,
    group: Option<Slot<'v, '_>>,
    resolve: Option<Slot<'v, '_>>,
    attrs: AttrsPatch,
) -> Result<'v, 's, dolang_shell_vfs::MetadataPatch> {
    let mode = mode.map(|mode| mode.to_u32(strand)).transpose()?;
    let user = user
        .map(|user| parse_ownership_identity(strand, global, &user, "user"))
        .transpose()?;
    let group = group
        .map(|group| parse_ownership_identity(strand, global, &group, "group"))
        .transpose()?;
    let follow = resolve_sym(strand, global, resolve, true)?;
    Ok(dolang_shell_vfs::MetadataPatch {
        mode,
        user,
        group,
        attrs,
        follow,
    })
}

async fn set_metadata<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    paths: Vec<Utf8TypedPathBuf>,
    patch: dolang_shell_vfs::MetadataPatch,
) -> Result<'v, 's, ()> {
    let paths = paths
        .into_iter()
        .map(|path| prepend_cwd(strand, global, path.to_path()))
        .collect::<Result<'v, 's, Vec<_>>>()?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.set_metadata(&paths, patch).await.into_sys(strand)?;
    Ok(())
}

async fn remove<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    all: bool,
    ignore: bool,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();

    let result = if all {
        match vfs.symlink_metadata(path.to_path()).await {
            Ok(metadata) if metadata.file_type == FileType::Dir => {
                vfs.remove(path.to_path(), true, ignore).await
            }
            Ok(_) => vfs.remove(path.to_path(), false, ignore).await.map(|_| ()),
            Err(e) => Err(e),
        }
    } else {
        vfs.remove(path.to_path(), false, ignore).await.map(|_| ())
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
    path: Utf8TypedPath<'_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let res = vfs.metadata(path.to_path()).await.map(|_| ());
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
    path: Utf8TypedPathBuf,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let full = prepend_cwd(strand, global, path.to_path())?;
    let local = global.local.get(strand);
    let read_dir = local
        .vfs()
        .read_dir(full.to_path())
        .await
        .into_sys(strand)?;

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
    path: Utf8TypedPath<'_>,
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
    let mut file = file::open(strand, global, path, mode).await?;
    let mut embryo = BinEmbryo::new_with_capacity(strand, 8192);
    let read_result = read_all(strand, &mut file, &mut embryo).await;
    let close_result = file.close().await;
    read_result?;
    close_result.into_sys(strand)?;
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
    path: Utf8TypedPath<'_>,
    data: Slot<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    write_with_mode(strand, global, path, data, out, "w").await
}

async fn append<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    data: Slot<'v, '_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    write_with_mode(strand, global, path, data, out, "a").await
}

async fn write_with_mode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    data: Slot<'v, '_>,
    out: impl Output<'v>,
    mode: &str,
) -> Result<'v, 's, ()> {
    let mut file = file::open(strand, global, path, mode).await?;
    let bytes_written = match data.view(strand) {
        View::Str(s) => {
            let s = s.pin();
            file.write_all(s.as_bytes()).await.map(|_| s.len())
        }
        View::Bin(b) => {
            let b = b.pin();
            file.write_all(&b).await.map(|_| b.len())
        }
        _ => {
            let _ = file.close().await;
            return Err(Error::type_error(strand, "expected `str` or `bin`"));
        }
    };

    let close_result = file.close().await;
    let bytes_written = bytes_written.into_sys(strand)?;
    close_result.into_sys(strand)?;
    Output::set(strand, out, bytes_written);
    Ok(())
}

async fn set_len<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    size: u64,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let mut file = vfs
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .open(path.to_path())
        .await
        .into_sys(strand)?;
    let set_len_result = file.set_len(size).await;
    let close_result = file.close().await;
    set_len_result.into_sys(strand)?;
    close_result.into_sys(strand)?;
    Ok(())
}

async fn copy<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: Utf8TypedPath<'_>,
    to: Utf8TypedPath<'_>,
    all: bool,
) -> Result<'v, 's, ()> {
    let from_path = prepend_cwd(strand, global, from)?;
    let to_path = prepend_cwd(strand, global, to)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.copy(from_path.to_path(), to_path.to_path(), all)
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn move_<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: Utf8TypedPath<'_>,
    to: Utf8TypedPath<'_>,
    all: bool,
) -> Result<'v, 's, ()> {
    let from_path = prepend_cwd(strand, global, from)?;
    let to_path = prepend_cwd(strand, global, to)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.move_(from_path.to_path(), to_path.to_path(), all)
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn rename<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    from: Utf8TypedPath<'_>,
    to: Utf8TypedPath<'_>,
) -> Result<'v, 's, ()> {
    let from_path = prepend_cwd(strand, global, from)?;
    let to_path = prepend_cwd(strand, global, to)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.rename(from_path.to_path(), to_path.to_path())
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn symlink<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: Utf8TypedPath<'_>,
    dst: Utf8TypedPath<'_>,
) -> Result<'v, 's, ()> {
    let cwd = global.local.get(strand).cwd().clone();
    let target = global
        .local
        .get(strand)
        .target()
        .operating_system
        .path_type();
    let src = convert_path_type(strand, src.to_path_buf(), &target)?;
    let dst = safe_concat(strand, cwd.to_path(), dst)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.symlink(cwd.to_path(), src.to_path(), dst.to_path())
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn hard_link<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: Utf8TypedPath<'_>,
    dst: Utf8TypedPath<'_>,
) -> Result<'v, 's, ()> {
    let src_path = prepend_cwd(strand, global, src)?;
    let dst_path = prepend_cwd(strand, global, dst)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.hard_link(src_path.to_path(), dst_path.to_path())
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn symlink_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: Utf8TypedPath<'_>,
    dst: Utf8TypedPath<'_>,
) -> Result<'v, 's, ()> {
    let target = global
        .local
        .get(strand)
        .target()
        .operating_system
        .path_type();
    let src = convert_path_type(strand, src.to_path_buf(), &target)?;
    let dst = prepend_cwd(strand, global, dst)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.symlink_dir(src.to_path(), dst.to_path())
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn symlink_file<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    src: Utf8TypedPath<'_>,
    dst: Utf8TypedPath<'_>,
) -> Result<'v, 's, ()> {
    let target = global
        .local
        .get(strand)
        .target()
        .operating_system
        .path_type();
    let src = convert_path_type(strand, src.to_path_buf(), &target)?;
    let dst = prepend_cwd(strand, global, dst)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.symlink_file(src.to_path(), dst.to_path())
        .await
        .into_sys(strand)?;
    Ok(())
}

async fn create_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    all: bool,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.create_dir(path.to_path(), all).await.into_sys(strand)?;
    Ok(())
}

async fn remove_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    all: bool,
    ignore: bool,
) -> Result<'v, 's, ()> {
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let result = vfs.remove_dir(path.to_path(), all, ignore).await;
    match result {
        Ok(()) => Ok(()),
        Err(e) if ignore && e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into_sys(strand)),
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
    path: Utf8TypedPath<'_>,
    modified: Option<Slot<'v, '_>>,
    accessed: Option<Slot<'v, '_>>,
    created: Option<Slot<'v, '_>>,
    resolve: Option<Slot<'v, '_>>,
) -> Result<'v, 's, ()> {
    let modified = parse_timestamp_arg(strand, global, modified, "modified")?;
    let accessed = parse_timestamp_arg(strand, global, accessed, "accessed")?;
    let created = parse_timestamp_arg(strand, global, created, "created")?;
    let modified = system_time_to_unix_timestamp(strand, modified)?;
    let accessed = system_time_to_unix_timestamp(strand, accessed)?;
    let created = system_time_to_unix_timestamp(strand, created)?;
    let follow = resolve_sym(strand, global, resolve, true)?;
    let path = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    vfs.set_times(path.to_path(), accessed, modified, created, follow)
        .await
        .into_sys(strand)?;
    Ok(())
}

fn parse_ownership_identity<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    value: &Value<'v>,
    field: &'static str,
) -> Result<'v, 's, dolang_shell_vfs::OwnershipIdentity> {
    if let Some(value) = value.as_int(strand) {
        let value = u32::try_from(value)
            .map_err(|_| Error::type_error(strand, "expected non-negative int or str"))?;
        Ok(dolang_shell_vfs::OwnershipIdentity::Id(value))
    } else if let Some(value) = value.as_str(strand) {
        Ok(dolang_shell_vfs::OwnershipIdentity::Name(value.to_string()))
    } else if let Some(value) = global.types.sid.downcast(value) {
        Ok(dolang_shell_vfs::OwnershipIdentity::Sid(
            value.annex().clone(),
        ))
    } else {
        Err(Error::type_error(
            strand,
            match field {
                "user" => "user: expected int, str, or security.Sid",
                "group" => "group: expected int, str, or security.Sid",
                _ => "expected int, str, or security.Sid",
            },
        ))
    }
}

/// Shared implementation for `fs.absolute` and `Path.absolute`.
pub(crate) fn path_absolute<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        prepend_cwd(strand, global, path)?
    };
    let annex = PathAnnex::try_new(strand, absolute, global)?;
    create_path_annex(strand, annex, out);
    Ok(())
}

async fn well_known_path<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    key: WellKnownPath,
    app: Option<&str>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let env = local.env().flatten_delta();
    let path = vfs.well_known_path(key, app, &env).await.into_sys(strand)?;
    let annex = PathAnnex::try_new(strand, path, global)?;
    create_path_annex(strand, annex, out);
    Ok(())
}

/// Shared implementation for `fs.relative` and `Path.relative`.
pub(crate) fn path_relative<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    base: Option<Slot<'v, '_>>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let relative = match base {
        Some(b) => path.strip_prefix(path_from_value(strand, global, &b)?.as_str()),
        None => path.strip_prefix(global.local.get(strand).cwd().as_str()),
    };
    let relative = relative
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf());
    let annex = PathAnnex::try_new(strand, relative, global)?;
    create_path_annex(strand, annex, out);
    Ok(())
}

/// Shared implementation for `fs.canonical` and `Path.canonical`.
pub(crate) async fn path_canonical<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    path: Utf8TypedPath<'_>,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    let absolute = prepend_cwd(strand, global, path)?;
    let local = global.local.get(strand);
    let vfs = local.vfs();
    let canonical = vfs
        .canonicalize(absolute.to_path())
        .await
        .into_sys(strand)?;
    let annex = PathAnnex::try_new(strand, canonical, global)?;
    create_path_annex(strand, annex, out);
    Ok(())
}

async fn glob<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    root: Option<Utf8TypedPath<'_>>,
    pattern: Slot<'v, '_>,
    max_depth: Option<Slot<'v, '_>>,
    resolve: Option<Slot<'v, '_>>,
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
    let follow = resolve_sym(strand, global, resolve, false)?;

    let root = root.unwrap_or_else(|| {
        match global
            .local
            .get(strand)
            .target()
            .operating_system
            .path_type()
        {
            dolang_shell_vfs::PathType::Unix => {
                Utf8TypedPath::Unix(dolang_shell_vfs::Utf8UnixPath::new(""))
            }
            dolang_shell_vfs::PathType::Windows => {
                Utf8TypedPath::Windows(dolang_shell_vfs::Utf8WindowsPath::new(""))
            }
        }
    });
    let abs_root = prepend_cwd(strand, global, root)?;
    let vfs = global.local.get(strand).vfs();

    let paths = vfs
        .glob(pattern, abs_root.to_path(), follow, max_depth)
        .await
        .into_sys(strand)?;

    global.types.glob_iter.create_with_annex(
        strand,
        GlobIter {
            paths: paths.into(),
        },
        GlobIterAnnex {
            global,
            prefix: root.to_path_buf(),
        },
        out,
    );
    Ok(())
}

async fn create_temp_dir<'v, 's>(
    strand: &mut Strand<'v, 's>,
    global: State<'v, Global<'v>>,
    parent: Utf8TypedPath<'_>,
) -> dolang_shell_vfs::Result<Utf8TypedPathBuf> {
    let mut rng = rand::rng();
    let vfs = global.local.get(strand).vfs();
    for attempt in 0..1000 {
        let random_suffix: String = (0..16)
            .map(|_| rng.sample(Alphanumeric))
            .map(char::from)
            .collect();
        let temp_path = parent.join(format!("tmp_{}", random_suffix));
        let result = vfs.create_dir(temp_path.to_path(), false).await;
        match result {
            Ok(()) => return Ok(temp_path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 999 => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create temporary directory after many attempts",
    )
    .into())
}

pub(crate) fn configure_vm<'v>(builder: &mut Builder<'v>, global: State<'v, Global<'v>>) {
    let parent = builder.sym("parent");
    let all = builder.sym("all");
    let ignore = builder.sym("ignore");
    let max_depth = builder.sym("max_depth");
    let resolve = builder.sym("resolve");
    let mode = builder.sym("mode");
    let user = builder.sym("user");
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
    let app = builder.sym("app");
    let module = builder
        .module("fs")
        .function("open", async move |strand, args, out| {
            let ([path], [opt1, opt2]) = unpack!(strand, args, 1, 2)?;
            let path = path_from_value(strand, global, &path)?;
            File::open(strand, global, path.to_path(), opt1, opt2, out).await
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
                        remove(strand, global, path.to_path(), all, ignore).await?;
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            Ok(())
        })
        .function("metadata", async move |strand, args, out| {
            let ([path], [resolve]) = unpack!(strand, args, 1, 0, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            let follow = resolve_sym(strand, global, resolve, true)?;
            metadata(strand, global, path.to_path(), follow, out).await
        })
        .function("fs_metadata", async move |strand, args, out| {
            let ([path], [resolve]) = unpack!(strand, args, 1, 0, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            let follow = resolve_sym(strand, global, resolve, true)?;
            fs_metadata(strand, global, path.to_path(), follow, out).await
        })
        .function("sec_desc", async move |strand, args, out| {
            let ([path], [owner, group, dacl, sacl, resolve]) = unpack!(
                strand,
                args,
                1,
                0,
                owner = None,
                group = None,
                dacl = None,
                sacl = None,
                resolve = None
            )?;
            let path = path_from_value(strand, global, &path)?;
            let mask = sec_desc_mask(strand, owner, group, dacl, sacl)?;
            let follow = resolve_sym(strand, global, resolve, true)?;
            sec_desc(strand, global, path.to_path(), mask, follow, out).await
        })
        .function("set_sec_desc", async move |strand, args, _out| {
            let ([path, descriptor], [resolve]) = unpack!(strand, args, 2, 0, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            let descriptor = security::sec_desc_from_value(strand, global, &descriptor)?;
            let follow = resolve_sym(strand, global, resolve, true)?;
            set_sec_desc(strand, global, path.to_path(), &descriptor, follow).await
        })
        .function("xattrs", async move |strand, args, out| {
            let ([path], [namespace, resolve]) =
                unpack!(strand, args, 1, 0, namespace = None, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_list(strand, global, path.to_path(), namespace, resolve, out).await
        })
        .function("streams", async move |strand, args, out| {
            let ([path], [resolve]) = unpack!(strand, args, 1, 0, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            stream::path_list(strand, global, path.to_path(), resolve, out).await
        })
        .function("xattr", async move |strand, args, out| {
            let ([path, name], [namespace, resolve]) =
                unpack!(strand, args, 2, 0, namespace = None, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_get(
                strand,
                global,
                path.to_path(),
                &name,
                namespace,
                resolve,
                out,
            )
            .await
        })
        .function("set_xattr", async move |strand, args, _out| {
            let ([path, name, value], [namespace, resolve]) =
                unpack!(strand, args, 3, 0, namespace = None, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_set(
                strand,
                global,
                path.to_path(),
                &name,
                namespace,
                &value,
                resolve,
            )
            .await
        })
        .function("remove_xattr", async move |strand, args, _out| {
            let ([path, name], [namespace, resolve]) =
                unpack!(strand, args, 2, 0, namespace = None, resolve = None)?;
            let path = path_from_value(strand, global, &path)?;
            xattr::path_remove(strand, global, path.to_path(), &name, namespace, resolve).await
        })
        .function("exists", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            exists(strand, global, path.to_path(), out).await
        })
        .function("read", async move |strand, args, out| {
            let ([path], [mode]) = unpack!(strand, args, 1, 1)?;
            let path = path_from_value(strand, global, &path)?;
            read(strand, global, path.to_path(), mode, out).await
        })
        .function("write", async move |strand, args, out| {
            let ([path, data], []) = unpack!(strand, args, 2, 0)?;
            let path = path_from_value(strand, global, &path)?;
            write(strand, global, path.to_path(), data, out).await
        })
        .function("append", async move |strand, args, out| {
            let ([path, data], []) = unpack!(strand, args, 2, 0)?;
            let path = path_from_value(strand, global, &path)?;
            append(strand, global, path.to_path(), data, out).await
        })
        .function("set_len", async move |strand, args, _out| {
            let ([path, size], []) = unpack!(strand, args, 2, 0)?;
            let path = path_from_value(strand, global, &path)?;
            let size = size
                .to_i64(strand)
                .map_err(|_| Error::type_error(strand, "size must be a non-negative integer"))?;
            let size = u64::try_from(size)
                .map_err(|_| Error::type_error(strand, "size must be a non-negative integer"))?;
            set_len(strand, global, path.to_path(), size).await
        })
        .function("set_metadata", async move |strand, args, _out| {
            let (
                [],
                [
                    mode,
                    user,
                    group,
                    resolve,
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
                paths,
            ) = unpack!(
                strand,
                args,
                0,
                0,
                mode = None,
                user = None,
                group = None,
                resolve = None,
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
                opaque = None,
                ...
            )?;
            let attrs = attrs_patch(
                strand,
                [
                    (AttrFlags::READONLY, readonly),
                    (AttrFlags::HIDDEN, hidden),
                    (AttrFlags::SYSTEM, system),
                    (AttrFlags::ARCHIVE, archive),
                    (AttrFlags::COMPRESSED, compressed),
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
            let patch = metadata_patch(strand, global, mode, user, group, resolve, attrs)?;
            let mut requested_paths = Vec::new();
            for path in paths {
                match path {
                    Arg::Pos(path) => {
                        let path = path_from_value(strand, global, &path)?;
                        requested_paths.push(path);
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            if requested_paths.is_empty() {
                return Err(Error::missing_positional(strand, 0));
            }
            set_metadata(strand, global, requested_paths, patch).await?;
            Ok(())
        })
        .function("is_absolute", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            Output::set(strand, out, path.is_absolute());
            Ok(())
        })
        .function("home_dir", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            well_known_path(strand, global, WellKnownPath::HomeDir, None, out).await
        })
        .function("cache_dir", async move |strand, args, out| {
            let ([], [app]) = unpack!(strand, args, 0, 0, app = None)?;
            let app = app.and_then(|s| s.as_str(strand).map(|s| s.to_string()));
            well_known_path(strand, global, WellKnownPath::CacheDir, app.as_deref(), out).await
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
            copy(strand, global, from.to_path(), to.to_path(), all).await
        })
        .function("rename", async move |strand, args, _out| {
            let ([from, to], []) = unpack!(strand, args, 2, 0)?;
            let from = path_from_value(strand, global, &from)?;
            let to = path_from_value(strand, global, &to)?;
            rename(strand, global, from.to_path(), to.to_path()).await
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
            move_(strand, global, from.to_path(), to.to_path(), all).await
        })
        .function("symlink", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            symlink(strand, global, src.to_path(), dst.to_path()).await
        })
        .function("hard_link", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            hard_link(strand, global, src.to_path(), dst.to_path()).await
        })
        .function("symlink_dir", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            symlink_dir(strand, global, src.to_path(), dst.to_path()).await
        })
        .function("symlink_file", async move |strand, args, _out| {
            let ([src, dst], []) = unpack!(strand, args, 2, 0)?;
            let src = path_from_value(strand, global, &src)?;
            let dst = path_from_value(strand, global, &dst)?;
            symlink_file(strand, global, src.to_path(), dst.to_path()).await
        })
        .function("entries", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            entries(strand, global, path, out).await
        })
        .function("glob", async move |strand, args, out| {
            let ([pattern], [max_depth, resolve]) =
                unpack!(strand, args, 1, 0, max_depth = None, resolve = None)?;
            glob(strand, global, None, pattern, max_depth, resolve, out).await
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
            create_dir(strand, global, path.to_path(), all).await
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
                        remove_dir(strand, global, path.to_path(), all, ignore).await?;
                    }
                    Arg::Key(sym, _) => return Err(Error::unexpected_key(strand, sym)),
                }
            }
            Ok(())
        })
        .function("set_timestamps", async move |strand, args, _out| {
            let ([path], [modified, accessed, created, resolve]) = unpack!(
                strand,
                args,
                1,
                0,
                modified = None,
                accessed = None,
                created = None,
                resolve = None
            )?;
            let path = path_from_value(strand, global, &path)?;
            set_timestamps(
                strand,
                global,
                path.to_path(),
                modified,
                accessed,
                created,
                resolve,
            )
            .await
        });
    module
        .function("normalize", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            let normalized = path.normalize();
            let annex = PathAnnex::try_new(strand, normalized, global)?;
            create_path_annex(strand, annex, out);
            Ok(())
        })
        .function("absolute", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            path_absolute(strand, global, path.to_path(), out)
        })
        .function("relative", async move |strand, args, out| {
            let ([path], [base]) = unpack!(strand, args, 1, 1)?;
            let path = path_from_value(strand, global, &path)?;
            let base_path = match base {
                Some(slot) => path_from_value(strand, global, &slot)?,
                None => {
                    let local = global.local.get(strand);
                    local.cwd().clone()
                }
            };
            let relative = path
                .strip_prefix(base_path.as_str())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.clone());
            let annex = PathAnnex::try_new(strand, relative, global)?;
            create_path_annex(strand, annex, out);
            Ok(())
        })
        .function("canonical", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            path_canonical(strand, global, path.to_path(), out).await
        })
        .function("read_link", async move |strand, args, out| {
            let ([path], []) = unpack!(strand, args, 1, 0)?;
            let path = path_from_value(strand, global, &path)?;
            let path = prepend_cwd(strand, global, path.to_path())?;
            let local = global.local.get(strand);
            let vfs = local.vfs();
            let target = vfs.read_link(path.to_path()).await.into_sys(strand)?;
            let annex = PathAnnex::try_new(strand, target, global)?;
            create_path_annex(strand, annex, out);
            Ok(())
        })
        .function_with_slots(
            "with_temp_dir",
            async move |strand, args, out, [mut path]| {
                let ([callable], [parent]) = unpack!(strand, args, 1, 0, parent = None)?;
                let parent = match parent {
                    Some(p) => {
                        let p = path_from_value(strand, global, &p)?;
                        prepend_cwd(strand, global, p.to_path())?
                    }
                    None => {
                        let local = global.local.get(strand);
                        let env = local.env().flatten_delta();
                        local
                            .vfs()
                            .well_known_path(WellKnownPath::TempDir, None, &env)
                            .await
                            .into_sys(strand)?
                    }
                };
                let temp_path = create_temp_dir(strand, global, parent.to_path())
                    .await
                    .into_sys(strand)?;
                let annex = PathAnnex::try_new(strand, temp_path.clone(), global)?;
                create_path_annex(strand, annex, &mut path);
                let result = call!(strand, callable, out, &path).await;
                let _ = strand
                    .with_interrupt_mask(true, async move |strand| {
                        let local = global.local.get(strand);
                        let vfs = local.vfs();
                        vfs.remove(temp_path.to_path(), true, false).await
                    })
                    .await;
                result
            },
        )
        .value("Metadata", global.types.metadata)
        .value("FsMetadata", global.types.fs_metadata)
        .value("XattrEntry", global.types.xattr_entry)
        .value("StreamEntry", global.types.stream_entry)
        .value("DirEntry", global.types.dir_entry)
        .value("Path", global.types.path)
        .value("UnixPath", global.types.unix_path)
        .value("WindowsPath", global.types.windows_path)
        .commit();
}
