//! On-the-wire shape of the SCM VFS extension.
//!
//! Nothing in this module is exported from the crate root except the plain
//! data types (`ServiceStatus`, `ServiceAccess`, `ServiceType`, `StartType`,
//! `ErrorControl`, `ServiceControl`, `NotifyMask`, `ServiceState`,
//! `ServiceControlsAccepted`, `ServiceStateFilter`, `ServiceInfo`). Callers
//! only ever see [`crate::ScManager`]/[`crate::Service`] and those data
//! types; the request/response enums here exist solely so [`WinScmExt`] can
//! route and (de)serialize through the VFS extension mechanism.

use dolang_vfs::{Error, ExtContext, ExtOpaque, VfsExtension};
use dolang_winterop::SecDesc;
use serde::{Deserialize, Serialize};

use crate::backend;

/// Marker for the opaque SC manager handle. Never named outside this crate.
pub(crate) struct ScManagerMarker;

/// Marker for the opaque service handle. Never named outside this crate.
pub(crate) struct ServiceMarker;

/// A Windows access-rights bitmask for opening/creating an SCM object.
///
/// Built by OR-ing named constants together, same rationale as
/// `dolang-vfs-winreg`'s `Access`: these are stable, documented Win32 SAM
/// desired-access bits, so no `windows-sys` dependency is needed here — this
/// type stays portable so it still compiles on non-Windows hosts running
/// only the stub backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccess(pub u32);

impl ServiceAccess {
    // SC manager rights
    pub const SC_MANAGER_CONNECT: ServiceAccess = ServiceAccess(0x0001);
    pub const SC_MANAGER_CREATE_SERVICE: ServiceAccess = ServiceAccess(0x0002);
    pub const SC_MANAGER_ENUMERATE_SERVICE: ServiceAccess = ServiceAccess(0x0004);
    pub const SC_MANAGER_LOCK: ServiceAccess = ServiceAccess(0x0008);
    pub const SC_MANAGER_QUERY_LOCK_STATUS: ServiceAccess = ServiceAccess(0x0010);
    pub const SC_MANAGER_MODIFY_BOOT_CONFIG: ServiceAccess = ServiceAccess(0x0020);
    pub const SC_MANAGER_ALL_ACCESS: ServiceAccess = ServiceAccess(0x000F_003F);

    // Service rights
    pub const SERVICE_QUERY_CONFIG: ServiceAccess = ServiceAccess(0x0001);
    pub const SERVICE_CHANGE_CONFIG: ServiceAccess = ServiceAccess(0x0002);
    pub const SERVICE_QUERY_STATUS: ServiceAccess = ServiceAccess(0x0004);
    pub const SERVICE_ENUMERATE_DEPENDENTS: ServiceAccess = ServiceAccess(0x0008);
    pub const SERVICE_START: ServiceAccess = ServiceAccess(0x0010);
    pub const SERVICE_STOP: ServiceAccess = ServiceAccess(0x0020);
    pub const SERVICE_PAUSE_CONTINUE: ServiceAccess = ServiceAccess(0x0040);
    pub const SERVICE_INTERROGATE: ServiceAccess = ServiceAccess(0x0080);
    pub const SERVICE_USER_DEFINED_CONTROL: ServiceAccess = ServiceAccess(0x0100);
    pub const SERVICE_ALL_ACCESS: ServiceAccess = ServiceAccess(0x000F_01FF);

    // Generic object rights, shared by both SC manager and service handles
    // (needed for `sec_desc`/`set_sec_desc`, same as
    // `dolang-vfs-winreg::Access`'s equivalents).
    pub const READ_CONTROL: ServiceAccess = ServiceAccess(0x0002_0000);
    pub const WRITE_DAC: ServiceAccess = ServiceAccess(0x0004_0000);
    pub const WRITE_OWNER: ServiceAccess = ServiceAccess(0x0008_0000);
    pub const ACCESS_SYSTEM_SECURITY: ServiceAccess = ServiceAccess(0x0100_0000);
}

impl std::ops::BitOr for ServiceAccess {
    type Output = ServiceAccess;
    fn bitor(self, rhs: ServiceAccess) -> ServiceAccess {
        ServiceAccess(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ServiceAccess {
    fn bitor_assign(&mut self, rhs: ServiceAccess) {
        self.0 |= rhs.0;
    }
}

/// A service's type, passed to `CreateServiceW` and reported back in
/// [`ServiceStatus::service_type`].
///
/// A bitmask, not a single discrete value: `WIN32_OWN_PROCESS` and
/// `INTERACTIVE_PROCESS` can be combined, for example.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceType(pub u32);

impl ServiceType {
    pub const KERNEL_DRIVER: ServiceType = ServiceType(0x0000_0001);
    pub const FILE_SYSTEM_DRIVER: ServiceType = ServiceType(0x0000_0002);
    pub const WIN32_OWN_PROCESS: ServiceType = ServiceType(0x0000_0010);
    pub const WIN32_SHARE_PROCESS: ServiceType = ServiceType(0x0000_0020);
    pub const INTERACTIVE_PROCESS: ServiceType = ServiceType(0x0000_0100);

    /// All driver types. Only meaningful as an
    /// [`ScManager::enumerate_services`](crate::ScManager::enumerate_services)
    /// filter — not a combination of the individual driver-type consts
    /// above (it also covers reserved/undocumented driver type bits).
    pub const DRIVER: ServiceType = ServiceType(0x0000_000B);
    /// Both Win32 process types. Only meaningful as an
    /// [`ScManager::enumerate_services`](crate::ScManager::enumerate_services)
    /// filter.
    pub const WIN32: ServiceType = ServiceType(0x0000_0030);
}

impl std::ops::BitOr for ServiceType {
    type Output = ServiceType;
    fn bitor(self, rhs: ServiceType) -> ServiceType {
        ServiceType(self.0 | rhs.0)
    }
}

/// A service's start type, passed to `CreateServiceW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartType(pub u32);

impl StartType {
    pub const BOOT_START: StartType = StartType(0);
    pub const SYSTEM_START: StartType = StartType(1);
    pub const AUTO_START: StartType = StartType(2);
    pub const DEMAND_START: StartType = StartType(3);
    pub const DISABLED: StartType = StartType(4);
}

/// A service's error-control level, passed to `CreateServiceW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorControl(pub u32);

impl ErrorControl {
    pub const IGNORE: ErrorControl = ErrorControl(0);
    pub const NORMAL: ErrorControl = ErrorControl(1);
    pub const SEVERE: ErrorControl = ErrorControl(2);
    pub const CRITICAL: ErrorControl = ErrorControl(3);
}

/// A control code passed to `ControlService`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceControl(pub u32);

impl ServiceControl {
    pub const STOP: ServiceControl = ServiceControl(1);
    pub const PAUSE: ServiceControl = ServiceControl(2);
    pub const CONTINUE: ServiceControl = ServiceControl(3);
    pub const INTERROGATE: ServiceControl = ServiceControl(4);
}

/// A bitmask of service states to be notified about, passed to
/// `NotifyServiceStatusChangeW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyMask(pub u32);

impl NotifyMask {
    pub const STOPPED: NotifyMask = NotifyMask(0x0000_0001);
    pub const START_PENDING: NotifyMask = NotifyMask(0x0000_0002);
    pub const STOP_PENDING: NotifyMask = NotifyMask(0x0000_0004);
    pub const RUNNING: NotifyMask = NotifyMask(0x0000_0008);
    pub const CONTINUE_PENDING: NotifyMask = NotifyMask(0x0000_0010);
    pub const PAUSE_PENDING: NotifyMask = NotifyMask(0x0000_0020);
    pub const PAUSED: NotifyMask = NotifyMask(0x0000_0040);
    pub const CREATED: NotifyMask = NotifyMask(0x0000_0080);
    pub const DELETED: NotifyMask = NotifyMask(0x0000_0100);
    pub const DELETE_PENDING: NotifyMask = NotifyMask(0x0000_0200);
}

impl std::ops::BitOr for NotifyMask {
    type Output = NotifyMask;
    fn bitor(self, rhs: NotifyMask) -> NotifyMask {
        NotifyMask(self.0 | rhs.0)
    }
}

/// A service's current lifecycle state, as reported in
/// [`ServiceStatus::current_state`].
///
/// A discrete value (unlike [`ServiceType`]/[`ServiceControlsAccepted`]):
/// a service is in exactly one of these states at a time, so this has no
/// `BitOr` impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceState(pub u32);

impl ServiceState {
    pub const STOPPED: ServiceState = ServiceState(1);
    pub const START_PENDING: ServiceState = ServiceState(2);
    pub const STOP_PENDING: ServiceState = ServiceState(3);
    pub const RUNNING: ServiceState = ServiceState(4);
    pub const CONTINUE_PENDING: ServiceState = ServiceState(5);
    pub const PAUSE_PENDING: ServiceState = ServiceState(6);
    pub const PAUSED: ServiceState = ServiceState(7);
}

/// A bitmask of control codes a service currently accepts, as reported in
/// [`ServiceStatus::controls_accepted`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceControlsAccepted(pub u32);

impl ServiceControlsAccepted {
    pub const STOP: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0001);
    pub const PAUSE_CONTINUE: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0002);
    pub const SHUTDOWN: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0004);
    pub const PARAMCHANGE: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0008);
    pub const NETBINDCHANGE: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0010);
    pub const HARDWAREPROFILECHANGE: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0020);
    pub const POWEREVENT: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0040);
    pub const SESSIONCHANGE: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0080);
    pub const PRESHUTDOWN: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0100);
    pub const TIMECHANGE: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0200);
    pub const TRIGGEREVENT: ServiceControlsAccepted = ServiceControlsAccepted(0x0000_0400);
}

impl std::ops::BitOr for ServiceControlsAccepted {
    type Output = ServiceControlsAccepted;
    fn bitor(self, rhs: ServiceControlsAccepted) -> ServiceControlsAccepted {
        ServiceControlsAccepted(self.0 | rhs.0)
    }
}

/// A service's current status, as reported by `QueryServiceStatusEx` and
/// `NotifyServiceStatusChangeW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub service_type: ServiceType,
    pub current_state: ServiceState,
    pub controls_accepted: ServiceControlsAccepted,
    pub win32_exit_code: u32,
    pub service_specific_exit_code: u32,
    pub check_point: u32,
    pub wait_hint: u32,
    pub process_id: u32,
}

/// Which services to include by activity state, passed to
/// `EnumServicesStatusExW`.
///
/// A discrete selector (unlike [`ServiceType`]/[`ServiceControlsAccepted`]),
/// not a bitmask to be OR'd with other values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStateFilter(pub u32);

impl ServiceStateFilter {
    pub const ACTIVE: ServiceStateFilter = ServiceStateFilter(1);
    pub const INACTIVE: ServiceStateFilter = ServiceStateFilter(2);
    pub const ALL: ServiceStateFilter = ServiceStateFilter(3);
}

/// One entry from [`ScManager::enumerate_services`](crate::ScManager::enumerate_services).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    pub display_name: String,
    pub status: ServiceStatus,
}

/// A snapshot of a service's base configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub service_type: ServiceType,
    pub start_type: StartType,
    pub error_control: ErrorControl,
    pub binary_path: String,
    pub load_order_group: Option<String>,
    pub tag_id: u32,
    pub dependencies: Vec<String>,
    pub service_start_name: String,
    pub display_name: String,
}

/// Optional parameters accepted when creating a service in addition to its
/// required base configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateServiceOptions {
    pub load_order_group: Option<String>,
    pub dependencies: Vec<String>,
    pub service_start_name: Option<String>,
    pub password: Option<String>,
}

/// Base configuration fields to change. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfigUpdate {
    pub service_type: Option<ServiceType>,
    pub start_type: Option<StartType>,
    pub error_control: Option<ErrorControl>,
    pub binary_path: Option<String>,
    pub load_order_group: Option<String>,
    pub dependencies: Option<Vec<String>>,
    pub service_start_name: Option<String>,
    pub password: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WinScmRequest {
    OpenManager {
        access: ServiceAccess,
    },
    CloseManager {
        manager: ExtOpaque<ScManagerMarker>,
    },
    OpenService {
        manager: ExtOpaque<ScManagerMarker>,
        name: String,
        access: ServiceAccess,
    },
    CreateService {
        manager: ExtOpaque<ScManagerMarker>,
        name: String,
        display_name: String,
        service_type: ServiceType,
        start_type: StartType,
        error_control: ErrorControl,
        binary_path: String,
        options: CreateServiceOptions,
        access: ServiceAccess,
    },
    EnumServices {
        manager: ExtOpaque<ScManagerMarker>,
        service_type: ServiceType,
        state_filter: ServiceStateFilter,
    },
    DeleteService {
        service: ExtOpaque<ServiceMarker>,
    },
    CloseService {
        service: ExtOpaque<ServiceMarker>,
    },
    StartService {
        service: ExtOpaque<ServiceMarker>,
        args: Vec<String>,
    },
    ControlService {
        service: ExtOpaque<ServiceMarker>,
        control: ServiceControl,
    },
    QueryStatus {
        service: ExtOpaque<ServiceMarker>,
    },
    QueryConfig {
        service: ExtOpaque<ServiceMarker>,
    },
    ChangeConfig {
        service: ExtOpaque<ServiceMarker>,
        update: ServiceConfigUpdate,
    },
    WaitForStatusChange {
        service: ExtOpaque<ServiceMarker>,
        mask: NotifyMask,
    },
    GetSecDesc {
        service: ExtOpaque<ServiceMarker>,
        mask: u32,
    },
    SetSecDesc {
        service: ExtOpaque<ServiceMarker>,
        sec_desc: SecDesc,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum WinScmResponse {
    Manager(ExtOpaque<ScManagerMarker>),
    Svc(ExtOpaque<ServiceMarker>),
    Closed,
    Deleted,
    Ack,
    Status(ServiceStatus),
    Config(ServiceConfig),
    Services(Vec<ServiceInfo>),
    SecDesc(SecDesc),
}

pub(crate) struct WinScmExt;

impl VfsExtension for WinScmExt {
    type Request = WinScmRequest;
    type Response = Result<WinScmResponse, Error>;

    const NAME: &'static str = "dolang-vfs-winscm";
    const VERSION: u16 = 1;

    async fn handle(&self, ctx: &mut ExtContext<'_>, request: WinScmRequest) -> Self::Response {
        backend::handle(ctx, request).await
    }
}

dolang_vfs::vfs_extension!(WinScmExt);
