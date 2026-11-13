//! `winscm.ServiceType`/`NotifyMask`/`ServiceControlsAccepted` — bespoke
//! bitmask types with no relation to `AccessMask`, each mirroring one of
//! `dolang_vfs_winscm`'s wire bitmask types.

use std::ops::{BitAnd, BitOr, BitXor, Not};

use dolang::runtime::object::FlagLike;
use dolang_vfs_winscm::{
    NotifyMask as WireNotifyMask, ServiceControlsAccepted as WireServiceControlsAccepted,
    ServiceType as WireServiceType,
};

macro_rules! int_repr {
    ($local:ty, $wire:ty) => {
        const INT: bool = true;

        fn to_i128(self) -> Option<i128> {
            Some(self.0.bits().into())
        }

        fn from_i128(value: i128) -> Option<Self> {
            u32::try_from(value)
                .ok()
                .map(|bits| <$local>::from(<$wire>::from_bits_retain(bits)))
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ServiceType(pub(crate) WireServiceType);

impl ServiceType {
    pub(crate) const KERNEL_DRIVER: ServiceType = ServiceType(WireServiceType::KERNEL_DRIVER);
    pub(crate) const FILE_SYSTEM_DRIVER: ServiceType =
        ServiceType(WireServiceType::FILE_SYSTEM_DRIVER);
    pub(crate) const WIN32_OWN_PROCESS: ServiceType =
        ServiceType(WireServiceType::WIN32_OWN_PROCESS);
    pub(crate) const WIN32_SHARE_PROCESS: ServiceType =
        ServiceType(WireServiceType::WIN32_SHARE_PROCESS);
    pub(crate) const INTERACTIVE_PROCESS: ServiceType =
        ServiceType(WireServiceType::INTERACTIVE_PROCESS);
    pub(crate) const DRIVER: ServiceType = ServiceType(WireServiceType::DRIVER);
    pub(crate) const WIN32: ServiceType = ServiceType(WireServiceType::WIN32);
}

impl BitOr for ServiceType {
    type Output = ServiceType;
    fn bitor(self, rhs: ServiceType) -> ServiceType {
        ServiceType(self.0 | rhs.0)
    }
}

impl BitAnd for ServiceType {
    type Output = ServiceType;
    fn bitand(self, rhs: ServiceType) -> ServiceType {
        ServiceType(self.0 & rhs.0)
    }
}

impl BitXor for ServiceType {
    type Output = ServiceType;
    fn bitxor(self, rhs: ServiceType) -> ServiceType {
        ServiceType(self.0 ^ rhs.0)
    }
}

impl Not for ServiceType {
    type Output = ServiceType;
    fn not(self) -> ServiceType {
        ServiceType(!self.0)
    }
}

impl FlagLike for ServiceType {
    const ZERO: ServiceType = ServiceType(WireServiceType::empty());
    const MODULE: &'static str = "winscm";
    const NAME: &'static str = "ServiceType";
    const BITS: &'static [(&'static str, ServiceType)] = &[
        ("KERNEL_DRIVER", ServiceType::KERNEL_DRIVER),
        ("FILE_SYSTEM_DRIVER", ServiceType::FILE_SYSTEM_DRIVER),
        ("WIN32_OWN_PROCESS", ServiceType::WIN32_OWN_PROCESS),
        ("WIN32_SHARE_PROCESS", ServiceType::WIN32_SHARE_PROCESS),
        ("INTERACTIVE_PROCESS", ServiceType::INTERACTIVE_PROCESS),
        ("DRIVER", ServiceType::DRIVER),
        ("WIN32", ServiceType::WIN32),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    int_repr!(ServiceType, WireServiceType);
}

impl From<WireServiceType> for ServiceType {
    fn from(wire: WireServiceType) -> ServiceType {
        ServiceType(wire)
    }
}

impl From<ServiceType> for WireServiceType {
    fn from(mask: ServiceType) -> WireServiceType {
        mask.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NotifyMask(pub(crate) WireNotifyMask);

impl NotifyMask {
    pub(crate) const STOPPED: NotifyMask = NotifyMask(WireNotifyMask::STOPPED);
    pub(crate) const START_PENDING: NotifyMask = NotifyMask(WireNotifyMask::START_PENDING);
    pub(crate) const STOP_PENDING: NotifyMask = NotifyMask(WireNotifyMask::STOP_PENDING);
    pub(crate) const RUNNING: NotifyMask = NotifyMask(WireNotifyMask::RUNNING);
    pub(crate) const CONTINUE_PENDING: NotifyMask = NotifyMask(WireNotifyMask::CONTINUE_PENDING);
    pub(crate) const PAUSE_PENDING: NotifyMask = NotifyMask(WireNotifyMask::PAUSE_PENDING);
    pub(crate) const PAUSED: NotifyMask = NotifyMask(WireNotifyMask::PAUSED);
    pub(crate) const CREATED: NotifyMask = NotifyMask(WireNotifyMask::CREATED);
    pub(crate) const DELETED: NotifyMask = NotifyMask(WireNotifyMask::DELETED);
    pub(crate) const DELETE_PENDING: NotifyMask = NotifyMask(WireNotifyMask::DELETE_PENDING);
}

impl BitOr for NotifyMask {
    type Output = NotifyMask;
    fn bitor(self, rhs: NotifyMask) -> NotifyMask {
        NotifyMask(self.0 | rhs.0)
    }
}

impl BitAnd for NotifyMask {
    type Output = NotifyMask;
    fn bitand(self, rhs: NotifyMask) -> NotifyMask {
        NotifyMask(self.0 & rhs.0)
    }
}

impl BitXor for NotifyMask {
    type Output = NotifyMask;
    fn bitxor(self, rhs: NotifyMask) -> NotifyMask {
        NotifyMask(self.0 ^ rhs.0)
    }
}

impl Not for NotifyMask {
    type Output = NotifyMask;
    fn not(self) -> NotifyMask {
        NotifyMask(!self.0)
    }
}

impl FlagLike for NotifyMask {
    const ZERO: NotifyMask = NotifyMask(WireNotifyMask::empty());
    const MODULE: &'static str = "winscm";
    const NAME: &'static str = "NotifyMask";
    const BITS: &'static [(&'static str, NotifyMask)] = &[
        ("STOPPED", NotifyMask::STOPPED),
        ("START_PENDING", NotifyMask::START_PENDING),
        ("STOP_PENDING", NotifyMask::STOP_PENDING),
        ("RUNNING", NotifyMask::RUNNING),
        ("CONTINUE_PENDING", NotifyMask::CONTINUE_PENDING),
        ("PAUSE_PENDING", NotifyMask::PAUSE_PENDING),
        ("PAUSED", NotifyMask::PAUSED),
        ("CREATED", NotifyMask::CREATED),
        ("DELETED", NotifyMask::DELETED),
        ("DELETE_PENDING", NotifyMask::DELETE_PENDING),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    int_repr!(NotifyMask, WireNotifyMask);
}

impl From<WireNotifyMask> for NotifyMask {
    fn from(wire: WireNotifyMask) -> Self {
        Self(wire)
    }
}

impl From<NotifyMask> for WireNotifyMask {
    fn from(mask: NotifyMask) -> WireNotifyMask {
        mask.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ServiceControlsAccepted(pub(crate) WireServiceControlsAccepted);

impl ServiceControlsAccepted {
    pub(crate) const STOP: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::STOP);
    pub(crate) const PAUSE_CONTINUE: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::PAUSE_CONTINUE);
    pub(crate) const SHUTDOWN: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::SHUTDOWN);
    pub(crate) const PARAMCHANGE: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::PARAMCHANGE);
    pub(crate) const NETBINDCHANGE: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::NETBINDCHANGE);
    pub(crate) const HARDWAREPROFILECHANGE: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::HARDWAREPROFILECHANGE);
    pub(crate) const POWEREVENT: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::POWEREVENT);
    pub(crate) const SESSIONCHANGE: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::SESSIONCHANGE);
    pub(crate) const PRESHUTDOWN: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::PRESHUTDOWN);
    pub(crate) const TIMECHANGE: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::TIMECHANGE);
    pub(crate) const TRIGGEREVENT: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::TRIGGEREVENT);
}

impl BitOr for ServiceControlsAccepted {
    type Output = ServiceControlsAccepted;
    fn bitor(self, rhs: ServiceControlsAccepted) -> ServiceControlsAccepted {
        ServiceControlsAccepted(self.0 | rhs.0)
    }
}

impl BitAnd for ServiceControlsAccepted {
    type Output = ServiceControlsAccepted;
    fn bitand(self, rhs: ServiceControlsAccepted) -> ServiceControlsAccepted {
        ServiceControlsAccepted(self.0 & rhs.0)
    }
}

impl BitXor for ServiceControlsAccepted {
    type Output = ServiceControlsAccepted;
    fn bitxor(self, rhs: ServiceControlsAccepted) -> ServiceControlsAccepted {
        ServiceControlsAccepted(self.0 ^ rhs.0)
    }
}

impl Not for ServiceControlsAccepted {
    type Output = ServiceControlsAccepted;
    fn not(self) -> ServiceControlsAccepted {
        ServiceControlsAccepted(!self.0)
    }
}

impl FlagLike for ServiceControlsAccepted {
    const ZERO: ServiceControlsAccepted =
        ServiceControlsAccepted(WireServiceControlsAccepted::empty());
    const MODULE: &'static str = "winscm";
    const NAME: &'static str = "ServiceControlsAccepted";
    const BITS: &'static [(&'static str, ServiceControlsAccepted)] = &[
        ("STOP", ServiceControlsAccepted::STOP),
        ("PAUSE_CONTINUE", ServiceControlsAccepted::PAUSE_CONTINUE),
        ("SHUTDOWN", ServiceControlsAccepted::SHUTDOWN),
        ("PARAMCHANGE", ServiceControlsAccepted::PARAMCHANGE),
        ("NETBINDCHANGE", ServiceControlsAccepted::NETBINDCHANGE),
        (
            "HARDWAREPROFILECHANGE",
            ServiceControlsAccepted::HARDWAREPROFILECHANGE,
        ),
        ("POWEREVENT", ServiceControlsAccepted::POWEREVENT),
        ("SESSIONCHANGE", ServiceControlsAccepted::SESSIONCHANGE),
        ("PRESHUTDOWN", ServiceControlsAccepted::PRESHUTDOWN),
        ("TIMECHANGE", ServiceControlsAccepted::TIMECHANGE),
        ("TRIGGEREVENT", ServiceControlsAccepted::TRIGGEREVENT),
    ];

    fn rank(self) -> usize {
        self.0.bits().count_ones() as usize
    }

    int_repr!(ServiceControlsAccepted, WireServiceControlsAccepted);
}

impl From<WireServiceControlsAccepted> for ServiceControlsAccepted {
    fn from(wire: WireServiceControlsAccepted) -> ServiceControlsAccepted {
        ServiceControlsAccepted(wire)
    }
}
