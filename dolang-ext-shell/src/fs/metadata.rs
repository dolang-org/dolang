use super::{FS_BASE, FS_UNIX, FS_WINDOWS};

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::{
    AllocExt, Error, Object, Output, Result, State, Strand, Sym,
    object::{FlagLike, FlagsInstanceExt, FlagsTypeExt, Instance, TypeBuilder},
    unpack,
    value::Value,
};
use dolang_vfs::metadata::{FileType, Metadata as VfsMetadata, Mode as VfsMode};

use crate::{
    fs::attrs,
    global::{FsGlobal, UnixSecurityTag, WindowsSecurityTag},
    security::Permission,
    util,
};

const NANOS_PER_SEC_I128: i128 = 1_000_000_000;

fn mode_u32<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    name: &'static str,
) -> Result<'v, 's, u32> {
    let value = value
        .to_i64(strand)
        .map_err(|_| Error::type_error(strand, format!("{name}: expected Int")))?;
    u32::try_from(value).map_err(|_| Error::value(strand, format!("{name}: out of range")))
}

/// Unix mode bits (`fs.unix.Mode`), a local newtype over
/// [`dolang_vfs::metadata::Mode`] so [`FlagLike`] can be implemented here
/// (both the trait and the VFS type are foreign to this crate).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Mode(pub(crate) VfsMode);

impl BitOr for Mode {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitAnd for Mode {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
impl BitXor for Mode {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self {
        Self(self.0 ^ rhs.0)
    }
}
impl Not for Mode {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl FlagLike for Mode {
    const ZERO: Self = Self(VfsMode::empty());
    const MODULE: &'static str = "fs.unix";
    const NAME: &'static str = "Mode";
    const BITS: &'static [(&'static str, Self)] = &[
        ("SET_UID", Self(VfsMode::SET_UID)),
        ("SET_GID", Self(VfsMode::SET_GID)),
        ("STICKY", Self(VfsMode::STICKY)),
        ("OWNER_READ", Self(VfsMode::OWNER_READ)),
        ("OWNER_WRITE", Self(VfsMode::OWNER_WRITE)),
        ("OWNER_EXECUTE", Self(VfsMode::OWNER_EXECUTE)),
        ("GROUP_READ", Self(VfsMode::GROUP_READ)),
        ("GROUP_WRITE", Self(VfsMode::GROUP_WRITE)),
        ("GROUP_EXECUTE", Self(VfsMode::GROUP_EXECUTE)),
        ("OTHER_READ", Self(VfsMode::OTHER_READ)),
        ("OTHER_WRITE", Self(VfsMode::OTHER_WRITE)),
        ("OTHER_EXECUTE", Self(VfsMode::OTHER_EXECUTE)),
        ("IFIFO", Self(VfsMode::IFIFO)),
        ("IFCHR", Self(VfsMode::IFCHR)),
        ("IFDIR", Self(VfsMode::IFDIR)),
        ("IFBLK", Self(VfsMode::IFBLK)),
        ("IFREG", Self(VfsMode::IFREG)),
        ("IFLNK", Self(VfsMode::IFLNK)),
        ("IFSOCK", Self(VfsMode::IFSOCK)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    fn build<'v, 'a>(
        builder: TypeBuilder<'v, 'a, dolang::runtime::object::Flags<Self>>,
    ) -> TypeBuilder<'v, 'a, dolang::runtime::object::Flags<Self>> {
        builder
            .get("owner", |this, strand, out| {
                let permission = strand.force_state::<UnixSecurityTag>().types.permission;
                permission.create_flags(strand, Permission(this.flags().0.owner()), out);
                Ok(())
            })
            .get("group", |this, strand, out| {
                let permission = strand.force_state::<UnixSecurityTag>().types.permission;
                permission.create_flags(strand, Permission(this.flags().0.group()), out);
                Ok(())
            })
            .get("other", |this, strand, out| {
                let permission = strand.force_state::<UnixSecurityTag>().types.permission;
                permission.create_flags(strand, Permission(this.flags().0.other()), out);
                Ok(())
            })
            .get("int", |this, strand, out| {
                Output::set(strand, out, this.flags().0.bits());
                Ok(())
            })
            .type_method("from_int", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let value = mode_u32(strand, &value, "value")?;
                this.create_flags(strand, Self(VfsMode::from_bits_retain(value)), out);
                Ok(())
            })
    }
}

pub(crate) struct Metadata<const FAMILY: u8 = FS_BASE>;

pub(crate) struct MetadataAnnex<'v> {
    pub(crate) global: State<'v, FsGlobal<'v>>,
    pub(crate) inner: VfsMetadata,
}

pub(crate) fn file_type_to_sym<'v>(
    file_type: FileType,
    global: State<'v, FsGlobal<'v>>,
) -> Sym<'v, 'v> {
    match file_type {
        FileType::File => global.syms.file,
        FileType::Dir => global.syms.dir,
        FileType::Symlink => global.syms.symlink,
        FileType::Fifo => global.syms.fifo,
        FileType::CharacterDevice => global.syms.char_device,
        FileType::BlockDevice => global.syms.block_device,
        FileType::Socket => global.syms.socket,
        FileType::Unknown => global.syms.unknown,
    }
}

fn timestamp_nanos(secs: i64, nanos: i64) -> i128 {
    i128::from(secs) * NANOS_PER_SEC_I128 + i128::from(nanos)
}

fn write_timestamp<'v, 's>(
    strand: &mut Strand<'v, 's>,
    secs: i64,
    nanos: i64,
    out: impl Output<'v>,
) -> Result<'v, 's, ()> {
    dolang_ext_time::create_datetime(strand, timestamp_nanos(secs, nanos), out)
}

fn attr_field<'v, 's, const FAMILY: u8>(
    strand: &mut Strand<'v, 's>,
    this: Instance<'v, '_, Metadata<FAMILY>>,
    out: impl Output<'v>,
    windows: u32,
    linux: u32,
    macos: u32,
    freebsd: u32,
) -> Result<'v, 's, ()> {
    match attrs::flag(&this.annex().inner, windows, linux, macos, freebsd) {
        attrs::Flag::Inapplicable | attrs::Flag::Unavailable => {
            Output::set(strand, out, dolang::runtime::value::Nil);
            Ok(())
        }
        attrs::Flag::Value(value) => {
            Output::set(strand, out, value);
            Ok(())
        }
    }
}

pub(crate) fn create_metadata<'v>(
    strand: &mut Strand<'v, '_>,
    global: State<'v, FsGlobal<'v>>,
    metadata: VfsMetadata,
    out: impl Output<'v>,
) {
    let unix = metadata.unix().is_some();
    let annex = MetadataAnnex {
        global,
        inner: metadata,
    };
    if unix {
        global
            .types
            .unix_metadata
            .create_with_annex(strand, Metadata::<FS_UNIX>, annex, out);
    } else {
        global
            .types
            .windows_metadata
            .create_with_annex(strand, Metadata::<FS_WINDOWS>, annex, out);
    }
}

impl<'v, const FAMILY: u8> Object<'v> for Metadata<FAMILY> {
    const NAME: &'v str = "Metadata";
    const MODULE: &'v str = match FAMILY {
        FS_UNIX => "fs.unix",
        FS_WINDOWS => "fs.windows",
        _ => "fs",
    };
    type Annex = MetadataAnnex<'v>;
    type Type = ();
    type TypeAnnex = ();

    fn build<'a>(mut builder: TypeBuilder<'v, 'a, Self>) -> TypeBuilder<'v, 'a, Self> {
        let mode = builder.sym("mode");
        let dev = builder.sym("dev");
        let ino = builder.sym("ino");
        let nlink = builder.sym("nlink");
        let uid = builder.sym("uid");
        let gid = builder.sym("gid");
        let rdev = builder.sym("rdev");
        let blksize = builder.sym("blksize");
        let blocks = builder.sym("blocks");
        let win_attrs = builder.sym("win_attrs");
        let linux_attrs = builder.sym("linux_attrs");
        let owner = builder.sym("owner");
        let group = builder.sym("group");
        builder = builder
            .get("size", |this, strand, out| {
                Output::set(strand, out, this.annex().inner.len());
                Ok(())
            })
            .get("type", |this, strand, out| {
                Output::set(
                    strand,
                    out,
                    file_type_to_sym(this.annex().inner.file_type(), this.annex().global),
                );
                Ok(())
            })
            .get("modified", |this, strand, out| {
                let annex = this.annex();
                write_timestamp(strand, annex.inner.mtime(), annex.inner.mtime_nsec(), out)
            })
            .get("accessed", |this, strand, out| {
                let annex = this.annex();
                write_timestamp(strand, annex.inner.atime(), annex.inner.atime_nsec(), out)
            })
            .get("created", |this, strand, out| {
                let annex = this.annex();
                write_timestamp(strand, annex.inner.ctime(), annex.inner.ctime_nsec(), out)
            });
        if FAMILY == FS_UNIX {
            builder = builder
                .get("mode", move |this, strand, out| {
                    let annex = this.annex();
                    let Some(unix) = annex.inner.unix() else {
                        return Err(Error::field(strand, mode));
                    };
                    annex
                        .global
                        .types
                        .mode
                        .create_flags(strand, Mode(unix.mode()), out);
                    Ok(())
                })
                .get("dev", move |this, strand, out| {
                    util::option_field(strand, this.annex().inner.unix().map(|v| v.dev()), dev, out)
                })
                .get("ino", move |this, strand, out| {
                    util::option_field(strand, this.annex().inner.unix().map(|v| v.ino()), ino, out)
                })
                .get("nlink", move |this, strand, out| {
                    util::option_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.nlink()),
                        nlink,
                        out,
                    )
                })
                .get("uid", move |this, strand, out| {
                    util::option_field(strand, this.annex().inner.unix().map(|v| v.uid()), uid, out)
                })
                .get("gid", move |this, strand, out| {
                    util::option_field(strand, this.annex().inner.unix().map(|v| v.gid()), gid, out)
                })
                .get("rdev", move |this, strand, out| {
                    util::option_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.rdev()),
                        rdev,
                        out,
                    )
                })
                .get("blksize", move |this, strand, out| {
                    util::option_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.block_size()),
                        blksize,
                        out,
                    )
                })
                .get("blocks", move |this, strand, out| {
                    util::option_field(
                        strand,
                        this.annex().inner.unix().map(|v| v.blocks()),
                        blocks,
                        out,
                    )
                });
        }
        if FAMILY == FS_WINDOWS {
            builder = builder.get("win_attrs", move |this, strand, out| {
                util::option_field(strand, this.annex().inner.win_attrs(), win_attrs, out)
            });
        }
        if FAMILY == FS_UNIX {
            builder = builder.get("freebsd_attrs", move |this, strand, out| {
                util::nullable_field(strand, this.annex().inner.freebsd_attrs(), out)
            });
        }
        // Ownership matches the representation accepted by `update_metadata`:
        // a SID on Windows, or a uid/gid on Unix.
        builder = builder
            .get("owner", move |this, strand, mut out| {
                let annex = this.annex();
                if let Some(windows) = annex.inner.windows() {
                    let Some(value) = windows.user().cloned() else {
                        Output::set(strand, out, dolang::runtime::value::Nil);
                        return Ok(());
                    };
                    let windows = strand.force_state::<WindowsSecurityTag>();
                    crate::security::create_sid(strand, windows, value, &mut out);
                    return Ok(());
                }
                util::option_field(strand, annex.inner.unix().map(|v| v.uid()), owner, out)
            })
            .get("group", move |this, strand, mut out| {
                let annex = this.annex();
                if let Some(windows) = annex.inner.windows() {
                    let Some(value) = windows.group().cloned() else {
                        Output::set(strand, out, dolang::runtime::value::Nil);
                        return Ok(());
                    };
                    let windows = strand.force_state::<WindowsSecurityTag>();
                    crate::security::create_sid(strand, windows, value, &mut out);
                    return Ok(());
                }
                util::option_field(strand, annex.inner.unix().map(|v| v.gid()), group, out)
            });
        if FAMILY == FS_UNIX {
            builder = builder
                .get("linux_attrs", move |this, strand, out| {
                    let annex = this.annex();
                    let Some(unix) = annex.inner.unix() else {
                        return Err(Error::field(strand, linux_attrs));
                    };
                    util::nullable_field(strand, unix.linux_attrs(), out)
                })
                .get("macos_attrs", move |this, strand, out| {
                    util::nullable_field(strand, this.annex().inner.macos_attrs(), out)
                });
        }
        if FAMILY != FS_BASE {
            builder = builder
                .get("readonly", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        attrs::windows::READONLY,
                        0,
                        0,
                        attrs::freebsd::READONLY,
                    )
                })
                .get("hidden", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        attrs::windows::HIDDEN,
                        0,
                        attrs::macos::HIDDEN,
                        attrs::freebsd::HIDDEN,
                    )
                })
                .get("system", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        attrs::windows::SYSTEM,
                        0,
                        0,
                        attrs::freebsd::SYSTEM,
                    )
                })
                .get("archive", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        attrs::windows::ARCHIVE,
                        0,
                        0,
                        attrs::freebsd::ARCHIVE,
                    )
                })
                .get("reparse_point", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        attrs::windows::REPARSE_POINT,
                        0,
                        0,
                        attrs::freebsd::REPARSE_POINT,
                    )
                });
        }
        builder = builder.get("compressed", move |this, strand, out| {
            attr_field(
                strand,
                this,
                out,
                attrs::windows::COMPRESSED,
                attrs::linux::COMPRESSED,
                attrs::macos::COMPRESSED,
                attrs::freebsd::COMPRESSED,
            )
        });
        if FAMILY == FS_WINDOWS {
            builder = builder
                .get("sparse", move |this, strand, out| {
                    attr_field(strand, this, out, attrs::windows::SPARSE, 0, 0, 0)
                })
                .get("encrypted", move |this, strand, out| {
                    attr_field(strand, this, out, attrs::windows::ENCRYPTED, 0, 0, 0)
                })
                .get("temporary", move |this, strand, out| {
                    attr_field(strand, this, out, attrs::windows::TEMPORARY, 0, 0, 0)
                });
        }
        if FAMILY != FS_BASE {
            builder = builder.get("offline", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    attrs::windows::OFFLINE,
                    0,
                    0,
                    attrs::freebsd::OFFLINE,
                )
            });
        }
        if FAMILY == FS_WINDOWS {
            builder = builder.get("not_content_indexed", move |this, strand, out| {
                attr_field(
                    strand,
                    this,
                    out,
                    attrs::windows::NOT_CONTENT_INDEXED,
                    0,
                    0,
                    0,
                )
            });
        }
        if FAMILY == FS_UNIX {
            builder = builder
                .get("immutable", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        0,
                        attrs::linux::IMMUTABLE,
                        attrs::macos::IMMUTABLE,
                        attrs::freebsd::IMMUTABLE,
                    )
                })
                .get("append_only", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        0,
                        attrs::linux::APPEND_ONLY,
                        attrs::macos::APPEND_ONLY,
                        attrs::freebsd::APPEND_ONLY,
                    )
                })
                .get("no_dump", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        0,
                        attrs::linux::NO_DUMP,
                        attrs::macos::NO_DUMP,
                        attrs::freebsd::NO_DUMP,
                    )
                })
                .get("no_atime", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::NO_ATIME, 0, 0)
                })
                .get("no_copy_on_write", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::NO_COPY_ON_WRITE, 0, 0)
                })
                .get("dir_sync", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::DIR_SYNC, 0, 0)
                })
                .get("casefold", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::CASEFOLD, 0, 0)
                })
                .get("data_journaling", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::DATA_JOURNALING, 0, 0)
                })
                .get("no_compress", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::NO_COMPRESS, 0, 0)
                })
                .get("project_inherit", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::PROJECT_INHERIT, 0, 0)
                })
                .get("secure_delete", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::SECURE_DELETE, 0, 0)
                })
                .get("sync", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::SYNC, 0, 0)
                })
                .get("no_tail_merge", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::NO_TAIL_MERGE, 0, 0)
                })
                .get("top_dir", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::TOP_DIR, 0, 0)
                })
                .get("undelete", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::UNDELETE, 0, 0)
                })
                .get("direct_access", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::DIRECT_ACCESS, 0, 0)
                })
                .get("extent_format", move |this, strand, out| {
                    attr_field(strand, this, out, 0, attrs::linux::EXTENT_FORMAT, 0, 0)
                })
                .get("opaque", move |this, strand, out| {
                    attr_field(
                        strand,
                        this,
                        out,
                        0,
                        0,
                        attrs::macos::OPAQUE,
                        attrs::freebsd::OPAQUE,
                    )
                });
        }
        builder
    }
}
