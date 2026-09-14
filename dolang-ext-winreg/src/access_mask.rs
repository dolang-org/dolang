//! `winreg.AccessMask` — registry key access rights: the generic Windows
//! object-security bits plus the registry-specific composites
//! (`READ`/`WRITE`/`READ_WRITE`, i.e. `KEY_READ`/`KEY_WRITE`).

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::object::{FlagLike, Flags, FlagsInstanceExt, FlagsTypeExt, TypeBuilder};
use dolang::runtime::{Error, Output, unpack};
use dolang_vfs_winreg::Access as WireAccess;
use dolang_winterop::security::AccessMask as WinAccessMask;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AccessMask(pub(crate) WireAccess);

impl AccessMask {
    pub(crate) const QUERY_VALUE: AccessMask = AccessMask(WireAccess::QUERY_VALUE);
    pub(crate) const SET_VALUE: AccessMask = AccessMask(WireAccess::SET_VALUE);
    pub(crate) const CREATE_SUB_KEY: AccessMask = AccessMask(WireAccess::CREATE_SUB_KEY);
    pub(crate) const ENUMERATE_SUB_KEYS: AccessMask = AccessMask(WireAccess::ENUMERATE_SUB_KEYS);
    pub(crate) const NOTIFY: AccessMask = AccessMask(WireAccess::NOTIFY);
    pub(crate) const CREATE_LINK: AccessMask = AccessMask(WireAccess::CREATE_LINK);
    pub(crate) const WOW64_64KEY: AccessMask = AccessMask(WireAccess::WOW64_64KEY);
    pub(crate) const WOW64_32KEY: AccessMask = AccessMask(WireAccess::WOW64_32KEY);
    pub(crate) const READ: AccessMask = AccessMask(WireAccess::READ);
    pub(crate) const WRITE: AccessMask = AccessMask(WireAccess::WRITE);
    pub(crate) const READ_WRITE: AccessMask = AccessMask(WireAccess::READ_WRITE);
    pub(crate) const DELETE: AccessMask = AccessMask(WireAccess(WinAccessMask::DELETE));
    pub(crate) const READ_CONTROL: AccessMask = AccessMask(WireAccess(WinAccessMask::READ_CONTROL));
    pub(crate) const WRITE_DAC: AccessMask = AccessMask(WireAccess(WinAccessMask::WRITE_DAC));
    pub(crate) const WRITE_OWNER: AccessMask = AccessMask(WireAccess(WinAccessMask::WRITE_OWNER));
    pub(crate) const SYNCHRONIZE: AccessMask = AccessMask(WireAccess(WinAccessMask::SYNCHRONIZE));
    pub(crate) const STANDARD_RIGHTS_REQUIRED: AccessMask =
        AccessMask(WireAccess(WinAccessMask::STANDARD_RIGHTS_REQUIRED));
    pub(crate) const STANDARD_RIGHTS_ALL: AccessMask =
        AccessMask(WireAccess(WinAccessMask::STANDARD_RIGHTS_ALL));
    pub(crate) const ACCESS_SYSTEM_SECURITY: AccessMask =
        AccessMask(WireAccess(WinAccessMask::ACCESS_SYSTEM_SECURITY));
    pub(crate) const MAXIMUM_ALLOWED: AccessMask =
        AccessMask(WireAccess(WinAccessMask::MAXIMUM_ALLOWED));
    pub(crate) const GENERIC_READ: AccessMask = AccessMask(WireAccess(WinAccessMask::GENERIC_READ));
    pub(crate) const GENERIC_WRITE: AccessMask =
        AccessMask(WireAccess(WinAccessMask::GENERIC_WRITE));
    pub(crate) const GENERIC_EXECUTE: AccessMask =
        AccessMask(WireAccess(WinAccessMask::GENERIC_EXECUTE));
    pub(crate) const GENERIC_ALL: AccessMask = AccessMask(WireAccess(WinAccessMask::GENERIC_ALL));
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
        AccessMask(WireAccess(self.0.0 & rhs.0.0))
    }
}

impl BitXor for AccessMask {
    type Output = AccessMask;
    fn bitxor(self, rhs: AccessMask) -> AccessMask {
        AccessMask(WireAccess(self.0.0 ^ rhs.0.0))
    }
}

impl Not for AccessMask {
    type Output = AccessMask;
    fn not(self) -> AccessMask {
        AccessMask(WireAccess(!self.0.0))
    }
}

impl FlagLike for AccessMask {
    const ZERO: AccessMask = AccessMask(WireAccess(WinAccessMask::empty()));
    const MODULE: &'static str = "winreg";
    const NAME: &'static str = "AccessMask";
    const BITS: &'static [(&'static str, AccessMask)] = &[
        ("QUERY_VALUE", AccessMask::QUERY_VALUE),
        ("SET_VALUE", AccessMask::SET_VALUE),
        ("CREATE_SUB_KEY", AccessMask::CREATE_SUB_KEY),
        ("ENUMERATE_SUB_KEYS", AccessMask::ENUMERATE_SUB_KEYS),
        ("NOTIFY", AccessMask::NOTIFY),
        ("CREATE_LINK", AccessMask::CREATE_LINK),
        ("WOW64_64KEY", AccessMask::WOW64_64KEY),
        ("WOW64_32KEY", AccessMask::WOW64_32KEY),
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

    fn rank(self) -> usize {
        self.0.0.bits().count_ones() as usize
    }

    fn build<'v, 'a>(
        mut builder: TypeBuilder<'v, 'a, Flags<Self>>,
    ) -> TypeBuilder<'v, 'a, Flags<Self>> {
        // `security.windows.AccessMask` is a nominal supertype so these
        // rights can be used wherever a Windows access mask is expected;
        // that contract is what the `int` projection below satisfies.
        let base = dolang_ext_shell::windows_access_mask_type(&mut builder);
        builder
            .nominal_supertype(base)
            .get("int", |this, strand, out| {
                Output::set(strand, out, this.flags().0.0.bits());
                Ok(())
            })
            .type_method("from_int", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let value = value.to_i64(strand)?;
                let value = u32::try_from(value)
                    .map_err(|_| Error::value(strand, "flags integer out of range"))?;
                this.create_flags(
                    strand,
                    Self(WireAccess(WinAccessMask::from_bits_retain(value))),
                    out,
                );
                Ok(())
            })
    }
}

impl From<AccessMask> for WireAccess {
    fn from(mask: AccessMask) -> WireAccess {
        mask.0
    }
}
