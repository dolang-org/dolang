use std::{
    hash::{Hash, Hasher},
    ops::{BitAnd, BitOr, BitXor, Not},
};

use dolang::runtime::value::fmt::Format;
use dolang::{
    compile::Config,
    runtime::{
        AllocExt, Arg, Args, Error, Instance, Object, Output, Result, Slot, State, Strand, Sym,
        Type, Value,
        object::{
            ArrayLike, ArrayView, FlagLike, Flags, FlagsInstanceExt, FlagsTypeExt, Mut, Ref,
            TypeBuilder, fmt,
        },
        strand::LocalKey,
        unpack,
        value::{AsTuple, Dict, Nil},
        vm::Register,
    },
};

use dolang_vfs::{
    security::{
        Acl as VfsAnyAcl, AclKind as VfsAclKind, MacosAce as VfsMacosAce,
        MacosAceFlags as VfsMacosAceFlags, MacosAceMask as VfsMacosAceMask,
        MacosAceType as VfsMacosAceType, MacosAcl as VfsMacosAcl, Nfs4Ace as VfsNfs4Ace,
        Nfs4AceFlags as VfsNfs4AceFlags, Nfs4AceMask as VfsNfs4AceMask,
        Nfs4AceQualifier as VfsNfs4AceQualifier, Nfs4AceType as VfsNfs4AceType,
        Nfs4Acl as VfsNfs4Acl, Permission as VfsPermission, PosixAce as VfsPosixAce,
        PosixAcl as VfsPosixAcl, PosixAclQualifier as VfsPosixAclQualifier,
        PrincipalId as VfsPrincipalId, PrincipalIdKind as VfsPrincipalIdKind, SecurityInfo,
        SidName as VfsSidName, SidNameUse, TokenGroup as VfsTokenGroup, UnixSecurityInfo,
        WindowsTokenInfo,
    },
    target::{OperatingSystem, OperatingSystemFamily},
};

use dolang_winterop::{
    guid::Guid,
    security::{
        AccessMask as WinAccessMask, Ace as VfsAce, AceBuf as VfsAceBuf, AceBuildOptions,
        AceFlags as WinAceFlags, AceType as VfsAceType, Acl as VfsAcl, AclBuf as VfsAclBuf,
        AclRevision, SecDesc as VfsSecDesc, SecDescControl as WinSecDescControl,
        SecDescUpdate as VfsSecDescUpdate, SecInfo as WinSecInfo, Sid as VfsSid,
        SidIdentifierAuthority, TokenGroupAttributes as WinTokenGroupAttributes, WellKnownSid,
    },
};

use crate::{
    error,
    global::{MacosSecurityGlobal, Nfs4SecurityGlobal, UnixSecurityGlobal, WindowsSecurityGlobal},
    local::Local,
    util,
};

pub(crate) fn configure_compiler<'a>(_config: &mut Config<'a>) {}

macro_rules! flags_ops {
    ($name:ident) => {
        impl BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }
        impl BitAnd for $name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }
        impl BitXor for $name {
            type Output = Self;
            fn bitxor(self, rhs: Self) -> Self {
                Self(self.0 ^ rhs.0)
            }
        }
        impl Not for $name {
            type Output = Self;
            fn not(self) -> Self {
                Self(!self.0)
            }
        }
    };
}

pub(crate) mod macos;
pub(crate) mod nfs4;
pub(crate) mod unix;
pub(crate) mod windows;

pub(crate) use macos::{
    MacosAceFlags, MacosAceMask, MacosAceObject, MacosAclObject, create_macos_acl,
};
pub(crate) use nfs4::{Nfs4AceFlags, Nfs4AceMask, Nfs4AceObject, Nfs4AclObject, create_nfs4_acl};
pub(crate) use unix::{
    Identity, Permission, PosixAceObject, PosixAclObject, create_identity, create_posix_acl,
};
pub use windows::AccessMask;
pub(crate) use windows::{
    Ace, AceFlags, Acl, SecDesc, SecDescControl, SecInfo, Sid, SidName, TokenGroup,
    TokenGroupAttributes, TokenInfo, WellKnownSids, as_windows_sid, create_sec_desc, create_sid,
    create_sid_name, create_token_info, sec_desc_from_args, sec_desc_from_value, windows_sid,
};

/// A position inside a declarative specification.
#[derive(Clone, Copy)]
pub(crate) struct SpecPath<'p> {
    parent: Option<&'p SpecPath<'p>>,
    step: SpecStep<'p>,
}

#[derive(Clone, Copy)]
enum SpecStep<'p> {
    Root(&'p str),
    Key(&'p str),
    Index(usize),
}

impl<'p> SpecPath<'p> {
    pub(crate) fn root(name: &'p str) -> Self {
        Self {
            parent: None,
            step: SpecStep::Root(name),
        }
    }

    pub(crate) fn key(&'p self, name: &'p str) -> Self {
        Self {
            parent: Some(self),
            step: SpecStep::Key(name),
        }
    }

    pub(crate) fn index(&'p self, index: usize) -> Self {
        Self {
            parent: Some(self),
            step: SpecStep::Index(index),
        }
    }
}

impl std::fmt::Display for SpecPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(parent) = self.parent {
            write!(f, "{parent}")?;
        }
        match self.step {
            SpecStep::Root(name) => write!(f, "{name}"),
            SpecStep::Key(name) => write!(f, ".{name}"),
            SpecStep::Index(index) => write!(f, "[{index}]"),
        }
    }
}

/// Converts a portable [`dolang_vfs::security::Acl`] into the appropriate
/// Do-facing ACL value (`security.unix.Acl`, `security.nfs4.Acl`, or
/// `security.macos.Acl`).
pub(crate) fn create_any_acl<'v>(
    strand: &mut Strand<'v, '_>,
    acl: Option<VfsAnyAcl>,
    out: &mut Slot<'v, '_>,
) {
    match acl {
        None => Output::set(strand, out, Nil),
        Some(VfsAnyAcl::Posix(acl)) => {
            let global = strand.force_state::<UnixSecurityGlobal<'v>>();
            create_posix_acl(strand, global, Some(acl), out)
        }
        Some(VfsAnyAcl::Nfs4(acl)) => {
            let global = strand.force_state::<Nfs4SecurityGlobal<'v>>();
            create_nfs4_acl(strand, global, Some(acl), out)
        }
        Some(VfsAnyAcl::Macos(acl)) => {
            let global = strand.force_state::<MacosSecurityGlobal<'v>>();
            create_macos_acl(strand, global, Some(acl), out)
        }
        Some(_) => Output::set(strand, out, Nil),
    }
}

/// Infers the ACL kind from `value`'s dynamic type: a `security.unix.Acl`
/// yields [`VfsAclKind::Posix`], a `security.nfs4.Acl` yields
/// [`VfsAclKind::Nfs4`], a `security.macos.Acl` yields [`VfsAclKind::Macos`].
/// Other values yield `None`.
fn built_acl_from_value<'v>(strand: &mut Strand<'v, '_>, value: &Value<'v>) -> Option<VfsAnyAcl> {
    if value.is_nil() {
        return None;
    }
    if let Some(acl) = strand
        .try_state::<UnixSecurityGlobal<'v>>()
        .and_then(|global| global.types.posix_acl.cast(value))
    {
        return Some(VfsAnyAcl::Posix(
            acl.enter_sync(strand, |_strand, acl| acl.annex().clone()),
        ));
    }
    if let Some(acl) = strand
        .try_state::<Nfs4SecurityGlobal<'v>>()
        .and_then(|global| global.types.nfs4_acl.cast(value))
    {
        return Some(VfsAnyAcl::Nfs4(
            acl.enter_sync(strand, |_strand, acl| acl.annex().clone()),
        ));
    }
    if let Some(acl) = strand
        .try_state::<MacosSecurityGlobal<'v>>()
        .and_then(|global| global.types.macos_acl.cast(value))
    {
        return Some(VfsAnyAcl::Macos(
            acl.enter_sync(strand, |_strand, acl| acl.annex().clone()),
        ));
    }
    None
}

/// Resolves the ACL and kind accepted by the three filesystem setter APIs.
pub(crate) async fn resolve_acl_input<'v, 's>(
    strand: &mut Strand<'v, 's>,
    value: &Value<'v>,
    kind: Option<Slot<'v, '_>>,
    path: &SpecPath<'_>,
) -> Result<'v, 's, (VfsAclKind, Option<VfsAnyAcl>)> {
    let explicit_kind = match kind {
        Some(kind) => Some(acl_kind_sym(strand, Some(kind))?),
        None => None,
    };

    if value.is_nil() {
        return Ok((explicit_kind.unwrap_or(VfsAclKind::Posix), None));
    }

    if let Some(acl) = built_acl_from_value(strand, value) {
        let actual = acl.kind();
        if let Some(expected) = explicit_kind
            && expected != actual
        {
            return Err(Error::value(
                strand,
                format!("{path}: ACL kind does not match kind:"),
            ));
        }
        return Ok((actual, Some(acl)));
    }

    let kind = explicit_kind.ok_or_else(|| {
        Error::type_error(
            strand,
            format!("{path}: kind: is required for an ACL specification"),
        )
    })?;
    let acl = match kind {
        VfsAclKind::Posix => {
            let global = strand.force_state::<UnixSecurityGlobal<'v>>();
            VfsAnyAcl::Posix(unix::coerce_acl(strand, global, value, path).await?)
        }
        VfsAclKind::Nfs4 => {
            let global = strand.force_state::<Nfs4SecurityGlobal<'v>>();
            VfsAnyAcl::Nfs4(nfs4::coerce_acl(strand, global, value, path).await?)
        }
        VfsAclKind::Macos => {
            let global = strand.force_state::<MacosSecurityGlobal<'v>>();
            VfsAnyAcl::Macos(macos::coerce_acl(strand, global, value, path).await?)
        }
        _ => return Err(Error::not_supported(strand)),
    };
    Ok((kind, Some(acl)))
}

/// Parses the `kind:` keyword argument (`:POSIX:`/`:NFS4:`/`:MACOS:`),
/// defaulting to [`VfsAclKind::Posix`] when absent.
pub(crate) fn acl_kind_sym<'v, 's>(
    strand: &mut Strand<'v, 's>,
    slot: Option<Slot<'v, '_>>,
) -> Result<'v, 's, VfsAclKind> {
    let Some(slot) = slot else {
        return Ok(VfsAclKind::Posix);
    };
    let sym = slot
        .as_sym(strand)
        .ok_or_else(|| Error::type_error(strand, "kind: expected :POSIX:, :NFS4:, or :MACOS:"))?;
    match sym.as_str(strand.vm()) {
        "POSIX" => Ok(VfsAclKind::Posix),
        "NFS4" => Ok(VfsAclKind::Nfs4),
        "MACOS" => Ok(VfsAclKind::Macos),
        _ => Err(Error::value(
            strand,
            "kind: expected :POSIX:, :NFS4:, or :MACOS:",
        )),
    }
}

fn security_info<'v, 's>(
    strand: &mut Strand<'v, 's>,
    local: LocalKey<'v, Local>,
) -> Result<'v, 's, SecurityInfo> {
    Ok(local.get(strand).security())
}

pub(crate) fn configure_vm<'v>(builder: &mut Register<'v>, local: LocalKey<'v, Local>) {
    builder
        .module("security")
        .function("user_name", async move |strand, args, out| {
            let ([], []) = unpack!(strand, args, 0, 0)?;
            let family = local.get(strand).target().os().family();
            let vfs = local.get(strand).vfs();
            let name = match family {
                OperatingSystemFamily::Unix => {
                    let security = security_info(strand, local)?;
                    let Some(info) = security.unix() else {
                        unreachable!("Unix target returned Windows security information")
                    };
                    error::io_result(strand, vfs.user_name(info.uid()).await)?
                }
                OperatingSystemFamily::Windows => {
                    let security = security_info(strand, local)?;
                    let Some(info) = security.windows() else {
                        unreachable!("Windows target returned Unix security information")
                    };
                    error::io_result(strand, vfs.sid_name(info.user_sid()).await)?
                        .name()
                        .to_owned()
                }
            };
            Output::set(strand, out, name.as_str());
            Ok(())
        })
        .commit();
}
