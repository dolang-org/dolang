//! Backend registry key resource.
//!
//! Only exists under `#[cfg(windows)]`: the stub backend never calls
//! [`dolang_vfs::ExtContext::register`], so there is nothing to hold
//! on other platforms.

#![cfg(windows)]

use std::sync::Mutex;

use dolang_vfs::ExtResource;
use windows_sys::Win32::System::Registry::{HKEY, RegCloseKey};

use crate::wire::KeyMarker;

/// An open registry key.
///
/// Wrapped in a [`Mutex`] because `RegEnumKeyExW`/`RegEnumValueW` share one
/// index cursor per call (not a persistent per-key cursor), so serializing
/// concurrent operations on the same key is simplest; this mirrors
/// `dolang-vfs`'s own "cursor-affecting operations on each retained
/// file are serialized by the server" precedent for retained files.
pub(crate) struct Key(pub(crate) Mutex<HKEY>);

// SAFETY: Win32 registry handles are valid to use from any thread; they just
// require external synchronization for cursor-affecting operations, which
// the `Mutex` above provides.
unsafe impl Send for Key {}
unsafe impl Sync for Key {}

impl ExtResource for Key {
    type Marker = KeyMarker;
}

impl Drop for Key {
    fn drop(&mut self) {
        let handle = *self
            .0
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: `handle` came from a successful RegOpenKeyExW/RegCreateKeyExW
        // and is only ever closed once, here.
        unsafe {
            RegCloseKey(handle);
        }
    }
}

impl Key {
    pub(crate) fn new(handle: HKEY) -> Self {
        Self(Mutex::new(handle))
    }
}
