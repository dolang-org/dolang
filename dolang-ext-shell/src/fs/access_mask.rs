//! `fs.windows.AccessMask` — file access rights: the file-specific rights
//! plus the generic Windows object-security bits.

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::object::{FlagLike, Flags, FlagsInstanceExt, FlagsTypeExt, TypeBuilder};
use dolang::runtime::{Error, Output, unpack};
use dolang_winterop::security::AccessMask as WinAccessMask;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AccessMask(pub(crate) WinAccessMask);

impl AccessMask {
    const fn bits(bits: u32) -> Self {
        Self(WinAccessMask::from_bits_retain(bits))
    }

    const fn win(mask: WinAccessMask) -> Self {
        Self(mask)
    }
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
        AccessMask(WinAccessMask::from_bits_retain(!self.0.bits()))
    }
}

impl FlagLike for AccessMask {
    const ZERO: AccessMask = AccessMask(WinAccessMask::empty());
    const MODULE: &'static str = "fs.windows";
    const NAME: &'static str = "AccessMask";
    const BITS: &'static [(&'static str, AccessMask)] = &[
        ("READ_DATA", AccessMask::bits(0x0001)),
        ("WRITE_DATA", AccessMask::bits(0x0002)),
        ("APPEND_DATA", AccessMask::bits(0x0004)),
        ("READ_EA", AccessMask::bits(0x0008)),
        ("WRITE_EA", AccessMask::bits(0x0010)),
        ("EXECUTE", AccessMask::bits(0x0020)),
        ("DELETE_CHILD", AccessMask::bits(0x0040)),
        ("READ_ATTRIBUTES", AccessMask::bits(0x0080)),
        ("WRITE_ATTRIBUTES", AccessMask::bits(0x0100)),
        ("DELETE", AccessMask::win(WinAccessMask::DELETE)),
        ("READ_CONTROL", AccessMask::win(WinAccessMask::READ_CONTROL)),
        ("WRITE_DAC", AccessMask::win(WinAccessMask::WRITE_DAC)),
        ("WRITE_OWNER", AccessMask::win(WinAccessMask::WRITE_OWNER)),
        ("SYNCHRONIZE", AccessMask::win(WinAccessMask::SYNCHRONIZE)),
        (
            "STANDARD_RIGHTS_REQUIRED",
            AccessMask::win(WinAccessMask::STANDARD_RIGHTS_REQUIRED),
        ),
        (
            "STANDARD_RIGHTS_ALL",
            AccessMask::win(WinAccessMask::STANDARD_RIGHTS_ALL),
        ),
        (
            "ACCESS_SYSTEM_SECURITY",
            AccessMask::win(WinAccessMask::ACCESS_SYSTEM_SECURITY),
        ),
        (
            "MAXIMUM_ALLOWED",
            AccessMask::win(WinAccessMask::MAXIMUM_ALLOWED),
        ),
        ("GENERIC_READ", AccessMask::win(WinAccessMask::GENERIC_READ)),
        (
            "GENERIC_WRITE",
            AccessMask::win(WinAccessMask::GENERIC_WRITE),
        ),
        (
            "GENERIC_EXECUTE",
            AccessMask::win(WinAccessMask::GENERIC_EXECUTE),
        ),
        ("GENERIC_ALL", AccessMask::win(WinAccessMask::GENERIC_ALL)),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    fn build<'v, 'a>(
        mut builder: TypeBuilder<'v, 'a, Flags<Self>>,
    ) -> TypeBuilder<'v, 'a, Flags<Self>> {
        // `security.windows.AccessMask` is a nominal supertype so these
        // rights can be used wherever a Windows access mask is expected;
        // that contract is what the `int` projection below satisfies.
        let base = crate::windows_access_mask_type(&mut builder);
        builder
            .nominal_supertype(base)
            .get("int", |this, strand, out| {
                Output::set(strand, out, this.flags().0.bits());
                Ok(())
            })
            .type_method("from_int", async move |this, strand, args, out| {
                let ([value], []) = unpack!(strand, args, 1, 0)?;
                let value = value.to_i64(strand)?;
                let value = u32::try_from(value)
                    .map_err(|_| Error::value(strand, "flags integer out of range"))?;
                this.create_flags(strand, Self(WinAccessMask::from_bits_retain(value)), out);
                Ok(())
            })
    }
}
