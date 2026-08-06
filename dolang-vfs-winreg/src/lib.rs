//! Windows registry VFS extension for `dolang-vfs`.
//!
//! Registers a remoteable [`dolang_vfs::VfsExtension`] providing
//! typed CRUD access to the Windows registry, dispatched identically
//! whether served in-process or over a real VFS RPC session. See
//! [`Key`] for the public entry point.
//!
//! The extension is registered on every platform. On non-Windows targets
//! every operation returns `Err` with [`dolang_vfs::ErrorKind::Unsupported`]
//! rather than the extension being absent, so a caller sees a clear,
//! catchable error instead of a routing failure indistinguishable from a
//! typo in the extension name/version.

mod api;
mod backend;
mod key;
mod value;
mod wire;

pub use api::Key;
pub use value::Value;
pub use wire::{Access, PredefinedRoot, View};
