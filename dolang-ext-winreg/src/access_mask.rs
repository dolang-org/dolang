//! `winreg.AccessMask` — registry key access rights: the generic Windows
//! object-security bits plus the registry-specific composites
//! (`READ`/`WRITE`/`READ_WRITE`, i.e. `KEY_READ`/`KEY_WRITE`).

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::object::FlagLike;
use dolang_vfs_winreg::Access as WireAccess;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AccessMask(pub(crate) u32);

impl AccessMask {
    pub(crate) const READ: AccessMask = AccessMask(WireAccess::READ.0);
    pub(crate) const WRITE: AccessMask = AccessMask(WireAccess::WRITE.0);
    pub(crate) const READ_WRITE: AccessMask = AccessMask(WireAccess::READ_WRITE.0);
    pub(crate) const DELETE: AccessMask = AccessMask(dolang_winterop::AccessMask::DELETE.0);
    pub(crate) const READ_CONTROL: AccessMask =
        AccessMask(dolang_winterop::AccessMask::READ_CONTROL.0);
    pub(crate) const WRITE_DAC: AccessMask = AccessMask(dolang_winterop::AccessMask::WRITE_DAC.0);
    pub(crate) const WRITE_OWNER: AccessMask =
        AccessMask(dolang_winterop::AccessMask::WRITE_OWNER.0);
    pub(crate) const SYNCHRONIZE: AccessMask =
        AccessMask(dolang_winterop::AccessMask::SYNCHRONIZE.0);
    pub(crate) const STANDARD_RIGHTS_REQUIRED: AccessMask =
        AccessMask(dolang_winterop::AccessMask::STANDARD_RIGHTS_REQUIRED.0);
    pub(crate) const STANDARD_RIGHTS_ALL: AccessMask =
        AccessMask(dolang_winterop::AccessMask::STANDARD_RIGHTS_ALL.0);
    pub(crate) const ACCESS_SYSTEM_SECURITY: AccessMask =
        AccessMask(dolang_winterop::AccessMask::ACCESS_SYSTEM_SECURITY.0);
    pub(crate) const MAXIMUM_ALLOWED: AccessMask =
        AccessMask(dolang_winterop::AccessMask::MAXIMUM_ALLOWED.0);
    pub(crate) const GENERIC_READ: AccessMask =
        AccessMask(dolang_winterop::AccessMask::GENERIC_READ.0);
    pub(crate) const GENERIC_WRITE: AccessMask =
        AccessMask(dolang_winterop::AccessMask::GENERIC_WRITE.0);
    pub(crate) const GENERIC_EXECUTE: AccessMask =
        AccessMask(dolang_winterop::AccessMask::GENERIC_EXECUTE.0);
    pub(crate) const GENERIC_ALL: AccessMask =
        AccessMask(dolang_winterop::AccessMask::GENERIC_ALL.0);
}

impl BitOr for AccessMask {
    type Output = AccessMask;
    fn bitor(self, rhs: AccessMask) -> AccessMask {
        AccessMask(self.0 | rhs.0)
    }
}

impl BitAnd for AccessMask {
    type Output = AccessMask;
    fn bitand(self, rhs: AccessMask) -> AccessMask {
        AccessMask(self.0 & rhs.0)
    }
}

impl BitXor for AccessMask {
    type Output = AccessMask;
    fn bitxor(self, rhs: AccessMask) -> AccessMask {
        AccessMask(self.0 ^ rhs.0)
    }
}

impl Not for AccessMask {
    type Output = AccessMask;
    fn not(self) -> AccessMask {
        AccessMask(!self.0)
    }
}

impl FlagLike for AccessMask {
    const ZERO: AccessMask = AccessMask(0);
    const MODULE: &'static str = "winreg";
    const NAME: &'static str = "AccessMask";
    const BITS: &'static [(&'static str, AccessMask)] = &[
        ("READ", AccessMask::READ),
        ("WRITE", AccessMask::WRITE),
        ("READ_WRITE", AccessMask::READ_WRITE),
        ("DELETE", AccessMask::DELETE),
        ("READ_CONTROL", AccessMask::READ_CONTROL),
        ("WRITE_DAC", AccessMask::WRITE_DAC),
        ("WRITE_OWNER", AccessMask::WRITE_OWNER),
        ("SYNCHRONIZE", AccessMask::SYNCHRONIZE),
        (
            "STANDARD_RIGHTS_REQUIRED",
            AccessMask::STANDARD_RIGHTS_REQUIRED,
        ),
        ("STANDARD_RIGHTS_ALL", AccessMask::STANDARD_RIGHTS_ALL),
        ("ACCESS_SYSTEM_SECURITY", AccessMask::ACCESS_SYSTEM_SECURITY),
        ("MAXIMUM_ALLOWED", AccessMask::MAXIMUM_ALLOWED),
        ("GENERIC_READ", AccessMask::GENERIC_READ),
        ("GENERIC_WRITE", AccessMask::GENERIC_WRITE),
        ("GENERIC_EXECUTE", AccessMask::GENERIC_EXECUTE),
        ("GENERIC_ALL", AccessMask::GENERIC_ALL),
    ];
}

impl From<AccessMask> for WireAccess {
    fn from(mask: AccessMask) -> WireAccess {
        WireAccess(mask.0)
    }
}
