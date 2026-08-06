//! `winscm.ManagerAccessMask`/`winscm.ServiceAccessMask` — access rights for
//! an `ScManager`/`Service` handle respectively: the generic Windows
//! object-security bits plus each handle's own specific rights. Both share
//! one wire representation (`dolang_vfs_winscm::ServiceAccess`) but have
//! disjoint symbol sets (`SC_MANAGER_*` vs `SERVICE_*`), so each gets its own
//! local `FlagLike` type.

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::object::FlagLike;
use dolang_vfs_winscm::ServiceAccess as WireServiceAccess;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ManagerAccessMask(pub(crate) u32);

impl ManagerAccessMask {
    pub(crate) const SC_MANAGER_CONNECT: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_CONNECT.0);
    pub(crate) const SC_MANAGER_CREATE_SERVICE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_CREATE_SERVICE.0);
    pub(crate) const SC_MANAGER_ENUMERATE_SERVICE: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_ENUMERATE_SERVICE.0);
    pub(crate) const SC_MANAGER_LOCK: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_LOCK.0);
    pub(crate) const SC_MANAGER_QUERY_LOCK_STATUS: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_QUERY_LOCK_STATUS.0);
    pub(crate) const SC_MANAGER_MODIFY_BOOT_CONFIG: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_MODIFY_BOOT_CONFIG.0);
    pub(crate) const SC_MANAGER_ALL_ACCESS: ManagerAccessMask =
        ManagerAccessMask(WireServiceAccess::SC_MANAGER_ALL_ACCESS.0);
    pub(crate) const DELETE: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::DELETE.0);
    pub(crate) const READ_CONTROL: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::READ_CONTROL.0);
    pub(crate) const WRITE_DAC: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::WRITE_DAC.0);
    pub(crate) const WRITE_OWNER: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::WRITE_OWNER.0);
    pub(crate) const SYNCHRONIZE: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::SYNCHRONIZE.0);
    pub(crate) const STANDARD_RIGHTS_REQUIRED: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::STANDARD_RIGHTS_REQUIRED.0);
    pub(crate) const STANDARD_RIGHTS_ALL: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::STANDARD_RIGHTS_ALL.0);
    pub(crate) const ACCESS_SYSTEM_SECURITY: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::ACCESS_SYSTEM_SECURITY.0);
    pub(crate) const MAXIMUM_ALLOWED: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::MAXIMUM_ALLOWED.0);
    pub(crate) const GENERIC_READ: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::GENERIC_READ.0);
    pub(crate) const GENERIC_WRITE: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::GENERIC_WRITE.0);
    pub(crate) const GENERIC_EXECUTE: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::GENERIC_EXECUTE.0);
    pub(crate) const GENERIC_ALL: ManagerAccessMask =
        ManagerAccessMask(dolang_winterop::AccessMask::GENERIC_ALL.0);
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
        ManagerAccessMask(self.0 & rhs.0)
    }
}

impl BitXor for ManagerAccessMask {
    type Output = ManagerAccessMask;
    fn bitxor(self, rhs: ManagerAccessMask) -> ManagerAccessMask {
        ManagerAccessMask(self.0 ^ rhs.0)
    }
}

impl Not for ManagerAccessMask {
    type Output = ManagerAccessMask;
    fn not(self) -> ManagerAccessMask {
        ManagerAccessMask(!self.0)
    }
}

impl FlagLike for ManagerAccessMask {
    const ZERO: ManagerAccessMask = ManagerAccessMask(0);
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
}

impl From<ManagerAccessMask> for WireServiceAccess {
    fn from(mask: ManagerAccessMask) -> WireServiceAccess {
        WireServiceAccess(mask.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ServiceAccessMask(pub(crate) u32);

impl ServiceAccessMask {
    pub(crate) const SERVICE_QUERY_CONFIG: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_QUERY_CONFIG.0);
    pub(crate) const SERVICE_CHANGE_CONFIG: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_CHANGE_CONFIG.0);
    pub(crate) const SERVICE_QUERY_STATUS: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_QUERY_STATUS.0);
    pub(crate) const SERVICE_ENUMERATE_DEPENDENTS: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_ENUMERATE_DEPENDENTS.0);
    pub(crate) const SERVICE_START: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_START.0);
    pub(crate) const SERVICE_STOP: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_STOP.0);
    pub(crate) const SERVICE_PAUSE_CONTINUE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_PAUSE_CONTINUE.0);
    pub(crate) const SERVICE_INTERROGATE: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_INTERROGATE.0);
    pub(crate) const SERVICE_USER_DEFINED_CONTROL: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_USER_DEFINED_CONTROL.0);
    pub(crate) const SERVICE_ALL_ACCESS: ServiceAccessMask =
        ServiceAccessMask(WireServiceAccess::SERVICE_ALL_ACCESS.0);
    pub(crate) const DELETE: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::DELETE.0);
    pub(crate) const READ_CONTROL: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::READ_CONTROL.0);
    pub(crate) const WRITE_DAC: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::WRITE_DAC.0);
    pub(crate) const WRITE_OWNER: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::WRITE_OWNER.0);
    pub(crate) const SYNCHRONIZE: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::SYNCHRONIZE.0);
    pub(crate) const STANDARD_RIGHTS_REQUIRED: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::STANDARD_RIGHTS_REQUIRED.0);
    pub(crate) const STANDARD_RIGHTS_ALL: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::STANDARD_RIGHTS_ALL.0);
    pub(crate) const ACCESS_SYSTEM_SECURITY: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::ACCESS_SYSTEM_SECURITY.0);
    pub(crate) const MAXIMUM_ALLOWED: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::MAXIMUM_ALLOWED.0);
    pub(crate) const GENERIC_READ: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::GENERIC_READ.0);
    pub(crate) const GENERIC_WRITE: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::GENERIC_WRITE.0);
    pub(crate) const GENERIC_EXECUTE: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::GENERIC_EXECUTE.0);
    pub(crate) const GENERIC_ALL: ServiceAccessMask =
        ServiceAccessMask(dolang_winterop::AccessMask::GENERIC_ALL.0);
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
        ServiceAccessMask(self.0 & rhs.0)
    }
}

impl BitXor for ServiceAccessMask {
    type Output = ServiceAccessMask;
    fn bitxor(self, rhs: ServiceAccessMask) -> ServiceAccessMask {
        ServiceAccessMask(self.0 ^ rhs.0)
    }
}

impl Not for ServiceAccessMask {
    type Output = ServiceAccessMask;
    fn not(self) -> ServiceAccessMask {
        ServiceAccessMask(!self.0)
    }
}

impl FlagLike for ServiceAccessMask {
    const ZERO: ServiceAccessMask = ServiceAccessMask(0);
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
}

impl From<ServiceAccessMask> for WireServiceAccess {
    fn from(mask: ServiceAccessMask) -> WireServiceAccess {
        WireServiceAccess(mask.0)
    }
}
