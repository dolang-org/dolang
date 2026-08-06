//! Backend service resource.
//!
//! Only exists under `#[cfg(windows)]`: the stub backend never calls
//! [`dolang_vfs::ExtContext::register`], so there is nothing to hold
//! on other platforms.

#![cfg(windows)]

use std::sync::Arc;

use dolang_vfs::ExtResource;
use dolang_winterop::Reactor;
use windows_sys::Win32::System::Services::{CloseServiceHandle, SC_HANDLE};

use crate::wire::ServiceMarker;

/// An open handle to a specific service.
///
/// Retains the [`Reactor`] it was opened/created through so
/// [`crate::backend::windows::wait_for_status_change`] can submit work to it
/// directly, without touching the process-wide reactor cache on every call.
/// This is also what keeps the reactor's background thread alive for
/// exactly as long as at least one `Service` handle referencing it exists —
/// see `crate::backend::windows::reactor` for the cache/quiescence design.
///
/// Also retains the service's own `name`: a status-change wait never
/// registers `NotifyServiceStatusChangeW` on `handle` itself. Instead it
/// opens a second, dedicated handle to the same service purely for that one
/// notification (see `crate::backend::windows::wait_for_status_change`) —
/// `name` is what lets it reopen the service on demand. This matters
/// because SCM has no "unregister notification" API: the only documented
/// way to cancel a still-outstanding request is to close the handle it was
/// registered on, and closing `handle` itself would invalidate every other
/// operation this `Service` supports. A dedicated, wait-scoped handle can
/// be closed freely on cancellation without disturbing anything else, and
/// — just as importantly — leaves `handle` free to be used for another
/// `wait_for_status_change` call later, since a handle that has ever had a
/// notification registered on it refuses a second registration
/// (`ERROR_ALREADY_REGISTERED`) until it's closed and reopened.
pub(crate) struct Service {
    pub(crate) handle: SC_HANDLE,
    pub(crate) reactor: Arc<Reactor>,
    pub(crate) name: String,
}

// SAFETY: Win32 service handles are valid to use from any thread.
unsafe impl Send for Service {}
unsafe impl Sync for Service {}

impl ExtResource for Service {
    type Marker = ServiceMarker;
}

impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is a live service handle owned by this
        // value; nothing else can close it first since `Service` isn't
        // `Clone`.
        unsafe {
            CloseServiceHandle(self.handle);
        }
    }
}
