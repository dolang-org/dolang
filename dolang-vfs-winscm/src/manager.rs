//! Backend SC manager resource.
//!
//! Only exists under `#[cfg(windows)]`: the stub backend never calls
//! [`dolang_vfs::ExtContext::register`], so there is nothing to hold
//! on other platforms.

#![cfg(windows)]

use dolang_vfs::ExtResource;
use windows_sys::Win32::System::Services::{CloseServiceHandle, SC_HANDLE};

use crate::wire::ScManagerMarker;

/// An open handle to the Service Control Manager database.
///
/// Unlike `dolang-vfs-winreg`'s `Key`, there is no cursor-affecting
/// operation that shares state across calls on the same handle, so no
/// `Mutex` is needed — the handle is simply safe to use from any thread.
pub(crate) struct ScManager(pub(crate) SC_HANDLE);

// SAFETY: Win32 SC manager handles are valid to use from any thread.
unsafe impl Send for ScManager {}
unsafe impl Sync for ScManager {}

impl ExtResource for ScManager {
    type Marker = ScManagerMarker;
}

impl Drop for ScManager {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live SC manager handle owned by this value;
        // nothing else can close it first since `ScManager` isn't `Clone`.
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
