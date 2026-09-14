//! `winscm.ManagerAccessMask`/`winscm.ServiceAccessMask` — access rights for
//! an `ScManager`/`Service` handle respectively: the generic Windows
//! object-security bits plus each handle's own specific rights. Both share
//! one wire representation (`dolang_vfs_winscm::ServiceAccess`) but have
//! disjoint symbol sets (`SC_MANAGER_*` vs `SERVICE_*`), so each gets its own
//! local `FlagLike` type.

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::object::{FlagLike, Flags, FlagsInstanceExt, FlagsTypeExt, TypeBuilder};
use dolang::runtime::{Error, Output, unpack};
use dolang_vfs_winscm::ServiceAccess as WireServiceAccess;
use dolang_winterop::security::AccessMask as WinAccessMask;

macro_rules! raw_projection {
    () => {
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
                        Self(WireServiceAccess(WinAccessMask::from_bits_retain(value))),
                        out,
                    );
                    Ok(())
                })
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ManagerAccessMask(pub(crate) WireServiceAccess);

impl ManagerAccessMask {
    pub(crate) const SC_MANAGER_CONNECT: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_CONNECT);
    pub(crate) const SC_MANAGER_CREATE_SERVICE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_CREATE_SERVICE);
    pub(crate) const SC_MANAGER_ENUMERATE_SERVICE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_ENUMERATE_SERVICE);
    pub(crate) const SC_MANAGER_LOCK: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_LOCK);
    pub(crate) const SC_MANAGER_QUERY_LOCK_STATUS: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_QUERY_LOCK_STATUS);
    pub(crate) const SC_MANAGER_MODIFY_BOOT_CONFIG: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_MODIFY_BOOT_CONFIG);
    pub(crate) const SC_MANAGER_ALL_ACCESS: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_ALL_ACCESS);
    pub(crate) const DELETE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::DELETE));
    pub(crate) const READ_CONTROL: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::READ_CONTROL));
    pub(crate) const WRITE_DAC: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::WRITE_DAC));
    pub(crate) const WRITE_OWNER: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::WRITE_OWNER));
    pub(crate) const SYNCHRONIZE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::SYNCHRONIZE));
    pub(crate) const STANDARD_RIGHTS_REQUIRED: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::STANDARD_RIGHTS_REQUIRED));
    pub(crate) const STANDARD_RIGHTS_ALL: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::STANDARD_RIGHTS_ALL));
    pub(crate) const ACCESS_SYSTEM_SECURITY: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::ACCESS_SYSTEM_SECURITY));
    pub(crate) const MAXIMUM_ALLOWED: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::MAXIMUM_ALLOWED));
    pub(crate) const GENERIC_READ: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::GENERIC_READ));
    pub(crate) const GENERIC_WRITE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::GENERIC_WRITE));
    pub(crate) const GENERIC_EXECUTE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::GENERIC_EXECUTE));
    pub(crate) const GENERIC_ALL: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess(WinAccessMask::GENERIC_ALL));
}

impl BitOr for ManagerAccessMask {
    type Output = ManagerAccessMask;
    fn bitor(self, rhs: ManagerAccessMask) -> ManagerAccessMask {
        ManagerAccessMask(self.0 | rhs.0)
    }
}

impl BitAnd for ManagerAccessMask {
    type Output = ManagerAccessMask;
    fn bitand(self, rhs: ManagerAccessMask) -> ManagerAccessMask {
        ManagerAccessMask(WireServiceAccess(self.0.0 & rhs.0.0))
    }
}

impl BitXor for ManagerAccessMask {
    type Output = ManagerAccessMask;
    fn bitxor(self, rhs: ManagerAccessMask) -> ManagerAccessMask {
        ManagerAccessMask(WireServiceAccess(self.0.0 ^ rhs.0.0))
    }
}

impl Not for ManagerAccessMask {
    type Output = ManagerAccessMask;
    fn not(self) -> ManagerAccessMask {
        ManagerAccessMask(WireServiceAccess(!self.0.0))
    }
}

impl FlagLike for ManagerAccessMask {
    const ZERO: ManagerAccessMask = ManagerAccessMask(WireServiceAccess(WinAccessMask::empty()));
    const MODULE: &'static str = "winscm";
    const NAME: &'static str = "ManagerAccessMask";
    const BITS: &'static [(&'static str, ManagerAccessMask)] = &[
        ("SC_MANAGER_CONNECT", ManagerAccessMask::SC_MANAGER_CONNECT),
        (
            "SC_MANAGER_CREATE_SERVICE",
            ManagerAccessMask::SC_MANAGER_CREATE_SERVICE,
        ),
        (
            "SC_MANAGER_ENUMERATE_SERVICE",
            ManagerAccessMask::SC_MANAGER_ENUMERATE_SERVICE,
        ),
        ("SC_MANAGER_LOCK", ManagerAccessMask::SC_MANAGER_LOCK),
        (
            "SC_MANAGER_QUERY_LOCK_STATUS",
            ManagerAccessMask::SC_MANAGER_QUERY_LOCK_STATUS,
        ),
        (
            "SC_MANAGER_MODIFY_BOOT_CONFIG",
            ManagerAccessMask::SC_MANAGER_MODIFY_BOOT_CONFIG,
        ),
        (
            "SC_MANAGER_ALL_ACCESS",
            ManagerAccessMask::SC_MANAGER_ALL_ACCESS,
        ),
        ("DELETE", ManagerAccessMask::DELETE),
        ("READ_CONTROL", ManagerAccessMask::READ_CONTROL),
        ("WRITE_DAC", ManagerAccessMask::WRITE_DAC),
        ("WRITE_OWNER", ManagerAccessMask::WRITE_OWNER),
        ("SYNCHRONIZE", ManagerAccessMask::SYNCHRONIZE),
        (
            "STANDARD_RIGHTS_REQUIRED",
            ManagerAccessMask::STANDARD_RIGHTS_REQUIRED,
        ),
        (
            "STANDARD_RIGHTS_ALL",
            ManagerAccessMask::STANDARD_RIGHTS_ALL,
        ),
        (
            "ACCESS_SYSTEM_SECURITY",
            ManagerAccessMask::ACCESS_SYSTEM_SECURITY,
        ),
        ("MAXIMUM_ALLOWED", ManagerAccessMask::MAXIMUM_ALLOWED),
        ("GENERIC_READ", ManagerAccessMask::GENERIC_READ),
        ("GENERIC_WRITE", ManagerAccessMask::GENERIC_WRITE),
        ("GENERIC_EXECUTE", ManagerAccessMask::GENERIC_EXECUTE),
        ("GENERIC_ALL", ManagerAccessMask::GENERIC_ALL),
    ];

    fn rank(self) -> usize {
        self.0.0.bits().count_ones() as usize
    }

    raw_projection!();
}

impl From<ManagerAccessMask> for WireServiceAccess {
    fn from(mask: ManagerAccessMask) -> WireServiceAccess {
        mask.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ServiceAccessMask(pub(crate) WireServiceAccess);

impl ServiceAccessMask {
    pub(crate) const SERVICE_QUERY_CONFIG: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_QUERY_CONFIG);
    pub(crate) const SERVICE_CHANGE_CONFIG: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_CHANGE_CONFIG);
    pub(crate) const SERVICE_QUERY_STATUS: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_QUERY_STATUS);
    pub(crate) const SERVICE_ENUMERATE_DEPENDENTS: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_ENUMERATE_DEPENDENTS);
    pub(crate) const SERVICE_START: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_START);
    pub(crate) const SERVICE_STOP: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_STOP);
    pub(crate) const SERVICE_PAUSE_CONTINUE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_PAUSE_CONTINUE);
    pub(crate) const SERVICE_INTERROGATE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_INTERROGATE);
    pub(crate) const SERVICE_USER_DEFINED_CONTROL: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_USER_DEFINED_CONTROL);
    pub(crate) const SERVICE_ALL_ACCESS: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_ALL_ACCESS);
    pub(crate) const DELETE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::DELETE));
    pub(crate) const READ_CONTROL: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::READ_CONTROL));
    pub(crate) const WRITE_DAC: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::WRITE_DAC));
    pub(crate) const WRITE_OWNER: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::WRITE_OWNER));
    pub(crate) const SYNCHRONIZE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::SYNCHRONIZE));
    pub(crate) const STANDARD_RIGHTS_REQUIRED: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::STANDARD_RIGHTS_REQUIRED));
    pub(crate) const STANDARD_RIGHTS_ALL: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::STANDARD_RIGHTS_ALL));
    pub(crate) const ACCESS_SYSTEM_SECURITY: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::ACCESS_SYSTEM_SECURITY));
    pub(crate) const MAXIMUM_ALLOWED: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::MAXIMUM_ALLOWED));
    pub(crate) const GENERIC_READ: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::GENERIC_READ));
    pub(crate) const GENERIC_WRITE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::GENERIC_WRITE));
    pub(crate) const GENERIC_EXECUTE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::GENERIC_EXECUTE));
    pub(crate) const GENERIC_ALL: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess(WinAccessMask::GENERIC_ALL));
}

impl BitOr for ServiceAccessMask {
    type Output = ServiceAccessMask;
    fn bitor(self, rhs: ServiceAccessMask) -> ServiceAccessMask {
        ServiceAccessMask(self.0 | rhs.0)
    }
}

impl BitAnd for ServiceAccessMask {
    type Output = ServiceAccessMask;
    fn bitand(self, rhs: ServiceAccessMask) -> ServiceAccessMask {
        ServiceAccessMask(WireServiceAccess(self.0.0 & rhs.0.0))
    }
}

impl BitXor for ServiceAccessMask {
    type Output = ServiceAccessMask;
    fn bitxor(self, rhs: ServiceAccessMask) -> ServiceAccessMask {
        ServiceAccessMask(WireServiceAccess(self.0.0 ^ rhs.0.0))
    }
}

impl Not for ServiceAccessMask {
    type Output = ServiceAccessMask;
    fn not(self) -> ServiceAccessMask {
        ServiceAccessMask(WireServiceAccess(!self.0.0))
    }
}

impl FlagLike for ServiceAccessMask {
    const ZERO: ServiceAccessMask = ServiceAccessMask(WireServiceAccess(WinAccessMask::empty()));
    const MODULE: &'static str = "winscm";
    const NAME: &'static str = "ServiceAccessMask";
    const BITS: &'static [(&'static str, ServiceAccessMask)] = &[
        (
            "SERVICE_QUERY_CONFIG",
            ServiceAccessMask::SERVICE_QUERY_CONFIG,
        ),
        (
            "SERVICE_CHANGE_CONFIG",
            ServiceAccessMask::SERVICE_CHANGE_CONFIG,
        ),
        (
            "SERVICE_QUERY_STATUS",
            ServiceAccessMask::SERVICE_QUERY_STATUS,
        ),
        (
            "SERVICE_ENUMERATE_DEPENDENTS",
            ServiceAccessMask::SERVICE_ENUMERATE_DEPENDENTS,
        ),
        ("SERVICE_START", ServiceAccessMask::SERVICE_START),
        ("SERVICE_STOP", ServiceAccessMask::SERVICE_STOP),
        (
            "SERVICE_PAUSE_CONTINUE",
            ServiceAccessMask::SERVICE_PAUSE_CONTINUE,
        ),
        (
            "SERVICE_INTERROGATE",
            ServiceAccessMask::SERVICE_INTERROGATE,
        ),
        (
            "SERVICE_USER_DEFINED_CONTROL",
            ServiceAccessMask::SERVICE_USER_DEFINED_CONTROL,
        ),
        ("SERVICE_ALL_ACCESS", ServiceAccessMask::SERVICE_ALL_ACCESS),
        ("DELETE", ServiceAccessMask::DELETE),
        ("READ_CONTROL", ServiceAccessMask::READ_CONTROL),
        ("WRITE_DAC", ServiceAccessMask::WRITE_DAC),
        ("WRITE_OWNER", ServiceAccessMask::WRITE_OWNER),
        ("SYNCHRONIZE", ServiceAccessMask::SYNCHRONIZE),
        (
            "STANDARD_RIGHTS_REQUIRED",
            ServiceAccessMask::STANDARD_RIGHTS_REQUIRED,
        ),
        (
            "STANDARD_RIGHTS_ALL",
            ServiceAccessMask::STANDARD_RIGHTS_ALL,
        ),
        (
            "ACCESS_SYSTEM_SECURITY",
            ServiceAccessMask::ACCESS_SYSTEM_SECURITY,
        ),
        ("MAXIMUM_ALLOWED", ServiceAccessMask::MAXIMUM_ALLOWED),
        ("GENERIC_READ", ServiceAccessMask::GENERIC_READ),
        ("GENERIC_WRITE", ServiceAccessMask::GENERIC_WRITE),
        ("GENERIC_EXECUTE", ServiceAccessMask::GENERIC_EXECUTE),
        ("GENERIC_ALL", ServiceAccessMask::GENERIC_ALL),
    ];

    fn rank(self) -> usize {
        self.0.0.bits().count_ones() as usize
    }

    raw_projection!();
}

impl From<ServiceAccessMask> for WireServiceAccess {
    fn from(mask: ServiceAccessMask) -> WireServiceAccess {
        mask.0
    }
}
