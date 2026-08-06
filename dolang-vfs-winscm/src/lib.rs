//! Windows Service Control Manager VFS extension for `dolang-vfs`.
//!
//! Registers a remoteable [`dolang_vfs::VfsExtension`] providing
//! typed access to the Windows Service Control Manager, dispatched
//! identically whether served in-process or over a real VFS RPC session.
//! See [`ScManager`] for the public entry point.
//!
//! The extension is registered on every platform. On non-Windows targets
//! every operation returns `Err` with [`dolang_vfs::ErrorKind::Unsupported`]
//! rather than the extension being absent, so a caller sees a clear,
//! catchable error instead of a routing failure indistinguishable from a
//! typo in the extension name/version.
//!
//! The status-change wait ([`Service::wait_for_status_change`]) is built on
//! `dolang-winterop`'s APC reactor (`dolang_winterop::apc`), since
//! `NotifyServiceStatusChangeW` delivers its completion as a user-mode APC
//! to the thread that registered it.

mod api;
mod backend;
mod manager;
mod service;
mod wire;

pub use api::{ScManager, Service};
pub use wire::{
    CreateServiceOptions, ErrorControl, NotifyMask, ServiceAccess, ServiceConfig,
    ServiceConfigUpdate, ServiceControl, ServiceControlsAccepted, ServiceInfo, ServiceState,
    ServiceStateFilter, ServiceStatus, ServiceType, StartType,
};
