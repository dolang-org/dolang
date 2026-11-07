//! The parameters of `fs.open`: the mode-string shorthand and the granular
//! keywords, resolved into one [`OpenSpec`].

use dolang::runtime::{
    Error, Result, Slot, State, Strand, Sym, io::IoCaps, object::FlagsTypeExt, value::Value,
    vm::Register,
};
use dolang_vfs::{
    file::{OpenCaps, OpenOptions, ShareMode},
    metadata::{AttrFlags, Mode as VfsMode, Resolve},
};
use dolang_winterop::security::SecDesc;

use crate::{global::FsGlobal, util};

/// The granular keywords, in the order [`OpenSyms::keys`] holds them and
/// [`OpenSpec::from_args`] takes their values.
const KEY_NAMES: [&str; OPEN_KEYS] = [
    "read",
    "write",
    "access",
    "exists",
    "missing",
    "share",
    "backup",
    "resolve",
    "mode",
    "sec_desc",
    "readonly",
    "hidden",
    "system",
    "archive",
    "temporary",
    "offline",
    "not_content_indexed",
];

/// How many granular keywords `fs.open` takes.
pub(crate) const OPEN_KEYS: usize = 17;

/// The attributes the attribute keywords set, in [`KEY_NAMES`] order from
/// `readonly` on.
const ATTRS: [AttrFlags; 7] = [
    AttrFlags::READONLY,
    AttrFlags::HIDDEN,
    AttrFlags::SYSTEM,
    AttrFlags::ARCHIVE,
    AttrFlags::TEMPORARY,
    AttrFlags::OFFLINE,
    AttrFlags::NOT_CONTENT_INDEXED,
];

pub(crate) struct OpenSyms<'v> {
    /// The granular keywords, in [`KEY_NAMES`] order.
    pub(crate) keys: [Sym<'v, 'v>; OPEN_KEYS],
    pub(crate) buffer: Sym<'v, 'v>,
    open: Sym<'v, 'v>,
    truncate: Sym<'v, 'v>,
    fail: Sym<'v, 'v>,
    create: Sym<'v, 'v>,
    append: Sym<'v, 'v>,
    share_read: Sym<'v, 'v>,
    share_write: Sym<'v, 'v>,
    share_delete: Sym<'v, 'v>,
}

impl<'v> OpenSyms<'v> {
    pub(crate) fn new(builder: &mut Register<'v>) -> Self {
        Self {
            keys: KEY_NAMES.map(|name| builder.sym(name)),
            buffer: builder.sym("buffer"),
            open: builder.sym("OPEN"),
            truncate: builder.sym("TRUNCATE"),
            fail: builder.sym("FAIL"),
            create: builder.sym("CREATE"),
            append: builder.sym("APPEND"),
            share_read: builder.sym("READ"),
            share_write: builder.sym("WRITE"),
            share_delete: builder.sym("DELETE"),
        }
    }
}

/// Write access: `write:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Write {
    No,
    Yes,
    Append,
}

/// What to do when the file exists: `exists:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exists {
    Open,
    Truncate,
    Fail,
}

/// What to do when the file is missing: `missing:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Missing {
    Create,
    Fail,
}

/// Everything an open asks for, resolved from either form of `fs.open`'s
/// parameters.
#[derive(Debug, Default, Clone)]
pub(crate) struct OpenSpec {
    read: bool,
    write: bool,
    append: bool,
    access: Option<u32>,
    create: bool,
    create_new: bool,
    truncate: bool,
    resolve: Resolve,
    share: Option<ShareMode>,
    backup: bool,
    mode: Option<VfsMode>,
    sec_desc: Option<SecDesc>,
    attrs: AttrFlags,
}

impl OpenSpec {
    /// Resolves access and disposition, applying the defaults: read access
    /// when no access is asked for at all, opening an existing file, and
    /// creating a missing one only for an exclusive create.
    fn granular(
        read: Option<bool>,
        write: Option<Write>,
        access: Option<u32>,
        exists: Option<Exists>,
        missing: Option<Missing>,
    ) -> std::result::Result<Self, &'static str> {
        let (read, write, append) = if read.is_none() && write.is_none() && access.is_none() {
            (true, false, false)
        } else {
            let write = write.unwrap_or(Write::No);
            (
                read.unwrap_or(false),
                write != Write::No,
                write == Write::Append,
            )
        };
        let exists = exists.unwrap_or(Exists::Open);
        let missing = missing.unwrap_or(if exists == Exists::Fail {
            Missing::Create
        } else {
            Missing::Fail
        });
        let (create, create_new, truncate) = match (exists, missing) {
            (Exists::Open, Missing::Create) => (true, false, false),
            (Exists::Open, Missing::Fail) => (false, false, false),
            (Exists::Truncate, Missing::Create) => (true, false, true),
            (Exists::Truncate, Missing::Fail) => (false, false, true),
            (Exists::Fail, Missing::Create) => (false, true, false),
            (Exists::Fail, Missing::Fail) => {
                return Err("`exists: :FAIL:` with `missing: :FAIL:` can never open a file");
            }
        };
        let spec = Self {
            read,
            write,
            append,
            access,
            create,
            create_new,
            truncate,
            ..Self::default()
        };
        let caps = spec.caps();
        if truncate && (!caps.write || caps.append) {
            return Err("`exists: :TRUNCATE:` requires write access without append");
        }
        Ok(spec)
    }

    /// Expands a mode string, or returns `None` if `how` is not one.
    pub(crate) fn how(how: &str) -> Option<Self> {
        let (base, plus) = match how.strip_suffix('+') {
            Some(base) => (base, true),
            None => (how, false),
        };
        let (read, write, exists, missing) = match base {
            "r" => (Some(true), None, None, None),
            "w" => (
                None,
                Some(Write::Yes),
                Some(Exists::Truncate),
                Some(Missing::Create),
            ),
            "a" => (None, Some(Write::Append), None, Some(Missing::Create)),
            "x" => (
                None,
                Some(Write::Yes),
                Some(Exists::Fail),
                Some(Missing::Create),
            ),
            _ => return None,
        };
        let (read, write) = if plus {
            (Some(true), Some(write.unwrap_or(Write::Yes)))
        } else {
            (read, write)
        };
        Some(Self::granular(read, write, None, exists, missing).expect("mode strings are valid"))
    }

    /// Resolves the granular keywords, given in [`KEY_NAMES`] order. With a
    /// mode string in `how`, none may be given.
    pub(crate) async fn from_args<'v, 's>(
        strand: &mut Strand<'v, 's>,
        global: State<'v, FsGlobal<'v>>,
        how: Option<&str>,
        keys: [Option<Slot<'v, '_>>; OPEN_KEYS],
    ) -> Result<'v, 's, Option<Self>> {
        if let Some(how) = how {
            if let Some(index) = keys.iter().position(Option::is_some) {
                return Err(Error::value(
                    strand,
                    format!(
                        "a mode string cannot be combined with `{}:`",
                        KEY_NAMES[index]
                    ),
                ));
            }
            return Ok(Self::how(how));
        }
        let syms = &global.syms.open;
        let [
            read,
            write,
            access,
            exists,
            missing,
            share,
            backup,
            resolve,
            mode,
            sec_desc,
            attrs @ ..,
        ] = keys;

        let read = read
            .map(|read| util::bool(strand, read, "read"))
            .transpose()?;
        let write = match write {
            None => None,
            Some(write) => Some(match write.as_bool(strand) {
                Some(true) => Write::Yes,
                Some(false) => Write::No,
                None if write.as_sym(strand) == Some(syms.append) => Write::Append,
                None => {
                    return Err(Error::type_error(
                        strand,
                        "write: expected Bool or :APPEND:",
                    ));
                }
            }),
        };
        let access = match access {
            None => None,
            Some(access) => Some(
                global
                    .types
                    .file_access_mask
                    .coerce(strand, &access)
                    .await?
                    .0
                    .bits(),
            ),
        };
        let exists = match exists {
            None => None,
            Some(exists) => Some(
                match exists.as_sym(strand) {
                    Some(sym) if sym == syms.open => Some(Exists::Open),
                    Some(sym) if sym == syms.truncate => Some(Exists::Truncate),
                    Some(sym) if sym == syms.fail => Some(Exists::Fail),
                    _ => None,
                }
                .ok_or_else(|| {
                    Error::value(strand, "exists: expected :OPEN:, :TRUNCATE: or :FAIL:")
                })?,
            ),
        };
        let missing = match missing {
            None => None,
            Some(missing) => Some(
                match missing.as_sym(strand) {
                    Some(sym) if sym == syms.create => Some(Missing::Create),
                    Some(sym) if sym == syms.fail => Some(Missing::Fail),
                    _ => None,
                }
                .ok_or_else(|| Error::value(strand, "missing: expected :CREATE: or :FAIL:"))?,
            ),
        };
        let mut spec = Self::granular(read, write, access, exists, missing)
            .map_err(|message| Error::value(strand, message))?;

        if let Some(share) = share {
            spec.share = Some(share_mode(strand, syms, &share).await?);
        }
        if let Some(backup) = backup {
            spec.backup = util::bool(strand, backup, "backup")?;
        }
        spec.resolve = super::resolve_sym(strand, global, resolve, Resolve::Target)?;
        if let Some(mode) = mode {
            spec.mode = Some(super::parse_mode(strand, global, mode)?);
        }
        if let Some(sec_desc) = sec_desc {
            spec.sec_desc = Some(crate::sec_desc_from_value(strand, &sec_desc, "sec_desc").await?);
        }
        for (attr, value) in ATTRS.into_iter().zip(attrs) {
            if let Some(value) = value
                && util::bool(strand, value, "attribute")?
            {
                spec.attrs = spec.attrs.union(attr);
            }
        }
        Ok(Some(spec))
    }

    /// Configures `opts` to open as asked.
    pub(crate) fn apply(&self, opts: &mut OpenOptions<'_>) {
        opts.read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate)
            .resolve(self.resolve)
            .backup(self.backup)
            .attrs(self.attrs);
        if let Some(access) = self.access {
            opts.access(access);
        }
        if let Some(share) = self.share {
            opts.share(share);
        }
        if let Some(mode) = self.mode {
            opts.mode(mode);
        }
        if let Some(sec_desc) = &self.sec_desc {
            opts.sec_desc(sec_desc.clone());
        }
    }

    /// What the opened file can do.
    pub(crate) fn caps(&self) -> OpenCaps {
        OpenCaps::new(self.read, self.write, self.append, self.access)
    }
}

/// The byte I/O capabilities of a file that can do what `caps` says.
///
/// An append-only file is not seekable as a handle: its writes land at the
/// end whatever the position. A seekable one also forks.
pub(crate) fn io_caps(caps: OpenCaps) -> IoCaps {
    let mut io = IoCaps::empty();
    if caps.read {
        io |= IoCaps::READ;
    }
    if caps.write {
        io |= IoCaps::WRITE;
    }
    if (caps.read || caps.write) && !caps.append {
        io |= IoCaps::SEEK | IoCaps::FORK;
    }
    io
}

/// Parses `share:`: one share symbol or an iterable of them.
async fn share_mode<'v, 's>(
    strand: &mut Strand<'v, 's>,
    syms: &OpenSyms<'v>,
    value: &Value<'v>,
) -> Result<'v, 's, ShareMode> {
    let one = |strand: &mut Strand<'v, 's>, value: &Value<'v>| {
        let mode = match value.as_sym(strand) {
            Some(sym) if sym == syms.share_read => Some(ShareMode::READ),
            Some(sym) if sym == syms.share_write => Some(ShareMode::WRITE),
            Some(sym) if sym == syms.share_delete => Some(ShareMode::DELETE),
            _ => None,
        };
        mode.ok_or_else(|| {
            Error::value(
                strand,
                "share: expected :READ:, :WRITE: or :DELETE:, or an iterable of them",
            )
        })
    };
    if value.as_sym(strand).is_some() {
        return one(strand, value);
    }
    strand
        .with_slots(async |strand, [mut iter, mut item]| {
            value.iter(strand, &mut iter).await?;
            let mut share = ShareMode::empty();
            while iter.next(strand, &mut item).await? {
                share |= one(strand, &item)?;
            }
            Ok(share)
        })
        .await
}
